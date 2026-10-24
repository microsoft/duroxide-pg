// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regression tests for orchestration lock renewal (migration 0026).
//!
//! Before migration 0026, fetch locked a queue row and then waited for
//! `instance_locks`, while renewal held `instance_locks` and waited for the
//! queue row, so the two could deadlock (SQLSTATE 40P01). Renewal now takes
//! fetch's per-instance advisory lock before any row lock.
//!
//! Renewal looks its token up (the check) before that wait and changes the
//! lease (the use) after it. The `toctou_*` tests hold the advisory lock,
//! let renewal pass its check and block, change the lease in that window in
//! one way each, and then check what renewal does. The next two tests check
//! that renewal and acknowledge lock rows in the same order, and that the
//! provider retries a renewal that PostgreSQL aborted as a deadlock victim.
//!
//! The `race_*` tests reproduce how the deadlock happens in production, with
//! no triggers or pauses: another node treats the owner's lease as expired
//! while the owner renews it, because its clock runs ahead or because the
//! renewal stalled on the way to the database. Each round sends the same two
//! statements `PostgresProvider` sends, on two connections, released together
//! with staggered start times.

use duroxide::providers::{ExecutionMetadata, Provider, ProviderError, WorkItem};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, PgPool};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Barrier};
use tokio::task::JoinHandle;

const ORCHESTRATION: &str = "FetchRenewLockOrder";
/// Upper bound for any single step, so a regression fails instead of hanging.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a backend to reach an expected lock wait.
const WAIT_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

fn database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("fetch_renew_lock_order_{suffix}")
}

