// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regression tests: `fetch_orchestration_item` must never hand out an
//! orchestration lock lease that is already expired.
//!
//! References: <https://github.com/microsoft/duroxide-pg/issues/21> and
//! <https://github.com/microsoft/duroxide/issues/46>.
//!
//! Before migration 0025 the procedure wrote
//! `locked_until = p_now_ms + p_lock_timeout_ms`, where `p_now_ms` was taken by
//! the caller before the call and before any wait. A wait longer than the lock
//! timeout produced a lease that had expired before the item was returned, so
//! every ack, including the poison ack that `max_attempts` relies on, failed
//! with `Invalid lock token` and the message was retried forever.

use duroxide::providers::{ExecutionMetadata, Provider, ProviderError, WorkItem};
use duroxide::{ErrorDetails, Event, EventKind, PoisonMessageType};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ORCHESTRATION: &str = "FetchLeaseRegression";
const VERSION: &str = "1.0.0";
/// The runtime default for `RuntimeOptions::max_attempts`.
const MAX_ATTEMPTS: u32 = 10;
/// Lock timeout used by the contention tests.
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a competing session holds the instance advisory lock. Longer than
/// `LOCK_TIMEOUT`, so a fetch that waits it out used to return an expired lease.
const HOLD: Duration = Duration::from_secs(4);

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("fetch_lease_test_{suffix}")
}

/// Instance ids are unique per test run: advisory-lock keys are database-wide,
/// so a key held by one test must never collide with another test.
fn unique_instance(prefix: &str) -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    format!("{prefix}-{}", &guid[..8])
}

fn rust_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as i64
}

async fn raw_pool(database_url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("Failed to connect raw pool")
}

async fn drop_schema(pool: &PgPool, schema: &str) {
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(pool)
        .await
        .expect("Failed to drop schema");
}

async fn enqueue_start(provider: &PostgresProvider, instance: &str) {
    provider
        .enqueue_for_orchestrator(
            WorkItem::StartOrchestration {
                instance: instance.to_string(),
                orchestration: ORCHESTRATION.to_string(),
                input: "{}".to_string(),
                version: Some(VERSION.to_string()),
                parent_instance: None,
                parent_id: None,
                parent_execution_id: None,
                execution_id: 1,
            },
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("failed to enqueue {instance}: {e:?}"));
}

/// Remaining lease time in milliseconds for `lock_token`, measured against the
/// database clock and against this process's clock. Negative means expired.
async fn lease_remaining_ms(pool: &PgPool, schema: &str, lock_token: &str) -> (i64, i64) {
    let (locked_until, db_now_ms): (i64, i64) = sqlx::query_as(&format!(
        "SELECT locked_until, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
           FROM {schema}.instance_locks
          WHERE lock_token = $1"
    ))
    .bind(lock_token)
    .fetch_one(pool)
    .await
    .expect("an instance_locks row must exist for the returned lock token");
    (locked_until - db_now_ms, locked_until - rust_now_ms())
}

/// Holds the advisory locks that a fetch of each instance takes, in one
/// transaction for `hold`.
fn hold_instance_locks(
    pool: PgPool,
    instances: Vec<String>,
    hold: Duration,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (taken_tx, taken_rx) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        let mut tx = pool.begin().await.expect("holder begin failed");
        for instance in &instances {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(instance)
                .execute(&mut *tx)
                .await
                .expect("holder advisory lock failed");
        }
        let _ = taken_tx.send(());
        tokio::time::sleep(hold).await;
        tx.rollback().await.expect("holder rollback failed");
    });
    (holder, taken_rx)
}

fn started_event(instance: &str) -> Event {
    Event::with_event_id(
        1,
        instance,
        1,
        None,
        EventKind::OrchestrationStarted {
            name: ORCHESTRATION.to_string(),
            version: VERSION.to_string(),
            input: "{}".to_string(),
            parent_instance: None,
            parent_id: None,
            parent_execution_id: None,
            carry_forward_events: None,
            initial_custom_status: None,
        },
    )
}

async fn ack_started(
    provider: &PostgresProvider,
    instance: &str,
    lock_token: &str,
) -> Result<(), ProviderError> {
    provider
        .ack_orchestration_item(
            lock_token,
            1,
            vec![started_event(instance)],
            vec![],
            vec![],
            ExecutionMetadata {
                orchestration_name: Some(ORCHESTRATION.to_string()),
                orchestration_version: Some(VERSION.to_string()),
                ..Default::default()
            },
            vec![],
        )
        .await
}

