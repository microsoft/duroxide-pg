// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regression tests for issue #39: a KV snapshot this binary cannot decode
//! must never become a successful empty snapshot. It is reported through
//! `history_error`, the same path the core prescribes for undecodable
//! history, so the runtime counts each attempt and poisons the instance
//! after `max_attempts` instead of retrying it forever.

mod common;

use duroxide::providers::{OrchestrationItem, Provider, WorkItem};
use duroxide::runtime::registry::ActivityRegistry;
use duroxide::runtime::{self, RuntimeOptions};
use duroxide::{
    Client, ErrorDetails, OrchestrationContext, OrchestrationRegistry, PoisonMessageType,
};
use duroxide_pg::PostgresProvider;
use std::sync::Arc;
use std::time::Duration;

/// Small on purpose: the poison test waits for this many 1 s backoffs.
const MAX_ATTEMPTS: u32 = 3;

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

/// Replace the fetch function with one that returns the migration-18 KV
/// snapshot shape (`{key: value}`). This is the state issue #39 left behind.
/// Returns the current definition so the caller can restore it.
async fn install_legacy_fetch_function(provider: &PostgresProvider) -> String {
    let signature = format!(
        "{}.fetch_orchestration_item(bigint,bigint,bigint,bigint)",
        provider.schema_name()
    );
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
    current_definition
}

async fn fetch_item(provider: &PostgresProvider) -> (OrchestrationItem, String, u32) {
    provider
        .fetch_orchestration_item(Duration::from_secs(30), Duration::ZERO, None)
        .await
        .expect("fetch must not fail: an undecodable snapshot is reported on the item")
        .expect("queued orchestration item")
}

async fn queue_count(provider: &PostgresProvider, instance: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {}.orchestrator_queue WHERE instance_id = $1",
        provider.schema_name()
    ))
    .bind(instance)
    .fetch_one(provider.pool())
    .await
    .expect("count queue rows")
}

