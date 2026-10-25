// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Acknowledge against fetch (migration 0026).
//!
//! Fetch takes an instance over when, by its caller's clock, the lease has
//! expired. Before migration 0026, acknowledge held no lock between its token
//! check and its commit, so a fetch from a worker whose clock runs ahead
//! could replace the token in the middle of an acknowledge and commit first.
//! The acknowledge then committed its history and new work while its deletes,
//! which match on the old token, removed nothing, and the messages it had
//! processed were delivered again. Acknowledge now takes fetch's advisory
//! lock right after its token check, so the fetch waits for it.

use crate::harness::*;
use std::time::Duration;

/// Acknowledge pauses after its token check, while it writes history. A fetch
/// from a worker whose clock is 120 s ahead sees the lease as expired. The
/// fetch must wait for the acknowledge instead of taking the instance over,
/// and must then find nothing to take: the messages were consumed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_waits_for_an_acknowledge_in_progress() {
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

        let fetch = spawn_sql_fetch(
            database_url.clone(),
            schema.clone(),
            now_ms() + 120_000,
            30_000,
        );
        wait_for_call_to_wait(&pool, &schema, "fetch_orchestration_item", true).await;
        let held = read_lease(&pool, &schema, &instance)
            .await
            .expect("the instance must still be locked");
        assert_eq!(
            held.token, token,
            "fetch took the instance over while the acknowledge was running"
        );

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the paused acknowledge");
        let ack_result = timed("acknowledge", ack)
            .await
            .expect("acknowledge task panicked");
        let fetch_result = timed("fetch", fetch).await.expect("fetch task panicked");

        assert_not_deadlock_victim("acknowledge", &ack_result);
        assert_not_deadlock_victim("fetch", &fetch_result);
        ack_result.expect("acknowledge must succeed");
        let fetched = fetch_result.expect("fetch must succeed");
        assert_eq!(
            fetched, None,
            "fetch must not take over an instance whose messages were acknowledged"
        );

        assert!(
            read_queue(&pool, &schema, &instance).await.is_empty(),
            "acknowledged messages must be consumed exactly once"
        );
        assert!(
            read_lease(&pool, &schema, &instance).await.is_none(),
            "acknowledge must release the lock"
        );
        assert_eq!(history_len(&pool, &schema, &instance).await, 1);
    })
    .await;
}