fn advisory_key() -> i64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    i64::from_be_bytes(bytes[..8].try_into().expect("UUID prefix has eight bytes"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

async fn raw_pool(database_url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("failed to create raw PostgreSQL pool")
}

async fn timed<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Runs `body` with a fresh schema name and drops the schema afterwards,
/// also when `body` panics.
async fn with_schema<F, Fut>(body: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let schema = unique_schema_name();
    let outcome = tokio::spawn(body(schema.clone())).await;

    let pool = raw_pool(&database_url()).await;
    let drop_sql = format!("DROP SCHEMA IF EXISTS {schema} CASCADE");
    let drop = sqlx::query(&drop_sql).execute(&pool);
    match tokio::time::timeout(STEP_TIMEOUT, drop).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => eprintln!("failed to drop test schema {schema}: {error}"),
        Err(_) => eprintln!("timed out dropping test schema {schema}"),
    }

    if let Err(error) = outcome {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("test body did not finish: {error}");
    }
}

/// Creates a provider in `schema`, starts one orchestration instance and
/// fetches it. Returns the provider, the instance id and the lock token.
async fn fetch_new_instance(
    schema: &str,
    lock_timeout: Duration,
) -> (Arc<PostgresProvider>, String, String) {
    let provider = PostgresProvider::new_with_schema(&database_url(), Some(schema))
        .await
        .expect("failed to create provider");
    let instance = format!("instance-{}", uuid::Uuid::new_v4());

    provider
        .enqueue_for_orchestrator(
            WorkItem::StartOrchestration {
                instance: instance.clone(),
                orchestration: ORCHESTRATION.to_string(),
                input: "{}".to_string(),
                version: Some("1.0.0".to_string()),
                parent_instance: None,
                parent_id: None,
                parent_execution_id: None,
                execution_id: 1,
            },
            None,
        )
        .await
        .expect("failed to enqueue orchestration work");

    let (_item, token, _attempt_count) = provider
        .fetch_orchestration_item(lock_timeout, Duration::ZERO, None)
        .await
        .expect("initial fetch failed")
        .expect("initial fetch did not return work");

    (Arc::new(provider), instance, token)
}

fn spawn_renewal(
    provider: Arc<PostgresProvider>,
    token: String,
    extend_for: Duration,
) -> JoinHandle<Result<(), ProviderError>> {
    tokio::spawn(async move {
        provider
            .renew_orchestration_item_lock(&token, extend_for)
            .await
    })
}

/// The instance lease and the lease on each of the instance's queue rows.
#[derive(Debug)]
struct Lease {
    token: String,
    locked_until: i64,
    queue: Vec<(Option<String>, Option<i64>)>,
}

impl Lease {
    fn assert_queue_matches(&self) {
        assert!(!self.queue.is_empty(), "the instance has no queue rows");
        for row in &self.queue {
            assert_eq!(
                row,
                &(Some(self.token.clone()), Some(self.locked_until)),
                "a queue row's lease differs from the instance lease: {self:?}"
            );
        }
    }
}

async fn read_lease(pool: &PgPool, schema: &str, instance: &str) -> Option<Lease> {
    let row: Option<(String, i64)> = sqlx::query_as(&format!(
        "SELECT lock_token, locked_until FROM {schema}.instance_locks WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_optional(pool)
    .await
    .expect("failed to read instance lease");
    let queue = read_queue(pool, schema, instance).await;

    row.map(|(token, locked_until)| Lease {
        token,
        locked_until,
        queue,
    })
}

/// The lease on each of the instance's queue rows.
async fn read_queue(
    pool: &PgPool,
    schema: &str,
    instance: &str,
) -> Vec<(Option<String>, Option<i64>)> {
    sqlx::query_as(&format!(
        "SELECT lock_token, locked_until FROM {schema}.orchestrator_queue
          WHERE instance_id = $1 ORDER BY id"
    ))
    .bind(instance)
    .fetch_all(pool)
    .await
    .expect("failed to read queue leases")
}

/// The instance lease deadline as seen by `connection`, including changes it
/// has not committed.
async fn deadline_in(connection: &mut PgConnection, schema: &str, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT locked_until FROM {schema}.instance_locks WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(connection)
    .await
    .expect("failed to read the instance lease")
}

async fn database_now_ms(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT")
        .fetch_one(pool)
        .await
        .expect("failed to read database time")
}

async fn sleep_until_database_time(pool: &PgPool, target_ms: i64) {
    loop {
        let remaining = target_ms - database_now_ms(pool).await;
        if remaining < 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(remaining.min(200) as u64 + 1)).await;
    }
}

async fn backend_pid(connection: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(connection)
        .await
        .expect("failed to read backend PID")
}

/// Waits until a call to `schema.function` waits for a lock: an advisory
/// lock when `advisory` is true, a row lock otherwise.
async fn wait_for_call_to_wait(pool: &PgPool, schema: &str, function: &str, advisory: bool) {
    let pattern = format!("%{schema}.{function}(%");
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                  WHERE wait_event_type = 'Lock'
                    AND (wait_event = 'advisory') = $2
                    AND query LIKE $1)",
        )
        .bind(&pattern)
        .bind(advisory)
        .fetch_one(pool)
        .await
        .expect("failed to inspect lock waits");
        if waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{function} never waited for a lock (advisory: {advisory})"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Waits until another session holds the transaction advisory lock `key`.
async fn wait_for_advisory_holder(pool: &PgPool, key: i64) {
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        let held: bool = sqlx::query_scalar("SELECT NOT pg_try_advisory_xact_lock($1)")
            .bind(key)
            .fetch_one(pool)
            .await
            .expect("failed to probe advisory lock");
        if held {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no session took advisory lock {key}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Waits until backend `pid` waits for any lock.
async fn wait_for_lock_waiter(pool: &PgPool, pid: i32) {
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT COALESCE(wait_event_type = 'Lock', false)
               FROM pg_stat_activity
              WHERE pid = $1",
        )
        .bind(pid)
        .fetch_optional(pool)
        .await
        .expect("failed to inspect backend state")
        .unwrap_or(false);
        if waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "backend {pid} never waited for a lock"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Installs a trigger that, for rows carrying `token`, takes `ready_key` and
/// then waits for `gate_key`. The test holds `gate_key` to pause the session
/// that fires the trigger, and detects the pause through `ready_key`.
async fn install_pause_trigger(
    pool: &PgPool,
    schema: &str,
    table: &str,
    event: &str,
    token: &str,
    ready_key: i64,
    gate_key: i64,
) {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    // The provider generates tokens as `lock_<uuid>`, so a token cannot
    // contain this dollar-quote delimiter.
    sqlx::query(&format!(
        "CREATE FUNCTION {schema}.pause_for_test()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $function$
         BEGIN
             IF {row}.lock_token = $token${token}$token$ THEN
                 PERFORM pg_advisory_xact_lock({ready_key});
                 PERFORM pg_advisory_xact_lock({gate_key});
             END IF;
             RETURN {row};
         END;
         $function$"
    ))
    .execute(pool)
    .await
    .expect("failed to create pause function");
    sqlx::query(&format!(
        "CREATE TRIGGER pause_for_test
         AFTER {event} ON {schema}.{table}
         FOR EACH ROW EXECUTE FUNCTION {schema}.pause_for_test()"
    ))
    .execute(pool)
    .await
    .expect("failed to create pause trigger");
}

/// Renewal pauses inside its `instance_locks` update while a fetch for the
/// same instance starts. The fetch must wait, and both must then finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_and_renew_serialize_before_taking_conflicting_row_locks() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (_provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(30)).await;
        let pool = raw_pool(&database_url).await;

        let ready_key = advisory_key();
        let gate_key = advisory_key();
        let mut gate_connection = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect gate session");
        let mut gate = gate_connection
            .begin()
            .await
            .expect("failed to begin gate transaction");
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(gate_key)
            .execute(&mut *gate)
            .await
            .expect("failed to take gate advisory lock");
        install_pause_trigger(
            &pool,
            &schema,
            "instance_locks",
            "UPDATE OF locked_until",
            &token,
            ready_key,
            gate_key,
        )
        .await;

        let renew_now_ms = now_ms();
        let renewal_schema = schema.clone();
        let renewal_token = token.clone();
        let renewal_database_url = database_url.clone();
        let renewal = tokio::spawn(async move {
            let mut connection = PgConnection::connect(&renewal_database_url)
                .await
                .expect("failed to connect renewal session");
            sqlx::query(&format!(
                "SELECT {renewal_schema}.renew_orchestration_item_lock($1, $2, $3)"
            ))
            .bind(renewal_token)
            .bind(renew_now_ms)
            .bind(60_i64)
            .execute(&mut connection)
            .await
        });

        wait_for_advisory_holder(&pool, ready_key).await;

        // Fetch as if the renewed lease had expired, so it takes the instance.
        let fetch_schema = schema.clone();
        let fetch_database_url = database_url.clone();
        let (fetch_pid_tx, fetch_pid_rx) = oneshot::channel();
        let fetch = tokio::spawn(async move {
            let mut connection = PgConnection::connect(&fetch_database_url)
                .await
                .expect("failed to connect fetch session");
            let _ = fetch_pid_tx.send(backend_pid(&mut connection).await);
            sqlx::query_as::<_, (String, String)>(&format!(
                "SELECT out_instance_id, out_lock_token
                   FROM {fetch_schema}.fetch_orchestration_item($1, $2, NULL, NULL)"
            ))
            .bind(renew_now_ms + 120_000)
            .bind(30_000_i64)
            .fetch_optional(&mut connection)
            .await
        });

        let fetch_pid = timed("fetch backend PID", fetch_pid_rx)
            .await
            .expect("fetch task did not report its backend PID");
        wait_for_lock_waiter(&pool, fetch_pid).await;

        timed("gate release", gate.commit())
            .await
            .expect("failed to release renewal gate");
        let renewal_result = timed("renewal", renewal)
            .await
            .expect("renewal task panicked");
        let fetch_result = timed("fetch", fetch).await.expect("fetch task panicked");

        renewal_result.expect("renewal must complete without a deadlock");
        let (fetched, new_token) = fetch_result
            .expect("fetch must complete without a deadlock")
            .expect("fetch must take the instance after renewal commits");
        assert_eq!(fetched, instance);
        assert_ne!(new_token, token);

        let lease = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must stay locked by the fetch");
        assert_eq!(lease.token, new_token);
        lease.assert_queue_matches();
    })
    .await;
}