fn assert_kv_error(error: &str) {
    assert!(
        error.contains("KV snapshot") && error.contains("counter") && error.contains("value"),
        "error must identify the malformed snapshot entry: {error}"
    );
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

    let (item, _, _) = fetch_item(&provider).await;
    assert!(item.history_error.is_none());
    let entry = item
        .kv_snapshot
        .get("counter")
        .expect("counter must remain in the snapshot");
    assert_eq!(entry.value, "325");
    assert_eq!(entry.last_updated_at_ms, 123);

    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn legacy_kv_snapshot_shape_is_reported_as_history_error() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "legacy-kv-snapshot";

    insert_kv_row(&provider, instance, "counter", "325").await;
    enqueue_start(&provider, instance).await;
    install_legacy_fetch_function(&provider).await;

    let (item, _, attempt_count) = fetch_item(&provider).await;
    let error = item
        .history_error
        .as_deref()
        .expect("legacy snapshot must not become successful empty state");
    assert_kv_error(error);
    assert!(
        item.kv_snapshot.is_empty(),
        "no partial snapshot may be handed out: {:?}",
        item.kv_snapshot
    );
    assert_eq!(attempt_count, 1, "the fetch counts as the first attempt");
    assert_eq!(
        queue_count(&provider, instance).await,
        1,
        "nothing may be acknowledged: the runtime needs the item to count and poison"
    );

    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn kv_snapshot_error_keeps_each_attempt_counted() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "kv-snapshot-attempts";

    insert_kv_row(&provider, instance, "counter", "325").await;
    enqueue_start(&provider, instance).await;
    let current_definition = install_legacy_fetch_function(&provider).await;

    // An undecodable snapshot is unrecoverable for this binary, so every
    // fetch must count toward max_attempts. Each round does what the
    // dispatcher does with a history_error item, minus the backoff.
    for round in 1..=3u32 {
        let (item, lock_token, attempt_count) = fetch_item(&provider).await;
        assert_kv_error(
            item.history_error
                .as_deref()
                .unwrap_or_else(|| panic!("round {round}: error must be reported")),
        );
        assert_eq!(
            attempt_count, round,
            "round {round}: the attempt must stay counted"
        );
        provider
            .abandon_orchestration_item(&lock_token, None, false)
            .await
            .expect("abandon like the dispatcher does");
    }

    // Repair the schema. The attempts spent on the broken snapshot stay
    // counted, and the real snapshot comes back intact.
    sqlx::query(&current_definition)
        .execute(provider.pool())
        .await
        .expect("restore fetch function");
    let (item, _, attempt_count) = fetch_item(&provider).await;
    assert!(item.history_error.is_none(), "{:?}", item.history_error);
    assert_eq!(attempt_count, 4);
    assert_eq!(item.kv_snapshot["counter"].value, "325");

    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn kv_snapshot_error_poisons_the_orchestration_after_max_attempts() {
    let schema = next_schema_name();
    let provider = Arc::new(
        PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
            .await
            .expect("create provider"),
    );
    let instance = "kv-snapshot-poison";

    // The state issue #39 left behind: a KV row this instance wrote earlier,
    // and a fetch function rolled back to the migration-18 snapshot shape.
    insert_kv_row(&provider, instance, "counter", "325").await;
    install_legacy_fetch_function(&provider).await;

    let orchestration_registry = OrchestrationRegistry::builder()
        .register(
            "KvSnapshotPoison",
            |_ctx: OrchestrationContext, _input: String| async move {
                Ok("must never run on a broken snapshot".to_string())
            },
        )
        .build();
    let store: Arc<dyn Provider> = provider.clone();
    let rt = runtime::Runtime::start_with_options(
        store.clone(),
        ActivityRegistry::builder().build(),
        orchestration_registry,
        RuntimeOptions {
            max_attempts: MAX_ATTEMPTS,
            dispatcher_min_poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
    )
    .await;

    let client = Client::new(store.clone());
    client
        .start_orchestration(instance, "KvSnapshotPoison", "")
        .await
        .expect("start orchestration");

    let status = client
        .wait_for_orchestration(instance, Duration::from_secs(60))
        .await
        .expect("orchestration must reach a terminal status");
    match status {
        runtime::OrchestrationStatus::Failed {
            details:
                ErrorDetails::Poison {
                    attempt_count,
                    max_attempts,
                    message_type: PoisonMessageType::FailedDeserialization { error, .. },
                    message,
                },
            ..
        } => {
            assert_eq!(max_attempts, MAX_ATTEMPTS);
            assert!(
                attempt_count > MAX_ATTEMPTS,
                "poison must wait for max_attempts, got attempt_count {attempt_count}"
            );
            assert_kv_error(&error);
            assert_kv_error(&message);
        }
        other => panic!("expected a FailedDeserialization poison failure, got: {other:?}"),
    }
    assert_eq!(
        queue_count(&provider, instance).await,
        0,
        "the poison ack must remove the queued work"
    );

    rt.shutdown(None).await;
    common::cleanup_schema(&schema).await;
}

#[tokio::test]
async fn malformed_kv_snapshot_is_not_acknowledged_as_an_orphan_message() {
    let schema = next_schema_name();
    let provider = PostgresProvider::new_with_schema(&get_database_url(), Some(&schema))
        .await
        .expect("create provider");
    let instance = "malformed-kv-orphan";

    // No instance, no history, only a QueueMessage: the orphan-message shape.
    // With an undecodable snapshot the provider must not treat it as orphan.
    insert_kv_row(&provider, instance, "counter", "325").await;
    install_legacy_fetch_function(&provider).await;
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

    let (item, _, _) = fetch_item(&provider).await;
    assert_kv_error(
        item.history_error
            .as_deref()
            .expect("malformed KV snapshot must be reported"),
    );
    assert_eq!(
        queue_count(&provider, instance).await,
        1,
        "KV decode failure must not acknowledge the queue item"
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

    let (item, _, _) = fetch_item(&provider).await;
    assert!(
        item.history_error.is_some(),
        "malformed durable history must be reported"
    );
    assert!(item.history.is_empty());
    assert_eq!(
        queue_count(&provider, instance).await,
        1,
        "history decode failure must not acknowledge the queue item"
    );

    common::cleanup_schema(&schema).await;
}
