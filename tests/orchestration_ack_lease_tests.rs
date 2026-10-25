// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Orchestration acknowledge against losing its lease (migration 0026).
//!
//! Acknowledge checks its lock token, writes the turn (history, new work),
//! and finally deletes the messages it processed and the instance lock, both
//! by token. Fetch takes an instance over when, by its caller's clock, the
//! lease has expired: a worker whose clock runs ahead, or an acknowledge that
//! stalled. Before migration 0026, nothing stopped such a fetch between the
//! token check and the commit. The acknowledge then committed the turn while
//! its deletes matched no row, so the processed messages were delivered
//! again. Duroxide does not de-duplicate redelivered queue messages or
//! external events, so the orchestration processed them twice.
//!
//! Acknowledge now takes fetch's advisory lock after its token check, checks
//! the token again, and fails if its instance lock is gone when it deletes it.
//! These tests change the lease at each point of an acknowledge and check
//! that the turn commits exactly once. The fetch against an acknowledge in
//! progress is in `tests/deadlocks/ack_fetch.rs`.

#[path = "deadlocks/harness.rs"]
mod harness;

use duroxide::providers::Provider;
use duroxide::runtime::registry::ActivityRegistry;
use duroxide::runtime::{self, OrchestrationStatus, RuntimeOptions};
use duroxide::{ActivityContext, Client, EventKind, OrchestrationContext, OrchestrationRegistry};
use duroxide_pg::PostgresProvider;
use harness::*;
use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A fetch from a worker whose clock runs 120 s ahead takes the instance over
/// while the acknowledge waits for the advisory lock, that is, after its
/// first token check. The acknowledge must fail and commit nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn toctou_fetch_takes_over_while_acknowledge_waits() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (_provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;

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

        let ack = spawn_sql_ack(
            database_url.clone(),
            schema.clone(),
            token.clone(),
            opaque_history(1),
        );
        wait_for_call_to_wait(&pool, &schema, "ack_orchestration_item", true).await;
        assert_eq!(
            history_len(&pool, &schema, &instance).await,
            0,
            "acknowledge wrote history before it got the advisory lock"
        );

        // The takeover runs in the transaction that holds the advisory lock;
        // PostgreSQL grants it again to the same session.
        let (fetched, new_token) = timed(
            "takeover fetch",
            sql_fetch(&mut gate, &schema, now_ms() + 120_000, 30_000),
        )
        .await
        .expect("takeover fetch failed")
        .expect("takeover fetch must take the instance");
        assert_eq!(fetched, instance);
        assert_ne!(new_token, token);

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the instance advisory lock");
        let result = timed("acknowledge", ack)
            .await
            .expect("acknowledge task panicked");

        assert_not_deadlock_victim("acknowledge", &result);
        let error = result.expect_err("acknowledge must fail once its lease was taken over");
        assert!(
            error.to_string().contains("Invalid lock token"),
            "acknowledge must fail on its token: {error}"
        );
        assert_eq!(
            history_len(&pool, &schema, &instance).await,
            0,
            "a failed acknowledge must not commit history"
        );
        let lease = read_lease(&pool, &schema, &instance)
            .await
            .expect("the new owner must hold the lock");
        assert_eq!(lease.token, new_token);
        lease.assert_queue_matches();
    })
    .await;
}

/// The instance lock disappears while the acknowledge writes the turn (as
/// when an administrator deletes the instance). The acknowledge must fail and
/// roll back the turn, instead of committing a turn that no longer owns the
/// instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledge_rolls_back_when_its_lock_disappears() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (_provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;

        let (ready_key, gate) =
            pause_history_inserts(&pool, &database_url, &schema, &instance).await;
        let ack = spawn_sql_ack(
            database_url.clone(),
            schema.clone(),
            token.clone(),
            opaque_history(1),
        );
        wait_for_advisory_holder(&pool, ready_key).await;

        let deleted = sqlx::query(&format!(
            "DELETE FROM {schema}.instance_locks WHERE instance_id = $1"
        ))
        .bind(&instance)
        .execute(&pool)
        .await
        .expect("failed to delete the instance lock")
        .rows_affected();
        assert_eq!(deleted, 1);

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the paused acknowledge");
        let result = timed("acknowledge", ack)
            .await
            .expect("acknowledge task panicked");

        assert_not_deadlock_victim("acknowledge", &result);
        let error = result.expect_err("acknowledge must fail when its lock is gone");
        assert!(
            error.to_string().contains("Invalid lock token"),
            "acknowledge must fail on its token: {error}"
        );
        assert_eq!(
            history_len(&pool, &schema, &instance).await,
            0,
            "the turn must roll back"
        );
        let queue = read_queue(&pool, &schema, &instance).await;
        assert!(!queue.is_empty(), "the processed messages must stay queued");
        assert!(
            queue
                .iter()
                .all(|(row_token, _)| row_token.as_deref() == Some(token.as_str())),
            "the processed messages must not be deleted: {queue:?}"
        );
    })
    .await;
}

/// Control: the takeover commits before the acknowledge starts. The token
/// check fails, as it does without migration 0026.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledge_fails_after_a_takeover() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let (_provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;

        let mut other = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect the other worker");
        let (fetched, new_token) = sql_fetch(&mut other, &schema, now_ms() + 120_000, 30_000)
            .await
            .expect("takeover fetch failed")
            .expect("takeover fetch must take the instance");
        assert_eq!(fetched, instance);

        let result = timed(
            "acknowledge",
            spawn_sql_ack(
                database_url.clone(),
                schema.clone(),
                token.clone(),
                opaque_history(1),
            ),
        )
        .await
        .expect("acknowledge task panicked");
        let error = result.expect_err("acknowledge must fail after a takeover");
        assert!(
            error.to_string().contains("Invalid lock token"),
            "acknowledge must fail on its token: {error}"
        );
        assert_eq!(history_len(&pool, &schema, &instance).await, 0);
        let lease = read_lease(&pool, &schema, &instance)
            .await
            .expect("the new owner must hold the lock");
        assert_eq!(lease.token, new_token);
        lease.assert_queue_matches();
    })
    .await;
}