/// What changes between renewal's token lookup (the check) and its update
/// after the advisory-lock wait (the use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interleaving {
    /// Only time passes, more than the requested extension. The lease stays
    /// live.
    LeaseStaysLive,
    /// Time passes until the lease expires.
    LeaseExpires,
    /// A fetch takes the instance over with a new token.
    FetchTakesOver,
    /// The owner acknowledges the turn, which releases the lock.
    Acknowledged,
    /// The owner abandons the turn, which releases the lock.
    Abandoned,
    /// Another renewal with the same token moves the deadline later.
    OtherRenewalExtends,
}

/// What renewal must do once it gets the advisory lock.
enum Expected {
    /// Succeed and leave this owner and deadline in both tables.
    Renewed { token: String, locked_until: i64 },
    /// Fail and leave this lease unchanged in both tables.
    RejectedLeaseUnchanged { token: String, locked_until: i64 },
    /// Fail and leave no lock row. Queue rows stay deleted (acknowledge) or
    /// unlocked (abandon).
    RejectedReleased { queue_deleted: bool },
}

/// Opens the window between renewal's check and its use, applies one
/// interleaving inside it, and checks the result.
///
/// The test holds the advisory lock that fetch and renewal take for the
/// instance. Renewal takes that lock only after its token lookup passed, so a
/// renewal seen waiting on it is between its check and its use. The
/// interleaving runs while renewal waits: fetch and renewal inside the
/// transaction that holds the lock (PostgreSQL grants it again to the same
/// session), acknowledge and abandon on their own connections (they do not
/// take the advisory lock). Committing that transaction lets renewal continue.
async fn renew_across(interleaving: Interleaving) {
    with_schema(move |schema| async move {
        let database_url = database_url();
        let lease_timeout = if interleaving == Interleaving::LeaseExpires {
            Duration::from_secs(4)
        } else {
            Duration::from_secs(60)
        };
        // Shorter than the lease left after the wait, so a renewal that used
        // its pre-wait time would move the deadline earlier.
        let extend_for = Duration::from_secs(1);
        let (provider, instance, token) = fetch_new_instance(&schema, lease_timeout).await;
        let pool = raw_pool(&database_url).await;
        let before = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must be locked");

        let mut gate_connection = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect gate session");
        let mut gate = gate_connection
            .begin()
            .await
            .expect("failed to begin gate transaction");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(&instance)
            .execute(&mut *gate)
            .await
            .expect("failed to take the instance advisory lock");

        let renewal = spawn_renewal(provider.clone(), token.clone(), extend_for);
        wait_for_call_to_wait(&pool, &schema, "renew_orchestration_item_lock", true).await;
        let during = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must still be locked");
        assert_eq!(
            (during.token.as_str(), during.locked_until),
            (token.as_str(), before.locked_until),
            "renewal changed the lease before it got the advisory lock"
        );

        let expected = match interleaving {
            Interleaving::LeaseStaysLive => {
                let waited_from = database_now_ms(&pool).await;
                sleep_until_database_time(&pool, waited_from + 2_000).await;
                Expected::Renewed {
                    token: token.clone(),
                    locked_until: before.locked_until,
                }
            }
            Interleaving::LeaseExpires => {
                sleep_until_database_time(&pool, before.locked_until + 250).await;
                Expected::RejectedLeaseUnchanged {
                    token: token.clone(),
                    locked_until: before.locked_until,
                }
            }
            Interleaving::FetchTakesOver => {
                // Fetch as if the lease had expired.
                let (fetched, new_token): (String, String) = sqlx::query_as(&format!(
                    "SELECT out_instance_id, out_lock_token
                       FROM {schema}.fetch_orchestration_item($1, $2, NULL, NULL)"
                ))
                .bind(now_ms() + 120_000)
                .bind(30_000_i64)
                .fetch_one(&mut *gate)
                .await
                .expect("takeover fetch failed");
                assert_eq!(fetched, instance);
                assert_ne!(new_token, token);
                let locked_until = deadline_in(&mut gate, &schema, &instance).await;
                Expected::RejectedLeaseUnchanged {
                    token: new_token,
                    locked_until,
                }
            }
            Interleaving::Acknowledged => {
                timed(
                    "acknowledge",
                    provider.ack_orchestration_item(
                        &token,
                        1,
                        vec![],
                        vec![],
                        vec![],
                        ExecutionMetadata::default(),
                        vec![],
                    ),
                )
                .await
                .expect("acknowledge must succeed while renewal waits");
                Expected::RejectedReleased {
                    queue_deleted: true,
                }
            }
            Interleaving::Abandoned => {
                timed(
                    "abandon",
                    provider.abandon_orchestration_item(&token, None, false),
                )
                .await
                .expect("abandon must succeed while renewal waits");
                Expected::RejectedReleased {
                    queue_deleted: false,
                }
            }
            Interleaving::OtherRenewalExtends => {
                sqlx::query(&format!(
                    "SELECT {schema}.renew_orchestration_item_lock($1, $2, $3)"
                ))
                .bind(&token)
                .bind(now_ms())
                .bind(120_i64)
                .execute(&mut *gate)
                .await
                .expect("the other renewal failed");
                let locked_until = deadline_in(&mut gate, &schema, &instance).await;
                assert!(locked_until > before.locked_until);
                Expected::Renewed {
                    token: token.clone(),
                    locked_until,
                }
            }
        };

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the instance advisory lock");
        let result = timed("renewal", renewal)
            .await
            .expect("renewal task panicked");
        let after = read_lease(&pool, &schema, &instance).await;

        match expected {
            Expected::Renewed {
                token,
                locked_until,
            } => {
                result.expect("renewal of a live lease must succeed");
                let after = after.expect("the instance must stay locked");
                assert_eq!(after.token, token);
                assert_eq!(
                    after.locked_until, locked_until,
                    "renewal must not move the deadline earlier"
                );
                assert!(
                    after.locked_until > database_now_ms(&pool).await,
                    "the deadline must be in the future"
                );
                after.assert_queue_matches();
            }
            Expected::RejectedLeaseUnchanged {
                token,
                locked_until,
            } => {
                let error =
                    result.expect_err("renewal must fail when its lease changed while it waited");
                assert!(!error.is_retryable(), "unexpected renewal error: {error:?}");
                let after = after.expect("the lease row must remain");
                assert_eq!(after.token, token);
                assert_eq!(
                    after.locked_until, locked_until,
                    "renewal must not change a lease it no longer holds"
                );
                after.assert_queue_matches();
            }
            Expected::RejectedReleased { queue_deleted } => {
                let error = result.expect_err("renewal must fail after the lock was released");
                assert!(!error.is_retryable(), "unexpected renewal error: {error:?}");
                assert!(after.is_none(), "renewal must not recreate a released lock");
                let queue = read_queue(&pool, &schema, &instance).await;
                if queue_deleted {
                    assert!(
                        queue.is_empty(),
                        "acknowledged messages must stay deleted: {queue:?}"
                    );
                } else {
                    assert!(!queue.is_empty(), "abandoned messages must stay queued");
                    assert!(
                        queue.iter().all(|row| *row == (None, None)),
                        "renewal must not lock abandoned messages again: {queue:?}"
                    );
                }
            }
        }
    })
    .await;
}

