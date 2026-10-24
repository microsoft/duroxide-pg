// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Orchestration lock renewal against acknowledge and abandon (migration 0026).
//!
//! Acknowledge and abandon change the instance's queue rows and then delete
//! its `instance_locks` row. They do not take fetch's advisory lock. Before
//! migration 0026, renewal updated `instance_locks` first and the queue rows
//! second, the opposite order, so it could deadlock with either of them
//! (SQLSTATE 40P01). Renewal now updates the queue rows first.
//!
//! Each test pauses acknowledge or abandon after its queue change and starts
//! a renewal. Renewal must wait for the queue rows without holding the
//! `instance_locks` row, and must fail as released once the other call
//! commits. Both calls send the provider's statement once, on their own
//! connections, so a deadlock shows as SQLSTATE 40P01 instead of being
//! retried away.

use crate::harness::*;
use sqlx::postgres::PgConnection;
use sqlx::Connection;
use std::time::Duration;
use tokio::task::JoinHandle;

/// The call that releases the instance lock while renewal runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// Deletes the queue rows, then the instance lock.
    Acknowledge,
    /// Unlocks the queue rows, then deletes the instance lock.
    Abandon,
}

impl Release {
    fn name(self) -> &'static str {
        match self {
            Release::Acknowledge => "acknowledge",
            Release::Abandon => "abandon",
        }
    }

    /// The trigger event and row condition that match this call's queue
    /// change, and only that change. Renewal's own queue update keeps the
    /// token, so it does not match the abandon condition.
    fn pause_point(self, token: &str) -> (&'static str, String) {
        let token = token_literal(token);
        match self {
            Release::Acknowledge => ("DELETE", format!("OLD.lock_token = {token}")),
            Release::Abandon => (
                "UPDATE",
                format!("OLD.lock_token = {token} AND NEW.lock_token IS NULL"),
            ),
        }
    }

    /// Sends the statement `PostgresProvider` sends for this call once, on its
    /// own connection.
    fn spawn(
        self,
        database_url: String,
        schema: String,
        token: String,
    ) -> JoinHandle<Result<(), sqlx::Error>> {
        tokio::spawn(async move {
            let mut connection = PgConnection::connect(&database_url).await?;
            match self {
                // A turn with no history, no new work and default metadata.
                Release::Acknowledge => sqlx::query(&format!(
                    "SELECT {schema}.ack_orchestration_item($1, $2, $3, $4, $5, $6, $7, $8)"
                ))
                .bind(&token)
                .bind(now_ms())
                .bind(1_i64)
                .bind(serde_json::json!([]))
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
                .execute(&mut connection)
                .await
                .map(|_| ()),
                Release::Abandon => sqlx::query(&format!(
                    "SELECT {schema}.abandon_orchestration_item($1, $2, NULL, false)"
                ))
                .bind(&token)
                .bind(now_ms())
                .execute(&mut connection)
                .await
                .map(|_| ()),
            }
        })
    }
}

/// Pauses `release` after its queue change, renews while it is paused, and
/// checks that the two do not deadlock and that renewal fails as released.
async fn renew_while_releasing(release: Release) {
    with_schema(move |schema| async move {
        let database_url = database_url();
        let (_provider, instance, token) =
            fetch_new_instance(&schema, Duration::from_secs(60)).await;
        let pool = raw_pool(&database_url).await;
        let name = release.name();

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
        let (event, condition) = release.pause_point(&token);
        install_pause_trigger_when(
            &pool,
            &schema,
            "orchestrator_queue",
            event,
            &condition,
            ready_key,
            gate_key,
        )
        .await;

        let releasing = release.spawn(database_url.clone(), schema.clone(), token.clone());
        wait_for_advisory_holder(&pool, ready_key).await;

        let renewal = spawn_sql_renewal(database_url.clone(), schema.clone(), token.clone(), 60);
        wait_for_call_to_wait(&pool, &schema, "renew_orchestration_item_lock", false).await;
        // The paused call has not reached instance_locks yet, so only renewal
        // can hold that row now.
        let renewal_held_instance_row = instance_lock_row_is_held(&pool, &schema, &instance).await;

        timed("gate release", gate.commit())
            .await
            .expect("failed to release the paused call");
        let release_result = timed(name, releasing)
            .await
            .unwrap_or_else(|_| panic!("{name} task panicked"));
        let renewal_result = timed("renewal", renewal)
            .await
            .expect("renewal task panicked");

        assert_not_deadlock_victim(name, &release_result);
        assert_not_deadlock_victim("renewal", &renewal_result);
        release_result.unwrap_or_else(|error| panic!("{name} must succeed: {error}"));
        assert!(
            !renewal_held_instance_row,
            "renewal held the instance_locks row while it waited for the queue rows; \
             {name} deletes that row next, so the two can deadlock"
        );
        assert_lease_gone(renewal_result);

        assert!(
            read_lease(&pool, &schema, &instance).await.is_none(),
            "renewal must not recreate a released lock"
        );
        let queue = read_queue(&pool, &schema, &instance).await;
        match release {
            Release::Acknowledge => assert!(
                queue.is_empty(),
                "acknowledged messages must stay deleted: {queue:?}"
            ),
            Release::Abandon => {
                assert!(!queue.is_empty(), "abandoned messages must stay queued");
                assert!(
                    queue.iter().all(|row| *row == (None, None)),
                    "renewal must not lock abandoned messages again: {queue:?}"
                );
            }
        }
    })
    .await;
}

/// Acknowledge pauses after it deletes the instance's queue rows and before it
/// deletes the instance lock. Renewal must wait for the queue rows without
/// holding the instance lock; once acknowledge commits, renewal must fail
/// because the lock was released. Opposite row orders deadlock here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_and_acknowledge_lock_rows_in_the_same_order() {
    renew_while_releasing(Release::Acknowledge).await;
}

/// Abandon pauses after it unlocks the instance's queue rows and before it
/// deletes the instance lock. Renewal must wait for the queue rows without
/// holding the instance lock; once abandon commits, renewal must fail because
/// the lock was released, and the messages must stay unlocked. Opposite row
/// orders deadlock here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_and_abandon_lock_rows_in_the_same_order() {
    renew_while_releasing(Release::Abandon).await;
}