const ORDERS: &str = "LostLeaseOrders";
const QUEUE: &str = "orders";

/// End to end with the duroxide runtime. The orchestration takes two orders
/// from a queue and charges each. Order 1 is enqueued once. While the runtime
/// acknowledges the turn that processed it, a worker whose clock runs 5 s
/// ahead tries to fetch the instance. That worker must wait, and order 1 must
/// be charged exactly once. Without migration 0026 the other worker takes the
/// instance over, the acknowledge still commits, and order 1 is delivered and
/// charged a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_charges_an_order_once_when_another_worker_fetches_during_the_ack() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let pool = raw_pool(&database_url).await;
        let provider: Arc<dyn Provider> = Arc::new(
            PostgresProvider::new_with_schema(&database_url, Some(&schema))
                .await
                .expect("failed to create provider"),
        );

        let charges: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let charged = charges.clone();
        let activities = ActivityRegistry::builder()
            .register("Charge", move |_ctx: ActivityContext, order: String| {
                let charged = charged.clone();
                async move {
                    charged.lock().expect("charges lock").push(order.clone());
                    Ok(format!("charged:{order}"))
                }
            })
            .build();
        let orchestrations = OrchestrationRegistry::builder()
            .register(
                ORDERS,
                |ctx: OrchestrationContext, _input: String| async move {
                    let mut charged = Vec::new();
                    for _ in 0..2 {
                        let order = ctx.dequeue_event(QUEUE).await;
                        charged.push(ctx.schedule_activity("Charge", order).await?);
                    }
                    Ok(charged.join(","))
                },
            )
            .build();
        let options = RuntimeOptions {
            dispatcher_min_poll_interval: Duration::from_millis(10),
            orchestrator_lock_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let rt = runtime::Runtime::start_with_options(
            provider.clone(),
            activities,
            orchestrations,
            options,
        )
        .await;
        let client = Client::new(provider.clone());
        let instance = format!("orders-{}", uuid::Uuid::new_v4());
        client
            .start_orchestration(&instance, ORDERS, "")
            .await
            .expect("failed to start the orchestration");
        wait_for_history(
            &*provider,
            &instance,
            "waiting for the first order",
            |kind| matches!(kind, EventKind::QueueSubscribed { .. }),
        )
        .await;

        let (ready_key, gate) =
            pause_history_inserts(&pool, &database_url, &schema, &instance).await;
        client
            .enqueue_event(&instance, QUEUE, "order-1")
            .await
            .expect("failed to enqueue order 1");
        wait_for_advisory_holder(&pool, ready_key).await;
        let owner = read_lease(&pool, &schema, &instance)
            .await
            .expect("the runtime must hold the lease while it acknowledges");

        let mut other = PgConnection::connect(&database_url)
            .await
            .expect("failed to connect the other worker");
        let other_pid = backend_pid(&mut other).await;
        let other_schema = schema.clone();
        let other_fetch = tokio::spawn(async move {
            sql_fetch(&mut other, &other_schema, now_ms() + 5_000, 2_000).await
        });
        wait_for_lock_waiter(&pool, other_pid).await;
        assert_eq!(
            read_lease(&pool, &schema, &instance)
                .await
                .map(|lease| lease.token),
            Some(owner.token.clone()),
            "the other worker took the instance over during the acknowledge"
        );

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the paused acknowledge");
        let other_result = timed("other worker's fetch", other_fetch)
            .await
            .expect("other worker's fetch panicked");
        assert_not_deadlock_victim("other worker's fetch", &other_result);
        other_result.expect("the other worker's fetch must succeed");

        wait_for_history(
            &*provider,
            &instance,
            "the acknowledge of order 1",
            |kind| matches!(kind, EventKind::ActivityScheduled { .. }),
        )
        .await;
        let redelivered: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {schema}.orchestrator_queue
              WHERE instance_id = $1 AND work_item LIKE '%QueueMessage%'"
        ))
        .bind(&instance)
        .fetch_one(&pool)
        .await
        .expect("failed to count queue messages");
        assert_eq!(redelivered, 0, "order 1 must leave the queue with its turn");

        client
            .enqueue_event(&instance, QUEUE, "order-2")
            .await
            .expect("failed to enqueue order 2");
        let status = client
            .wait_for_orchestration(&instance, Duration::from_secs(45))
            .await;
        let charged = charges.lock().expect("charges lock").clone();
        rt.shutdown(Some(5_000)).await;

        match status {
            Ok(OrchestrationStatus::Completed { output, .. }) => {
                assert_eq!(output, "charged:order-1,charged:order-2");
            }
            other => panic!("the orchestration did not complete: {other:?}"),
        }
        assert_eq!(charged, vec!["order-1".to_string(), "order-2".to_string()]);
    })
    .await;
}

/// Waits until the instance's history has an event that `matches`.
async fn wait_for_history(
    provider: &dyn Provider,
    instance: &str,
    what: &str,
    matches: impl Fn(&EventKind) -> bool,
) {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let history = provider.read(instance).await.unwrap_or_default();
        if history.iter().any(|event| matches(&event.kind)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