/// The ack the runtime commits when a message exceeds `max_attempts`
/// (`fail_orchestration_as_poison` in duroxide 0.1.30): the execution is
/// marked Failed with `ErrorDetails::Poison`, keyed on the fetched lock token.
async fn poison_ack(
    provider: &PostgresProvider,
    instance: &str,
    lock_token: &str,
    attempt_count: u32,
) -> Result<(), ProviderError> {
    let details = ErrorDetails::Poison {
        attempt_count,
        max_attempts: MAX_ATTEMPTS,
        message_type: PoisonMessageType::Orchestration {
            instance: instance.to_string(),
            execution_id: 1,
        },
        message: "[]".to_string(),
    };
    let failed = Event::with_event_id(
        2,
        instance,
        1,
        None,
        EventKind::OrchestrationFailed {
            details: details.clone(),
        },
    );
    provider
        .ack_orchestration_item(
            lock_token,
            1,
            vec![started_event(instance), failed],
            vec![],
            vec![],
            ExecutionMetadata {
                status: Some("Failed".to_string()),
                output: Some(details.display_message()),
                orchestration_name: Some(ORCHESTRATION.to_string()),
                orchestration_version: Some(VERSION.to_string()),
                ..Default::default()
            },
            vec![],
        )
        .await
}

async fn queued_messages(pool: &PgPool, schema: &str, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {schema}.orchestrator_queue WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(pool)
    .await
    .expect("queue count failed")
}

/// Issue #46 reproduction A: a caller timestamp that is already 10 s stale
/// when the procedure runs (what a 10 s pool or advisory-lock wait produces).
/// The lease must still be live when it is returned, and the ack must commit.
/// Both procedure branches (with and without the version filter) are covered.
#[tokio::test]
async fn stale_caller_timestamp_never_yields_an_expired_lease() {
    let schema = unique_schema_name();
    let database_url = get_database_url();
    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Failed to create provider");
    let pool = raw_pool(&database_url).await;

    const STALE_MS: i64 = 10_000;
    const TIMEOUT_MS: i64 = 5_000;

    for version_filter in [None, Some((0_i64, 999_999_999_i64))] {
        let instance = unique_instance("stale");
        enqueue_start(&provider, &instance).await;
        // The message must be visible to the stale clock as well; otherwise the
        // procedure finds no candidate and the defect is not exercised.
        sqlx::query(&format!(
            "UPDATE {schema}.orchestrator_queue
                SET visible_at = visible_at - INTERVAL '60 seconds'
              WHERE instance_id = $1"
        ))
        .bind(&instance)
        .execute(&pool)
        .await
        .expect("backdate failed");

        let (min_packed, max_packed) = match version_filter {
            Some((min, max)) => (Some(min), Some(max)),
            None => (None, None),
        };
        let row: Option<(String, String, i32)> = sqlx::query_as(&format!(
            "SELECT out_instance_id, out_lock_token, out_attempt_count
               FROM {schema}.fetch_orchestration_item(
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT - $1,
                    $2, $3, $4)"
        ))
        .bind(STALE_MS)
        .bind(TIMEOUT_MS)
        .bind(min_packed)
        .bind(max_packed)
        .fetch_optional(&pool)
        .await
        .expect("fetch_orchestration_item failed");
        let (fetched, lock_token, attempt_count) =
            row.expect("the backdated message must be fetched");
        assert_eq!(fetched, instance);
        assert_eq!(attempt_count, 1);

        let (db_remaining, rust_remaining) = lease_remaining_ms(&pool, &schema, &lock_token).await;
        println!(
            "filter={version_filter:?}: lease remaining db={db_remaining}ms rust={rust_remaining}ms"
        );
        assert!(
            db_remaining > TIMEOUT_MS - 2_000,
            "lease returned with {db_remaining}ms left by the database clock \
             (lock timeout {TIMEOUT_MS}ms); it was computed from the stale caller timestamp"
        );
        assert!(
            rust_remaining > 0,
            "lease returned already expired by {}ms by the caller clock",
            -rust_remaining
        );

        let queue_lease: i64 = sqlx::query_scalar(&format!(
            "SELECT MIN(locked_until) FROM {schema}.orchestrator_queue WHERE lock_token = $1"
        ))
        .bind(&lock_token)
        .fetch_one(&pool)
        .await
        .expect("queue lease read failed");
        let lock_lease: i64 = sqlx::query_scalar(&format!(
            "SELECT locked_until FROM {schema}.instance_locks WHERE lock_token = $1"
        ))
        .bind(&lock_token)
        .fetch_one(&pool)
        .await
        .expect("instance lease read failed");
        assert_eq!(
            queue_lease, lock_lease,
            "queue rows and instance lock carry the same lease"
        );

        ack_started(&provider, &instance, &lock_token)
            .await
            .unwrap_or_else(|e| panic!("ack with the returned lease failed: {e}"));
        assert_eq!(queued_messages(&pool, &schema, &instance).await, 0);
    }

    drop_schema(&pool, &schema).await;
}

