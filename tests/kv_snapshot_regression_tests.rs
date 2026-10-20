// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod common;

use duroxide::providers::{Provider, WorkItem};
use duroxide_pg::PostgresProvider;
use std::time::Duration;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn next_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("kv_snapshot_{suffix}")
}

async fn enqueue_start(provider: &PostgresProvider, instance: &str) {
    provider
        .enqueue_for_orchestrator(
            WorkItem::StartOrchestration {
                instance: instance.to_string(),
                orchestration: "KvSnapshotRegression".to_string(),
                version: Some("1.0.0".to_string()),
                input: String::new(),
                parent_instance: None,
                parent_id: None,
                parent_execution_id: None,
                execution_id: duroxide::INITIAL_EXECUTION_ID,
            },
            None,
        )
        .await
        .expect("enqueue start");
}

async fn insert_kv_row(provider: &PostgresProvider, instance: &str, key: &str, value: &str) {
    sqlx::query(&format!(
        "INSERT INTO {}.kv_store \
         (instance_id, key, value, execution_id, last_updated_at_ms) \
         VALUES ($1, $2, $3, $4, $5)",
        provider.schema_name()
    ))
    .bind(instance)
    .bind(key)
    .bind(value)
    .bind(duroxide::INITIAL_EXECUTION_ID as i64)
    .bind(123_i64)
    .execute(provider.pool())
    .await
    .expect("insert KV row");
}

#[tokio::test]
async fn valid_kv_snapshot_is_preserved() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "valid-kv-snapshot";

    insert_kv_row(&provider, instance, "counter", "325").await;
    enqueue_start(&provider, instance).await;

    let (item, _, _) = provider
        .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
        .await
        .expect("fetch should succeed")
        .expect("queued orchestration item");
    let entry = item
        .kv_snapshot
        .get("counter")
        .expect("counter must remain in the snapshot");
    assert_eq!(entry.value, "325");
    assert_eq!(entry.last_updated_at_ms, 123);

    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn legacy_kv_snapshot_shape_returns_a_permanent_error() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "legacy-kv-snapshot";

    insert_kv_row(&provider, instance, "counter", "325").await;
    enqueue_start(&provider, instance).await;

    let signature = format!("{schema}.fetch_orchestration_item(bigint,bigint,bigint,bigint)");
    let current_definition: String =
        sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1)::oid)")
            .bind(&signature)
            .fetch_one(provider.pool())
            .await
            .expect("read current fetch function");
    let legacy_definition = current_definition.replace(
        "jsonb_build_object('value', ks.value, 'last_updated_at_ms', ks.last_updated_at_ms)",
        "to_jsonb(ks.value)",
    );
    assert_ne!(
        legacy_definition, current_definition,
        "test setup did not replace the KV snapshot shape"
    );
    sqlx::query(&legacy_definition)
        .execute(provider.pool())
        .await
        .expect("install legacy fetch function");

    let result = provider
        .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
        .await;
    let err = match result {
        Ok(Some((item, _, _))) => panic!(
            "legacy snapshot must not become successful empty state: {:?}",
            item.kv_snapshot
        ),
        Ok(None) => panic!("queued item unexpectedly disappeared"),
        Err(e) => e,
    };
    assert!(
        !err.is_retryable(),
        "schema/data shape errors must be permanent: {err:?}"
    );
    assert_eq!(err.operation, "fetch_orchestration_item");
    assert!(
        err.message.contains("KV snapshot")
            && err.message.contains("counter")
            && err.message.contains("value"),
        "error must identify the malformed snapshot entry: {err:?}"
    );

    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn malformed_history_is_not_acknowledged_as_an_orphan_message() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "malformed-history";

    sqlx::query(&format!(
        "INSERT INTO {schema}.history \
         (instance_id, execution_id, event_id, event_type, event_data, created_at) \
         VALUES ($1, $2, $3, $4, $5, CURRENT_TIMESTAMP)"
    ))
    .bind(instance)
    .bind(duroxide::INITIAL_EXECUTION_ID as i64)
    .bind(duroxide::INITIAL_EVENT_ID as i64)
    .bind("Malformed")
    .bind(r#"{"unexpected":"event-shape"}"#)
    .execute(provider.pool())
    .await
    .expect("insert malformed durable history");

    provider
        .enqueue_for_orchestrator(
            WorkItem::QueueMessage {
                instance: instance.to_string(),
                name: "Signal".to_string(),
                data: "payload".to_string(),
            },
            None,
        )
        .await
        .expect("enqueue orphan-shaped message");

    let (item, _, _) = provider
        .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
        .await
        .expect("fetch should preserve the error-bearing item")
        .expect("queued orchestration item");
    assert!(
        item.history_error.is_some(),
        "malformed durable history must be reported"
    );
    assert!(item.history.is_empty());

    let queue_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {schema}.orchestrator_queue WHERE instance_id = $1"
    ))
    .bind(instance)
    .fetch_one(provider.pool())
    .await
    .expect("count queue rows after fetch");
    assert_eq!(
        queue_count, 1,
        "history decode failure must not acknowledge the queue item"
    );

    common::cleanup_schema(&schema).await;
}
