// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::harness::*;
use duroxide_pg::PostgresProvider;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

type BatchResult = (Option<String>, Option<i64>, i64, i64, i64);

const SEQUENTIAL_SCANS_ONLY: &str = "SET LOCAL enable_seqscan = on;
    SET LOCAL enable_indexscan = off;
    SET LOCAL enable_indexonlyscan = off;
    SET LOCAL enable_bitmapscan = off;";

async fn seed_instances(
    schema: &str,
    instance_ids: &[&str],
    current_execution_id: i64,
) -> PostgresProvider {
    let provider = PostgresProvider::new_with_schema(&database_url(), Some(schema))
        .await
        .expect("failed to create provider");
    for instance_id in instance_ids {
        sqlx::query(&format!(
            "INSERT INTO {schema}.instances
                 (instance_id, orchestration_name, current_execution_id, created_at, updated_at)
             VALUES ($1, 'test', $2, to_timestamp(0), to_timestamp(0))"
        ))
        .bind(*instance_id)
        .bind(current_execution_id)
        .execute(provider.pool())
        .await
        .expect("failed to seed instance");
    }
    sqlx::Executor::execute(
        provider.pool(),
        format!(
            "INSERT INTO {schema}.executions
             (instance_id, execution_id, status, started_at, completed_at)
         SELECT i.instance_id, n,
                CASE WHEN n < i.current_execution_id THEN 'ContinuedAsNew' ELSE 'Running' END,
                to_timestamp(n - 1),
                CASE WHEN n < i.current_execution_id THEN to_timestamp(1) END
         FROM {schema}.instances i CROSS JOIN generate_series(1, i.current_execution_id) n;
         INSERT INTO {schema}.history
             (instance_id, execution_id, event_id, event_type, event_data, created_at)
         SELECT instance_id, execution_id, 1, 'Event', 'original', to_timestamp(0)
         FROM {schema}.executions;"
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

async fn pause_first_history_delete(
    pool: &PgPool,
    schema: &str,
    instance: &str,
) -> (i64, sqlx::Transaction<'static, sqlx::Postgres>) {
    let ready_key = advisory_key();
    let gate_key = advisory_key();
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(gate_key)
        .execute(&mut *gate)
        .await
        .unwrap();
    install_pause_trigger_when(
        pool,
        schema,
        "history",
        "DELETE",
        &format!("OLD.instance_id = $instance${instance}$instance$ AND OLD.execution_id = 1"),
        ready_key,
        gate_key,
    )
    .await;
    (ready_key, gate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_pruning_does_not_lock_ineligible_instances() {
    with_schema(|schema| async move {
        let provider = seed_instances(
            &schema,
            &[
                "a-current",
                "b-future",
                "c-running",
                "d-null",
                "e-cutoff",
                "f-young",
                "z-eligible",
            ],
            2,
        )
        .await;
        sqlx::raw_sql(&format!(
            "UPDATE {schema}.instances SET current_execution_id=1 WHERE instance_id='a-current';
             UPDATE {schema}.instances SET current_execution_id=0 WHERE instance_id='b-future';
             UPDATE {schema}.executions SET status='Running' WHERE instance_id='c-running';
             UPDATE {schema}.executions SET completed_at=NULL WHERE instance_id='d-null';
             UPDATE {schema}.executions SET completed_at=to_timestamp(2) WHERE instance_id='e-cutoff';
             UPDATE {schema}.executions SET completed_at=to_timestamp(3) WHERE instance_id='f-young';"
        ))
        .execute(provider.pool())
        .await
        .unwrap();
        let mut locked = provider.pool().begin().await.unwrap();
        let mut expected = durable_state(&mut locked, &schema).await;
        for table in ["executions", "history"] {
            expected[table]
                .as_array_mut()
                .expect("seeded durable rows must exist")
                .retain(|row| row["instance_id"] != "z-eligible" || row["execution_id"] != 1);
        }
        sqlx::query(&format!(
            "SELECT 1 FROM {schema}.instances WHERE instance_id <> 'z-eligible'
             ORDER BY instance_id FOR NO KEY UPDATE"
        ))
        .execute(&mut *locked)
        .await
        .unwrap();

        let mut prune = provider.pool().begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '1s'")
            .execute(&mut *prune)
            .await
            .unwrap();
        let result: BatchResult = timed(
            "pruning past locked ineligible instances",
            sqlx::query_as(&format!(
                "SELECT * FROM {schema}.prune_executions_batch('', 0, 2000, 1000)"
            ))
            .fetch_one(&mut *prune),
        )
        .await
        .expect("protected instances must not require row locks");
        assert_eq!(result, (Some("z-eligible".into()), Some(2), 14, 1, 1));
        prune.commit().await.unwrap();
        assert_eq!(
            durable_state(&mut locked, &schema).await,
            expected,
            "only the eligible historical execution and its history may change"
        );
        locked.rollback().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_pruning_preserves_a_deleted_and_recreated_instance() {
    with_schema(|schema| async move {
        let provider = seed_instances(&schema, &["pruning-race"], 2).await;
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
        let provider = seed_instances(&schema, &["pruning-race"], 2).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_pruning_and_multi_instance_deletion_lock_in_the_same_order() {
    with_schema(|schema| async move {
        // Heap order deliberately opposes the batch's instance-ID order.
        let provider = seed_instances(&schema, &["b", "a"], 2).await;
        let (ready_key, gate) = pause_first_history_delete(provider.pool(), &schema, "a").await;

        let prune = tokio::spawn(prune_batch(provider.pool().clone(), schema.clone()));
        wait_for_advisory_holder(provider.pool(), ready_key).await;

        let delete_pool = provider.pool().clone();
        let delete_schema = schema.clone();
        let delete = tokio::spawn(async move {
            let mut tx = delete_pool.begin().await?;
            // Without ORDER BY, this visits b before waiting for the batch's a lock.
            sqlx::Executor::execute(&mut *tx, SEQUENTIAL_SCANS_ONLY).await?;
            let result: (i64, i64, i64, i64) = sqlx::query_as(&format!(
                "SELECT * FROM {delete_schema}.delete_instances_atomic(ARRAY['b', 'a'], TRUE)"
            ))
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(result)
        });
        wait_for_call_to_wait(provider.pool(), &schema, "delete_instances_atomic", false).await;

        timed("pruning gate release", gate.commit()).await.unwrap();
        let prune_result = timed("batch pruning", prune).await.unwrap();
        let delete_result = timed("multi-instance deletion", delete).await.unwrap();
        assert_not_deadlock_victim("batch pruning", &prune_result);
        assert_not_deadlock_victim("multi-instance deletion", &delete_result);
        assert_eq!(
            prune_result.expect("batch pruning must succeed"),
            (Some("b".into()), Some(2), 4, 2, 2)
        );
        assert_eq!(
            delete_result.expect("multi-instance deletion must succeed"),
            (2, 2, 2, 0)
        );
        let mut connection = provider.pool().acquire().await.unwrap();
        assert_eq!(
            durable_state(&mut connection, &schema).await,
            serde_json::json!({"instances": null, "executions": null, "history": null})
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_and_whole_instance_pruning_share_the_instance_lock() {
    with_schema(|schema| async move {
        let provider = seed_instances(&schema, &["pruning-race"], 3).await;
        // Force the old whole-instance pruner to visit generation 2 before 1.
        sqlx::Executor::execute(
            provider.pool(),
            format!(
                "CREATE INDEX reverse_execution_order ON {schema}.executions (execution_id DESC);
                 CLUSTER {schema}.executions USING reverse_execution_order;"
            )
            .as_str(),
        )
        .await
        .expect("failed to arrange reverse generation heap order");
        let mut connection = provider.pool().acquire().await.unwrap();
        let mut expected = durable_state(&mut connection, &schema).await;
        for table in ["executions", "history"] {
            expected[table]
                .as_array_mut()
                .expect("seeded durable rows must exist")
                .retain(|row| row["execution_id"] == 3);
        }
        drop(connection);

        let (ready_key, gate) =
            pause_first_history_delete(provider.pool(), &schema, "pruning-race").await;
        let batch = tokio::spawn(prune_batch(provider.pool().clone(), schema.clone()));
        wait_for_advisory_holder(provider.pool(), ready_key).await;

        let whole_pool = provider.pool().clone();
        let whole_schema = schema.clone();
        let whole = tokio::spawn(async move {
            let mut tx = whole_pool.begin().await?;
            sqlx::Executor::execute(&mut *tx, SEQUENTIAL_SCANS_ONLY).await?;
            let result: (i64, i64, i64) = sqlx::query_as(&format!(
                "SELECT * FROM {whole_schema}.prune_executions('pruning-race', NULL, 2000)"
            ))
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(result)
        });
        wait_for_call_to_wait(provider.pool(), &schema, "prune_executions", false).await;

        timed("pruning gate release", gate.commit()).await.unwrap();
        let batch_result = timed("batch pruning", batch).await.unwrap();
        let whole_result = timed("whole-instance pruning", whole).await.unwrap();
        assert_not_deadlock_victim("batch pruning", &batch_result);
        assert_not_deadlock_victim("whole-instance pruning", &whole_result);
        assert_eq!(
            batch_result.expect("batch pruning must succeed"),
            (Some("pruning-race".into()), Some(3), 3, 2, 2)
        );
        assert_eq!(
            whole_result.expect("whole-instance pruning must succeed"),
            (1, 0, 0)
        );
        let mut connection = provider.pool().acquire().await.unwrap();
        assert_eq!(durable_state(&mut connection, &schema).await, expected);
    })
    .await;
}
