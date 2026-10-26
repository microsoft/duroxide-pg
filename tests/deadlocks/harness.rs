// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers shared by the deadlock suite (`tests/deadlocks`) and the
//! orchestration lock renewal tests (`tests/orchestration_lock_renewal_tests.rs`).
//!
//! Every wait here is bounded by `STEP_TIMEOUT` or `WAIT_BUDGET`, so a
//! regression fails the test instead of hanging it.

#![allow(dead_code)]

use duroxide::providers::{Provider, ProviderError, WorkItem};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, PgPool};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

pub const ORCHESTRATION: &str = "FetchRenewLockOrder";
/// Upper bound for any single step, so a regression fails instead of hanging.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a backend to reach an expected lock wait.
pub const WAIT_BUDGET: Duration = Duration::from_secs(10);
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// SQLSTATE `deadlock_detected`.
pub const DEADLOCK_DETECTED: &str = "40P01";
/// The error the renewal function raises when its lease is gone.
pub const LEASE_GONE: &str = "Lock token invalid, expired, or already released";

pub fn database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("fetch_renew_lock_order_{suffix}")
}

pub fn advisory_key() -> i64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    i64::from_be_bytes(bytes[..8].try_into().expect("UUID prefix has eight bytes"))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

pub async fn raw_pool(database_url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("failed to create raw PostgreSQL pool")
}

pub async fn timed<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Runs `body` with a fresh schema name and drops the schema afterwards,
/// also when `body` panics.
pub async fn with_schema<F, Fut>(body: F)
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
pub async fn fetch_new_instance(
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

/// Renews through the provider, which retries transient errors.
pub fn spawn_renewal(
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

/// Sends the statement the provider sends to renew, once, on its own
/// connection. Unlike `spawn_renewal`, nothing retries a deadlock, so a test
/// sees SQLSTATE 40P01 directly.
pub fn spawn_sql_renewal(
    database_url: String,
    schema: String,
    token: String,
    extend_secs: i64,
) -> JoinHandle<Result<(), sqlx::Error>> {
    tokio::spawn(async move {
        let mut connection = PgConnection::connect(&database_url).await?;
        sqlx::query(&format!(
            "SELECT {schema}.renew_orchestration_item_lock($1, $2, $3)"
        ))
        .bind(token)
        .bind(now_ms())
        .bind(extend_secs)
        .execute(&mut connection)
        .await
        .map(|_| ())
    })
}

/// One history event in the JSON shape the provider sends to acknowledge. Its
/// data is never decoded by these tests.
pub fn opaque_history(event_id: i64) -> serde_json::Value {
    serde_json::json!([{
        "event_id": event_id,
        "event_type": "TestEvent",
        "event_data": "{}",
    }])
}

/// Sends the statement the provider sends to acknowledge, once, on
/// `connection`: `history` as the history delta, no new work and no metadata
/// change. Nothing retries, so a test sees any error directly.
pub async fn sql_ack(
    connection: &mut PgConnection,
    schema: &str,
    token: &str,
    history: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {schema}.ack_orchestration_item($1, $2, $3, $4, $5, $6, $7, $8)"
    ))
    .bind(token)
    .bind(now_ms())
    .bind(1_i64)
    .bind(history)
    .bind(serde_json::json!([]))
    .bind(serde_json::json!([]))
    .bind(serde_json::json!({
        "orchestration_name": null,
        "orchestration_version": null,
        "status": null,
        "output": null,
        "parent_instance_id": null,
        "pinned_duroxide_version": null,
        "custom_status_action": null,
        "custom_status_value": null,
        "kv_mutations": [],
    }))
    .bind(serde_json::json!([]))
    .execute(connection)
    .await
    .map(|_| ())
}

/// `sql_ack` on its own connection.
pub fn spawn_sql_ack(
    database_url: String,
    schema: String,
    token: String,
    history: serde_json::Value,
) -> JoinHandle<Result<(), sqlx::Error>> {
    tokio::spawn(async move {
        let mut connection = PgConnection::connect(&database_url).await?;
        sql_ack(&mut connection, &schema, &token, history).await
    })
}

/// Sends the statement the provider sends to fetch, once, on `connection`,
/// with the caller clock `now_ms`. Returns the instance and its new token.
pub async fn sql_fetch(
    connection: &mut PgConnection,
    schema: &str,
    now_ms: i64,
    lock_timeout_ms: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT out_instance_id, out_lock_token
           FROM {schema}.fetch_orchestration_item($1, $2, NULL, NULL)"
    ))
    .bind(now_ms)
    .bind(lock_timeout_ms)
    .fetch_optional(connection)
    .await
}

/// `sql_fetch` on its own connection.
pub fn spawn_sql_fetch(
    database_url: String,
    schema: String,
    now_ms: i64,
    lock_timeout_ms: i64,
) -> JoinHandle<Result<Option<(String, String)>, sqlx::Error>> {
    tokio::spawn(async move {
        let mut connection = PgConnection::connect(&database_url).await?;
        sql_fetch(&mut connection, &schema, now_ms, lock_timeout_ms).await
    })
}

/// Pauses every transaction that inserts a history row for `instance`, from
/// the first such row until the returned gate transaction ends. The paused
/// transaction holds the returned ready key, which `wait_for_advisory_holder`
/// detects. Acknowledge inserts history after its token check, so this
/// pauses an acknowledge in the middle.
pub async fn pause_history_inserts(
    pool: &PgPool,
    database_url: &str,
    schema: &str,
    instance: &str,
) -> (i64, sqlx::Transaction<'static, sqlx::Postgres>) {
    let ready_key = advisory_key();
    let gate_key = advisory_key();
    let gate_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("failed to connect gate session");
    let mut gate = gate_pool
        .begin()
        .await
        .expect("failed to begin gate transaction");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(gate_key)
        .execute(&mut *gate)
        .await
        .expect("failed to take gate advisory lock");
    install_pause_trigger_when(
        pool,
        schema,
        "history",
        "INSERT",
        &format!("NEW.instance_id = $instance${instance}$instance$"),
        ready_key,
        gate_key,
    )
    .await;
    (ready_key, gate)
}