/// Issue #46 reproduction B, made deterministic: one hot instance whose
/// message has reached `max_attempts`, a session that holds the instance for
/// longer than the lock timeout, and 16 concurrent dispatchers polling it.
/// The dispatchers still wait for the holder, but every lease handed out must
/// be live, and the poison ack must commit so the message leaves the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hot_instance_contention_never_yields_an_expired_lease_and_poison_commits() {
    const DISPATCHERS: usize = 16;

    let schema = unique_schema_name();
    let database_url = get_database_url();
    let provider = Arc::new(
        PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("Failed to create provider"),
    );
    let pool = raw_pool(&database_url).await;

    let hot = unique_instance("hot");
    enqueue_start(&provider, &hot).await;
    // The message has already been attempted max_attempts times; the next
    // fetch returns attempt_count = max_attempts + 1 and must be poisoned.
    sqlx::query(&format!(
        "UPDATE {schema}.orchestrator_queue SET attempt_count = $2 WHERE instance_id = $1"
    ))
    .bind(&hot)
    .bind(MAX_ATTEMPTS as i32)
    .execute(&pool)
    .await
    .expect("attempt_count setup failed");

    let (holder, taken) = hold_instance_locks(pool.clone(), vec![hot.clone()], HOLD);
    taken.await.expect("holder did not take the locks");

    let done = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + HOLD + Duration::from_secs(6);
    let mut dispatchers = Vec::new();
    for _ in 0..DISPATCHERS {
        let provider = provider.clone();
        let pool = pool.clone();
        let schema = schema.clone();
        let done = done.clone();
        dispatchers.push(tokio::spawn(async move {
            let mut slowest = Duration::ZERO;
            let mut claims = Vec::new();
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let started = Instant::now();
                let fetched = provider
                    .fetch_orchestration_item(LOCK_TIMEOUT, Duration::ZERO, None)
                    .await
                    .expect("fetch_orchestration_item failed");
                slowest = slowest.max(started.elapsed());
                match fetched {
                    Some((item, lock_token, attempt_count)) => {
                        let remaining = lease_remaining_ms(&pool, &schema, &lock_token).await;
                        let acked = if attempt_count > MAX_ATTEMPTS {
                            poison_ack(&provider, &item.instance, &lock_token, attempt_count).await
                        } else {
                            ack_started(&provider, &item.instance, &lock_token).await
                        };
                        if acked.is_ok() {
                            done.store(true, Ordering::SeqCst);
                        }
                        claims.push((item.instance, attempt_count, remaining, acked));
                    }
                    None => tokio::time::sleep(Duration::from_millis(25)).await,
                }
            }
            (slowest, claims)
        }));
    }

    let mut slowest = Duration::ZERO;
    let mut claims = Vec::new();
    for dispatcher in dispatchers {
        let (task_slowest, task_claims) = dispatcher.await.expect("dispatcher panicked");
        slowest = slowest.max(task_slowest);
        claims.extend(task_claims);
    }
    holder.await.expect("holder panicked");

    println!("slowest fetch {slowest:?}; claims:");
    for (instance, attempt_count, remaining, acked) in &claims {
        println!(
            "  {instance} attempt={attempt_count} lease_remaining(db,rust)={remaining:?} ack={acked:?}"
        );
    }

    for (instance, _, (db_remaining, rust_remaining), _) in &claims {
        assert!(
            *db_remaining > 0 && *rust_remaining > 0,
            "{instance} was returned with an already-expired lease \
             (remaining db={db_remaining}ms rust={rust_remaining}ms)"
        );
    }
    assert_eq!(
        claims.len(),
        1,
        "exactly one dispatcher must claim the hot instance: {claims:?}"
    );
    let (instance, attempt_count, _, acked) = &claims[0];
    assert_eq!(instance, &hot);
    assert_eq!(*attempt_count, MAX_ATTEMPTS + 1);
    if let Err(e) = acked {
        panic!("the poison ack failed: {e}");
    }

    assert_eq!(
        queued_messages(&pool, &schema, &hot).await,
        0,
        "the poisoned message must leave the queue"
    );
    let history = provider.read(&hot).await.expect("read history failed");
    assert!(
        history.iter().any(|e| matches!(
            &e.kind,
            EventKind::OrchestrationFailed {
                details: ErrorDetails::Poison { .. }
            }
        )),
        "history must record the poison failure: {history:?}"
    );

    drop_schema(&pool, &schema).await;
}