/// More time than the extension passes, but the lease stays live. Renewal must
/// succeed without moving the deadline earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_lease_stays_live_while_renewal_waits() {
    renew_across(Interleaving::LeaseStaysLive).await;
}

/// The lease expires. Renewal must fail rather than revive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_lease_expires_while_renewal_waits() {
    renew_across(Interleaving::LeaseExpires).await;
}

/// A fetch takes the instance over. Renewal must fail and leave the new
/// owner's lease unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_fetch_takes_over_while_renewal_waits() {
    renew_across(Interleaving::FetchTakesOver).await;
}

/// The owner acknowledges. Renewal must fail and must not recreate the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_acknowledge_releases_while_renewal_waits() {
    renew_across(Interleaving::Acknowledged).await;
}

/// The owner abandons. Renewal must fail and leave the messages unlocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_abandon_releases_while_renewal_waits() {
    renew_across(Interleaving::Abandoned).await;
}

/// Another renewal moves the deadline later. This renewal must succeed and
/// keep the later deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_other_renewal_extends_while_renewal_waits() {
    renew_across(Interleaving::OtherRenewalExtends).await;
}

/// Acknowledge pauses after it deletes the instance's queue rows and before it
/// deletes the instance lock. Renewal must wait for the queue rows without
/// holding the instance lock; once acknowledge commits, renewal must fail
/// because the lock was released. Opposite row orders deadlock here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_and_acknowledge_lock_rows_in_the_same_order() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;

        let ready_key = advisory_key();
        let gate_key = advisory_key();
        let mut gate_connection = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect gate session");
        let mut gate = gate_connection
            .begin()
            .await
            .expect("failed to begin gate transaction");
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(gate_key)
            .execute(&mut *gate)
            .await
            .expect("failed to take gate advisory lock");
        install_pause_trigger(
            &pool,
            &schema,
            "orchestrator_queue",
            "DELETE",
            &token,
            ready_key,
            gate_key,
        )
        .await;

        let ack_provider = provider.clone();
        let ack_token = token.clone();
        let acknowledge = tokio::spawn(async move {
            ack_provider
                .ack_orchestration_item(
                    &ack_token,
                    1,
                    vec![],
                    vec![],
                    vec![],
                    ExecutionMetadata::default(),
                    vec![],
                )
                .await
        });
        wait_for_advisory_holder(&pool, ready_key).await;

        let renewal = spawn_renewal(provider.clone(), token.clone(), Duration::from_secs(60));
        wait_for_call_to_wait(&pool, &schema, "renew_orchestration_item_lock", false).await;

        timed("gate release", gate.commit())
            .await
            .expect("failed to release acknowledge");
        timed("acknowledge", acknowledge)
            .await
            .expect("acknowledge task panicked")
            .expect("acknowledge must succeed");
        let error = timed("renewal", renewal)
            .await
            .expect("renewal task panicked")
            .expect_err("renewal must fail after acknowledge released the lock");
        assert!(
            !error.is_retryable(),
            "renewal must fail as released, not as a deadlock victim: {error:?}"
        );

        assert!(read_lease(&pool, &schema, &instance).await.is_none());
    })
    .await;
}