/// Number of committed history rows of the instance.
pub async fn history_len(pool: &PgPool, schema: &str, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {schema}.history WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(pool)
    .await
    .expect("failed to count history rows")
}

/// Number of worker_queue rows of the instance.
pub async fn worker_queue_len(pool: &PgPool, schema: &str, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {schema}.worker_queue WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(pool)
    .await
    .expect("failed to count worker_queue rows")
}

/// The instance lease and the lease on each of the instance's queue rows.
#[derive(Debug)]
pub struct Lease {
    pub token: String,
    pub locked_until: i64,
    pub queue: Vec<(Option<String>, Option<i64>)>,
}

impl Lease {
    pub fn assert_queue_matches(&self) {
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

pub async fn read_lease(pool: &PgPool, schema: &str, instance: &str) -> Option<Lease> {
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
pub async fn read_queue(
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
pub async fn deadline_in(connection: &mut PgConnection, schema: &str, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT locked_until FROM {schema}.instance_locks WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(connection)
    .await
    .expect("failed to read the instance lease")
}

/// Whether any transaction holds a row lock on the instance's
/// `instance_locks` row. Takes and releases the lock with NOWAIT, so it never
/// waits.
pub async fn instance_lock_row_is_held(pool: &PgPool, schema: &str, instance: &str) -> bool {
    let probe = sqlx::query_scalar::<_, i32>(&format!(
        "SELECT 1 FROM {schema}.instance_locks WHERE instance_id = $1 FOR UPDATE NOWAIT"
    ))
    .bind(instance)
    .fetch_optional(pool)
    .await;
    match probe {
        Ok(Some(_)) => false,
        Ok(None) => panic!("the instance has no instance_locks row"),
        // lock_not_available
        Err(error) if sqlstate(&error).as_deref() == Some("55P03") => true,
        Err(error) => panic!("failed to probe the instance_locks row: {error}"),
    }
}

pub async fn database_now_ms(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT")
        .fetch_one(pool)
        .await
        .expect("failed to read database time")
}

pub async fn sleep_until_database_time(pool: &PgPool, target_ms: i64) {
    loop {
        let remaining = target_ms - database_now_ms(pool).await;
        if remaining < 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(remaining.min(200) as u64 + 1)).await;
    }
}

pub async fn backend_pid(connection: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(connection)
        .await
        .expect("failed to read backend PID")
}

/// Waits until a call to `schema.function` waits for a lock: an advisory
/// lock when `advisory` is true, a row lock otherwise.
pub async fn wait_for_call_to_wait(pool: &PgPool, schema: &str, function: &str, advisory: bool) {
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
pub async fn wait_for_advisory_holder(pool: &PgPool, key: i64) {
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
pub async fn wait_for_lock_waiter(pool: &PgPool, pid: i32) {
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

/// `token` as a dollar-quoted SQL literal. The provider generates tokens as
/// `lock_<uuid>`, so a token cannot contain the delimiter.
pub fn token_literal(token: &str) -> String {
    format!("$token${token}$token$")
}

/// Installs a trigger that, for rows carrying `token`, takes `ready_key` and
/// then waits for `gate_key`. The test holds `gate_key` to pause the session
/// that fires the trigger, and detects the pause through `ready_key`.
pub async fn install_pause_trigger(
    pool: &PgPool,
    schema: &str,
    table: &str,
    event: &str,
    token: &str,
    ready_key: i64,
    gate_key: i64,
) {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    let condition = format!("{row}.lock_token = {}", token_literal(token));
    install_pause_trigger_when(pool, schema, table, event, &condition, ready_key, gate_key).await;
}

/// Like `install_pause_trigger`, for rows that match the SQL `condition`,
/// which may use `OLD` and `NEW`.
pub async fn install_pause_trigger_when(
    pool: &PgPool,
    schema: &str,
    table: &str,
    event: &str,
    condition: &str,
    ready_key: i64,
    gate_key: i64,
) {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    sqlx::query(&format!(
        "CREATE FUNCTION {schema}.pause_for_test()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $function$
         BEGIN
             IF {condition} THEN
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

pub fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
        _ => None,
    }
}

/// Fails the test with SQLSTATE 40P01 in the message when `who` was aborted
/// as a deadlock victim. Call it before any other check of `result`, so a
/// deadlock is reported as a deadlock.
pub fn assert_not_deadlock_victim<T>(who: &str, result: &Result<T, sqlx::Error>) {
    if let Err(error) = result {
        assert_ne!(
            sqlstate(error).as_deref(),
            Some(DEADLOCK_DETECTED),
            "{who} was aborted as a deadlock victim (SQLSTATE {DEADLOCK_DETECTED}): {error}"
        );
    }
}

/// Fails the test unless `result` is the error renewal raises when its lease
/// is gone.
pub fn assert_lease_gone(result: Result<(), sqlx::Error>) {
    let error = result.expect_err("renewal must fail after its lease was released");
    assert!(
        error.to_string().contains(LEASE_GONE),
        "renewal must fail as released: {error}"
    );
}

pub fn spin_for(micros: u64) {
    let until = Instant::now() + Duration::from_micros(micros);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}
