// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Orchestration lock renewal against fetch (migration 0026).
//!
//! Before migration 0026, fetch locked a queue row and then waited for
//! `instance_locks`, while renewal held `instance_locks` and waited for the
//! queue row, so the two could deadlock (SQLSTATE 40P01). Renewal now takes
//! fetch's per-instance advisory lock before any row lock.
//!
//! The first test forces that interleaving with a trigger. The `race_*` tests
//! reproduce how the deadlock happens in production, with no triggers or
//! pauses: another node treats the owner's lease as expired while the owner
//! renews it, because its clock runs ahead or because the renewal stalled on
//! the way to the database. Each round sends the same two statements
//! `PostgresProvider` sends, on two connections, released together with
//! staggered start times.

use crate::harness::*;
use duroxide::providers::{Provider, WorkItem};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Barrier};

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

        assert_not_deadlock_victim("renewal", &renewal_result);
        assert_not_deadlock_victim("fetch", &fetch_result);
        renewal_result.expect("renewal must complete");
        let (fetched, new_token) = fetch_result
            .expect("fetch must complete")
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
                Err(e) if sqlstate(&e).as_deref() == Some(DEADLOCK_DETECTED) => {
                    tally.renew_deadlocks += 1
                }
                Err(e) if e.to_string().contains(LEASE_GONE) => tally.renewal_rejected += 1,
                Err(e) => tally.other_errors.push(format!("renewal: {e}")),
            }
            match fetch_result {
                Ok(Some((fetched, new_token))) => {
                    assert_eq!(fetched, instance);
                    assert_ne!(new_token, token);
                    tally.taken_over += 1;
                }
                Ok(None) => tally.not_taken += 1,
                Err(e) if sqlstate(&e).as_deref() == Some(DEADLOCK_DETECTED) => {
                    tally.fetch_deadlocks += 1
                }
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
            "renewal and fetch deadlocked (SQLSTATE {DEADLOCK_DETECTED}): {tally:?}"
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