/// Renewal waits for the instance's advisory lock. When fetches deadlock each
/// other on that lock, PostgreSQL can abort the waiting renewal instead
/// (SQLSTATE 40P01). The provider must retry it rather than return the error,
/// because the runtime stops renewing a turn after any renewal error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_retries_when_aborted_as_deadlock_victim() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;
        let before = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must be locked");

        // Abort the first renewal attempt as a deadlock victim. The sequence
        // is not rolled back with the aborted transaction, so the next
        // attempt passes.
        sqlx::query(&format!("CREATE SEQUENCE {schema}.renewal_attempts"))
            .execute(&pool)
            .await
            .expect("failed to create attempt counter");
        // The provider generates tokens as `lock_<uuid>`, so a token cannot
        // contain this dollar-quote delimiter.
        sqlx::query(&format!(
            "CREATE FUNCTION {schema}.fail_first_renewal()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $function$
             BEGIN
                 IF NEW.lock_token = $token${token}$token$
                    AND nextval('{schema}.renewal_attempts') = 1 THEN
                     RAISE EXCEPTION 'injected deadlock' USING ERRCODE = '40P01';
                 END IF;
                 RETURN NEW;
             END;
             $function$"
        ))
        .execute(&pool)
        .await
        .expect("failed to create failure function");
        sqlx::query(&format!(
            "CREATE TRIGGER fail_first_renewal
             BEFORE UPDATE OF locked_until ON {schema}.orchestrator_queue
             FOR EACH ROW EXECUTE FUNCTION {schema}.fail_first_renewal()"
        ))
        .execute(&pool)
        .await
        .expect("failed to create failure trigger");

        timed(
            "renewal",
            provider.renew_orchestration_item_lock(&token, Duration::from_secs(120)),
        )
        .await
        .expect("renewal must be retried after it was aborted as a deadlock victim");

        let attempts: i64 =
            sqlx::query_scalar(&format!("SELECT last_value FROM {schema}.renewal_attempts"))
                .fetch_one(&pool)
                .await
                .expect("failed to read attempt counter");
        assert!(
            attempts >= 2,
            "renewal was not retried: {attempts} attempt(s)"
        );

        let after = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must stay locked");
        assert_eq!(after.token, token);
        assert!(
            after.locked_until > before.locked_until,
            "the retried renewal must extend the lease"
        );
        after.assert_queue_matches();
    })
    .await;
}

