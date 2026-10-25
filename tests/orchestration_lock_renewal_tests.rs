// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Orchestration lock renewal across its advisory-lock wait (migration 0026).
//!
//! Renewal looks its token up (the check) before it waits for fetch's
//! per-instance advisory lock, and changes the lease (the use) after that
//! wait. The `toctou_*` tests hold the advisory lock, let renewal pass its
//! check and block, change the lease in that window in one way each, and then
//! check what renewal does.
//!
//! The last test checks the unique index on `instance_locks(lock_token)` that
//! renewal, acknowledge and abandon use to find an instance by its token.
//!
//! The deadlock tests for renewal live in `tests/deadlocks`, whose helpers
//! this file shares.

#[path = "deadlocks/harness.rs"]
mod harness;

use duroxide::providers::Provider;
use duroxide_pg::PostgresProvider;
use harness::*;
use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::time::Duration;

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
/// interleaving runs while renewal waits: fetch, acknowledge and renewal
/// inside the transaction that holds the lock (they take it too, and
/// PostgreSQL grants it again to the same session), abandon on its own
/// connection (it does not take the advisory lock). Committing that
/// transaction lets renewal continue.
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
                    sql_ack(&mut *gate, &schema, &token, serde_json::json!([])),
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

/// Renewal, acknowledge and abandon find an instance by its lock token.
/// Migration 0026 adds a unique index on `instance_locks(lock_token)` so these
/// lookups do not scan the table, and so a token can name only one instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_token_lookups_use_a_unique_index() {
    with_schema(|schema| async move {
        let database_url = database_url();
        let _provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("failed to create provider");
        let pool = raw_pool(&database_url).await;

        let (unique, column): (bool, String) = sqlx::query_as(
            "SELECT ix.indisunique, pg_get_indexdef(ix.indexrelid, 1, true)
               FROM pg_index ix
               JOIN pg_class i ON i.oid = ix.indexrelid
               JOIN pg_namespace n ON n.oid = i.relnamespace
              WHERE n.nspname = $1
                AND i.relname = 'idx_instance_locks_lock_token'
                AND ix.indnatts = 1",
        )
        .bind(&schema)
        .fetch_one(&pool)
        .await
        .expect("idx_instance_locks_lock_token must exist");
        assert!(unique, "the lock_token index must be unique");
        assert_eq!(column, "lock_token");

        // Enough live leases that the planner prefers an index to a table scan.
        sqlx::query(&format!(
            "INSERT INTO {schema}.instance_locks (instance_id, lock_token, locked_until, locked_at)
             SELECT 'instance-' || g, 'lock_' || gen_random_uuid(),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT + 600000, 0
               FROM generate_series(1, 5000) AS g"
        ))
        .execute(&pool)
        .await
        .expect("failed to fill instance_locks");
        sqlx::query(&format!("ANALYZE {schema}.instance_locks"))
            .execute(&pool)
            .await
            .expect("failed to analyze instance_locks");

        // The lookups of abandon, and of renewal and acknowledge.
        for predicate in [
            "lock_token = 'lock_unknown'",
            "lock_token = 'lock_unknown'
               AND locked_until > (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT",
        ] {
            let plan: Vec<String> = sqlx::query_scalar(&format!(
                "EXPLAIN SELECT instance_id FROM {schema}.instance_locks WHERE {predicate}"
            ))
            .fetch_all(&pool)
            .await
            .expect("failed to explain the token lookup");
            let plan = plan.join("\n");
            assert!(
                plan.contains("idx_instance_locks_lock_token") && !plan.contains("Seq Scan"),
                "the token lookup must use the index:\n{plan}"
            );
        }

        let duplicate = sqlx::query(&format!(
            "INSERT INTO {schema}.instance_locks (instance_id, lock_token, locked_until, locked_at)
             SELECT 'instance-duplicate', lock_token, 0, 0
               FROM {schema}.instance_locks
              LIMIT 1"
        ))
        .execute(&pool)
        .await
        .expect_err("a lock token must not name two instances");
        assert_eq!(sqlstate(&duplicate).as_deref(), Some("23505"));
    })
    .await;
}
