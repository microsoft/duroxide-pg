// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::harness::*;
use duroxide_pg::PostgresProvider;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

type BatchResult = (Option<String>, Option<i64>, i64, i64, i64);

async fn seed_instance(schema: &str) -> PostgresProvider {
    let provider = PostgresProvider::new_with_schema(&database_url(), Some(schema))
        .await
        .expect("failed to create provider");
    sqlx::Executor::execute(
        provider.pool(),
        format!(
            "INSERT INTO {schema}.instances
             (instance_id, orchestration_name, current_execution_id, created_at, updated_at)
         VALUES ('pruning-race', 'test', 2, to_timestamp(0), to_timestamp(0));
         INSERT INTO {schema}.executions
             (instance_id, execution_id, status, started_at, completed_at)
         VALUES ('pruning-race', 1, 'ContinuedAsNew', to_timestamp(0), to_timestamp(1)),
                ('pruning-race', 2, 'Running', to_timestamp(1), NULL);
         INSERT INTO {schema}.history
             (instance_id, execution_id, event_id, event_type, event_data, created_at)
         SELECT 'pruning-race', n, 1, 'Event', 'original', to_timestamp(0)
         FROM generate_series(1, 2) n;"
        )
        .as_str(),
    )
    .await
    .expect("failed to seed pruning history");
    provider
}

async fn durable_state(connection: &mut PgConnection, schema: &str) -> Value {
    sqlx::query_scalar(&format!(
        "SELECT jsonb_build_object(
             'instances', (SELECT jsonb_agg(i ORDER BY i.instance_id)
                           FROM {schema}.instances i),
             'executions', (SELECT jsonb_agg(e ORDER BY e.instance_id, e.execution_id)
                            FROM {schema}.executions e),
             'history', (SELECT jsonb_agg(h ORDER BY h.instance_id, h.execution_id, h.event_id)
                         FROM {schema}.history h))"
    ))
    .fetch_one(connection)
    .await
    .expect("failed to snapshot durable state")
}

async fn prune_batch(pool: PgPool, schema: String) -> Result<BatchResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query_as(&format!(
        "SELECT * FROM {schema}.prune_executions_batch('', 0, 2000, 10)"
    ))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_pruning_preserves_a_deleted_and_recreated_instance() {
    with_schema(|schema| async move {
        let provider = seed_instance(&schema).await;
        let mut replacement = provider.pool().begin().await.unwrap();
        let deleted: (i64, i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT * FROM {schema}.delete_instances_atomic(ARRAY['pruning-race'], TRUE)"
        ))
        .fetch_one(&mut *replacement)
        .await
        .expect("failed to delete the original instance");
        assert_eq!(deleted, (1, 2, 2, 0));
        sqlx::Executor::execute(
            &mut *replacement,
            format!(
                "INSERT INTO {schema}.instances
                 (instance_id, orchestration_name, current_execution_id, created_at, updated_at)
             VALUES ('pruning-race', 'replacement', 1, to_timestamp(3), to_timestamp(3));
             INSERT INTO {schema}.executions
                 (instance_id, execution_id, status, started_at, completed_at)
             VALUES ('pruning-race', 1, 'Running', to_timestamp(3), NULL);
             INSERT INTO {schema}.history
                 (instance_id, execution_id, event_id, event_type, event_data, created_at)
             VALUES ('pruning-race', 1, 1, 'Event', 'replacement', to_timestamp(3));"
            )
            .as_str(),
        )
        .await
        .expect("failed to recreate the instance");
        let before = durable_state(&mut replacement, &schema).await;

        // The page sees the old committed lifetime while its replacement is uncommitted.
        let prune = tokio::spawn(prune_batch(provider.pool().clone(), schema.clone()));
        wait_for_call_to_wait(provider.pool(), &schema, "prune_executions_batch", false).await;
        timed("replacement commit", replacement.commit())
            .await
            .unwrap();
        let result = timed("batch pruning", prune).await.unwrap();
        assert_not_deadlock_victim("batch pruning", &result);
        let result = result.expect("batch pruning must succeed");

        let mut connection = provider.pool().acquire().await.unwrap();
        assert_eq!(
            durable_state(&mut connection, &schema).await,
            before,
            "a stale page must not delete the replacement's current execution or history"
        );
        assert_eq!(result, (Some("pruning-race".into()), Some(2), 2, 0, 0));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_pruning_rechecks_eligibility_after_an_instance_lock_wait() {
    with_schema(|schema| async move {
        let provider = seed_instance(&schema).await;
        let mut update = provider.pool().begin().await.unwrap();
        sqlx::Executor::execute(
            &mut *update,
            format!(
                "UPDATE {schema}.instances SET updated_at = to_timestamp(3)
             WHERE instance_id = 'pruning-race';
             UPDATE {schema}.executions SET status = 'Running'
             WHERE instance_id = 'pruning-race' AND execution_id = 1;"
            )
            .as_str(),
        )
        .await
        .expect("failed to update execution eligibility");
        let before = durable_state(&mut update, &schema).await;

        let prune = tokio::spawn(prune_batch(provider.pool().clone(), schema.clone()));
        wait_for_call_to_wait(provider.pool(), &schema, "prune_executions_batch", false).await;
        timed("execution update commit", update.commit())
            .await
            .unwrap();
        let result = timed("batch pruning", prune).await.unwrap();
        assert_not_deadlock_victim("batch pruning", &result);
        let result = result.expect("batch pruning must succeed");

        let mut connection = provider.pool().acquire().await.unwrap();
        assert_eq!(
            durable_state(&mut connection, &schema).await,
            before,
            "eligibility must be read again after the instance lock is acquired"
        );
        assert_eq!(result, (Some("pruning-race".into()), Some(2), 2, 0, 0));
    })
    .await;
}