/// How another node comes to treat the owner's lease as expired while the
/// owner renews it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpiredView {
    /// The other node's clock runs ahead of the owner's by more than the
    /// lease time left. With the runtime defaults (5 s lease, renewal every
    /// 3 s) that is a clock 2 s or more ahead. The owner renews on time.
    ClockAhead,
    /// The owner's renewal read its clock just before the lease expired and
    /// reached the database just after, for example behind a stalled
    /// connection pool. The other node's clock is correct.
    RenewalStalled,
}

#[derive(Debug, Default)]
struct RaceTally {
    rounds: u32,
    renew_deadlocks: u32,
    fetch_deadlocks: u32,
    renewed: u32,
    renewal_rejected: u32,
    taken_over: u32,
    not_taken: u32,
    tables_disagree: u32,
    other_errors: Vec<String>,
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
        _ => None,
    }
}

fn spin_for(micros: u64) {
    let until = Instant::now() + Duration::from_micros(micros);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

/// Runs `RACE_ROUNDS` rounds (default 200). In each round the owner fetches
/// a new instance, then its renewal and the other node's fetch are released
/// together. Their start times are staggered across rounds, from the fetch
/// 3 ms ahead to the renewal 0.5 ms ahead. Fetch does more work before it
/// takes its first row lock than renewal does, so it needs the longer lead.
async fn race_renewal_against_expired_view(view: ExpiredView) {
    let rounds: u32 = std::env::var("RACE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    with_schema(move |schema| async move {
        let database_url = database_url();
        let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("failed to create provider");
        let pool = raw_pool(&database_url).await;
        let mut owner = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect owner session");
        let mut other = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect other node's session");
        for connection in [&mut owner, &mut other] {
            sqlx::query("SET statement_timeout = '20s'")
                .execute(&mut *connection)
                .await
                .expect("failed to set statement timeout");
        }
        let lease = match view {
            ExpiredView::ClockAhead => Duration::from_secs(5),
            ExpiredView::RenewalStalled => Duration::from_millis(20),
        };
        let renew_sql = format!("SELECT {schema}.renew_orchestration_item_lock($1, $2, $3)");
        let fetch_sql = format!(
            "SELECT out_instance_id, out_lock_token
               FROM {schema}.fetch_orchestration_item($1, $2, NULL, NULL)"
        );

        let mut tally = RaceTally::default();
        for round in 0..rounds {
            let instance = format!("race-{}", uuid::Uuid::new_v4());
            provider
                .enqueue_for_orchestrator(
                    WorkItem::StartOrchestration {
                        instance: instance.clone(),
                        orchestration: ORCHESTRATION.to_string(),
                        input: "{}".to_string(),
                        version: Some("1.0.0".to_string()),
                        parent_instance: None,
                        parent_id: None,
                        parent_execution_id: None,
                        execution_id: 1,
                    },
                    None,
                )
                .await
                .expect("failed to enqueue orchestration work");
            let (_item, token, _attempt_count) = provider
                .fetch_orchestration_item(lease, Duration::ZERO, None)
                .await
                .expect("owner fetch failed")
                .expect("owner fetch did not return work");
            let locked_until = read_lease(&pool, &schema, &instance)
                .await
                .expect("the owner must hold the lease")
                .locked_until;

            // The clock each node passes to the database.
            let (renew_now, other_now) = match view {
                ExpiredView::ClockAhead => (now_ms(), locked_until + 2_000),
                ExpiredView::RenewalStalled => {
                    sleep_until_database_time(&pool, locked_until + 2).await;
                    (locked_until - 1, now_ms().max(locked_until + 2))
                }
            };

            let offset = (i64::from(round) * 97) % 3501 - 3000;
            let renew_delay = if offset < 0 { offset.unsigned_abs() } else { 0 };
            let fetch_delay = if offset > 0 { offset.unsigned_abs() } else { 0 };
            let barrier = Arc::new(Barrier::new(2));

            let renewal = tokio::spawn({
                let barrier = barrier.clone();
                let sql = renew_sql.clone();
                let token = token.clone();
                let mut connection = owner;
                async move {
                    barrier.wait().await;
                    spin_for(renew_delay);
                    let result = sqlx::query(&sql)
                        .bind(&token)
                        .bind(renew_now)
                        .bind(5_i64)
                        .execute(&mut connection)
                        .await
                        .map(|_| ());
                    (connection, result)
                }
            });
            let fetch = tokio::spawn({
                let barrier = barrier.clone();
                let sql = fetch_sql.clone();
                let mut connection = other;
                async move {
                    barrier.wait().await;
                    spin_for(fetch_delay);
                    let result = sqlx::query_as::<_, (String, String)>(&sql)
                        .bind(other_now)
                        .bind(30_000_i64)
                        .fetch_optional(&mut connection)
                        .await;
                    (connection, result)
                }
            });
            let (connection, renew_result) = timed("renewal", renewal)
                .await
                .expect("renewal task panicked");
            owner = connection;
            let (connection, fetch_result) =
                timed("fetch", fetch).await.expect("fetch task panicked");
            other = connection;

            tally.rounds += 1;
            match renew_result {
                Ok(()) => tally.renewed += 1,
                Err(e) if sqlstate(&e).as_deref() == Some("40P01") => tally.renew_deadlocks += 1,
                Err(e)
                    if e.to_string()
                        .contains("Lock token invalid, expired, or already released") =>
                {
                    tally.renewal_rejected += 1
                }
                Err(e) => tally.other_errors.push(format!("renewal: {e}")),
            }
            match fetch_result {
                Ok(Some((fetched, new_token))) => {
                    assert_eq!(fetched, instance);
                    assert_ne!(new_token, token);
                    tally.taken_over += 1;
                }
                Ok(None) => tally.not_taken += 1,
                Err(e) if sqlstate(&e).as_deref() == Some("40P01") => tally.fetch_deadlocks += 1,
                Err(e) => tally.other_errors.push(format!("fetch: {e}")),
            }

            match read_lease(&pool, &schema, &instance).await {
                Some(lease) => {
                    if lease.queue.is_empty()
                        || lease.queue.iter().any(|row| {
                            *row != (Some(lease.token.clone()), Some(lease.locked_until))
                        })
                    {
                        tally.tables_disagree += 1;
                    }
                }
                None => tally.tables_disagree += 1,
            }

            for table in ["orchestrator_queue", "instance_locks"] {
                sqlx::query(&format!(
                    "DELETE FROM {schema}.{table} WHERE instance_id = $1"
                ))
                .bind(&instance)
                .execute(&pool)
                .await
                .expect("failed to clean up the round");
            }
        }

        println!(
            "RACEJSON {}",
            serde_json::json!({
                "view": format!("{view:?}"),
                "rounds": tally.rounds,
                "renew_deadlocks": tally.renew_deadlocks,
                "fetch_deadlocks": tally.fetch_deadlocks,
                "renewed": tally.renewed,
                "renewal_rejected": tally.renewal_rejected,
                "taken_over": tally.taken_over,
                "not_taken": tally.not_taken,
                "tables_disagree": tally.tables_disagree,
                "other_errors": tally.other_errors.len(),
            })
        );

        assert_eq!(
            tally.renew_deadlocks + tally.fetch_deadlocks,
            0,
            "renewal and fetch deadlocked: {tally:?}"
        );
        assert!(
            tally.other_errors.is_empty(),
            "unexpected errors: {tally:?}"
        );
        assert_eq!(tally.tables_disagree, 0, "lease tables disagree: {tally:?}");
        assert_eq!(tally.renewed + tally.renewal_rejected, rounds, "{tally:?}");
        assert_eq!(tally.taken_over + tally.not_taken, rounds, "{tally:?}");
        if view == ExpiredView::RenewalStalled {
            assert_eq!(
                tally.renewed, 0,
                "a renewal that reached the database after the lease expired must fail: {tally:?}"
            );
            assert_eq!(tally.taken_over, rounds, "{tally:?}");
        }
    })
    .await;
}

/// Case 1, clock skew: the other node's clock runs ahead, so it fetches the
/// instance while the owner renews it on time. No deadlock is allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn race_renewal_against_fetch_from_node_with_clock_ahead() {
    race_renewal_against_expired_view(ExpiredView::ClockAhead).await;
}

/// Case 1, stalled renewal: the renewal reaches the database just after the
/// lease expired, while another node fetches the instance. No deadlock is
/// allowed, and the renewal must not revive the expired lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn race_stalled_renewal_against_fetch_after_expiry() {
    race_renewal_against_expired_view(ExpiredView::RenewalStalled).await;
}
