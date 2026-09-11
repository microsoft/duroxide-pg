// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regression test for the expired orchestration lock lease bug.
//!
//! Reference: <https://github.com/microsoft/duroxide-pg/issues/21>
//!
//! # Summary
//!
//! `fetch_orchestration_item` selected its candidate deterministically
//! (`ORDER BY visible_at, id LIMIT 1`) and then took a *blocking*
//! `pg_advisory_xact_lock` on that instance. Two consequences followed:
//!
//! 1. Every dispatcher converged on the same key and queued behind it, even
//!    when other instances were free to run (head-of-line blocking).
//! 2. The lock lease was computed from `p_now_ms` — a timestamp minted by the
//!    Rust caller *before* the call — which was never refreshed. When the
//!    advisory wait exceeded the lock timeout, `locked_until` was written
//!    already in the past, so the item was handed back with a dead lease and
//!    every subsequent ack failed with `Invalid lock token`. The message was
//!    then never poisoned, never backed off, and retried forever.
//!
//! Migration 0024 makes the advisory acquisition non-blocking
//! (`pg_try_advisory_xact_lock`, skipping contended instances) and derives the
//! lease from the caller epoch advanced by time actually elapsed inside the
//! call.
//!
//! # What this test does
//!
//! A competing session holds the instance advisory lock for `HOLD` seconds
//! while a fetch runs with a lock timeout *shorter* than that hold. Against the
//! pre-0024 procedure the fetch blocks for the full hold and then returns the
//! contended instance with a negative remaining lease. Measured on
//! PostgreSQL 15.19: blocked 2.64s, lease -1658ms.

use duroxide::providers::{Provider, WorkItem};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("expired_lease_test_{suffix}")
}

/// The contended instance. Enqueued first, so it is the head-of-queue candidate
/// that every dispatcher selects.
const HOT: &str = "expired-lease-hot";
/// A second, entirely uncontended instance that is always available to run.
const OTHER: &str = "expired-lease-other";

/// How long the competing session holds the advisory lock.
const HOLD: Duration = Duration::from_secs(3);
/// Lock timeout for the fetch under test. Deliberately shorter than `HOLD`:
/// this is what makes a pre-refresh lease expire before it is returned.
const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
/// A fetch that does not convoy returns far below this; one that waits out the
/// competing session takes at least `HOLD`.
const MAX_ACCEPTABLE_FETCH: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_orchestration_item_never_returns_an_expired_lease() {
    let schema = unique_schema_name();
    let database_url = get_database_url();

    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Failed to create provider");

    for (index, instance) in [HOT, OTHER].iter().enumerate() {
        provider
            .enqueue_for_orchestrator(
                WorkItem::StartOrchestration {
                    instance: (*instance).to_string(),
                    orchestration: "ExpiredLeaseRegression".to_string(),
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
            .unwrap_or_else(|e| panic!("failed to enqueue instance {index}: {e:?}"));
    }

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("Failed to connect for lock holding");

    // Competing session: hold the instance-level advisory lock for HOLD.
    // This is exactly the contention the dispatcher fleet creates on a hot
    // instance; the lock is taken on the same key the provider uses.
    //
    // Two keys are held, so this test reproduces contention against the
    // procedure both before and after migration 0024:
    //
    //   '<instance_id>'            -- the key used before 0024
    //   '<schema>.<instance_id>'   -- the key used from 0024 onward
    //
    // PostgreSQL advisory locks share one database-wide space, so the unscoped
    // key made two duroxide schemas in the same database contend on identical
    // instance ids. Scoping the key confines contention to the owning schema.
    // Holding both means this test always describes the defect that is
    // actually present, instead of silently failing to contend at all.
    let legacy_key = HOT.to_string();
    let scoped_key = format!("{schema}.{HOT}");
    let holder_pool = pool.clone();
    let holder = tokio::spawn(async move {
        let mut tx = holder_pool.begin().await.expect("begin failed");
        for key in [&legacy_key, &scoped_key] {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(key)
                .execute(&mut *tx)
                .await
                .expect("advisory lock failed");
        }
        tokio::time::sleep(HOLD).await;
        tx.rollback().await.expect("rollback failed");
    });

    // Let the competing session take the lock before the fetch starts.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let started = Instant::now();
    let fetched = provider
        .fetch_orchestration_item(LOCK_TIMEOUT, Duration::ZERO, None)
        .await
        .expect("fetch_orchestration_item should succeed");
    let blocked_for = started.elapsed();

    // Defect 1: a contended instance must not stall the dispatcher. Before the
    // fix this waited out the whole hold.
    assert!(
        blocked_for < MAX_ACCEPTABLE_FETCH,
        "fetch_orchestration_item blocked for {blocked_for:?} behind a contended \
         instance (limit {MAX_ACCEPTABLE_FETCH:?}); the advisory lock is convoying"
    );

    // Defect 2: whatever comes back must carry a lease that is still live. This
    // is the invariant the runtime depends on — an expired lease makes every
    // ack fail, including the poison path that is supposed to terminate the
    // message.
    let (item, lock_token, _attempt) =
        fetched.expect("expected the uncontended instance to be returned");

    let remaining_ms: i64 = sqlx::query_scalar(&format!(
        "SELECT locked_until - (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
           FROM {schema}.instance_locks
          WHERE lock_token = $1"
    ))
    .bind(&lock_token)
    .fetch_one(&pool)
    .await
    .expect("lease row should exist for the returned lock token");

    assert!(
        remaining_ms > 0,
        "fetch_orchestration_item returned instance {} with a lease that had \
         already expired {}ms earlier; every ack against it will fail with \
         'Invalid lock token'",
        item.instance,
        -remaining_ms
    );

    // The uncontended instance was available the whole time, so a dispatcher
    // that routes around contention makes progress instead of stalling.
    assert_eq!(
        item.instance, OTHER,
        "expected the uncontended instance to be picked while {HOT} was locked"
    );

    holder.await.expect("lock holder task panicked");

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("Failed to drop schema");
}
