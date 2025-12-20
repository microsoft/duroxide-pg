//! Fault injection tests for long-polling implementation
//!
//! These tests verify the system handles various failure modes gracefully,
//! including notifier panics, query errors, and edge cases.
//!
//! Run with: cargo test --test fault_injection_tests --features test-fault-injection -- --ignored --nocapture

mod common;

use duroxide::providers::{Provider, WorkItem};
use duroxide_pg_opt::{LongPollConfig, PostgresProvider};
#[cfg(feature = "test-fault-injection")]
use duroxide_pg_opt::FaultInjector;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn next_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("fi_test_{suffix}")
}

async fn cleanup_schema(schema_name: &str) {
    let database_url = get_database_url();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database for schema cleanup");

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE"))
        .execute(&pool)
        .await
        .expect("Failed to drop test schema");
}

// =============================================================================
// Category 11: Fault Injection Tests
// =============================================================================

/// Test that a notifier panic causes fallback to poll_timeout.
#[tokio::test]
#[cfg(feature = "test-fault-injection")]
async fn fault_notifier_panic() {
    let schema = next_schema_name();
    let database_url = get_database_url();

    let fault_injector = Arc::new(FaultInjector::new());

    let provider = PostgresProvider::new_with_fault_injection(
        &database_url,
        Some(&schema),
        LongPollConfig::default(),
        fault_injector.clone(),
    )
    .await
    .expect("Failed to create provider");

    // Trigger panic in notifier
    fault_injector.set_notifier_should_panic(true);

    // Give time for panic to occur
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Insert work
    provider
        .enqueue_for_orchestrator(
            WorkItem::StartOrchestration {
                instance: "panic-test".to_string(),
                orchestration: "test-orch".to_string(),
                version: Some("1.0".to_string()),
                input: "{}".to_string(),
                parent_instance: None,
                parent_id: None,
                execution_id: 1,
            },
            None,
        )
        .await
        .expect("Failed to enqueue work");

    // Fetch should still work (via do_fetch, then poll_timeout fallback)
    let result = provider
        .fetch_orchestration_item(Duration::from_secs(5), Duration::from_secs(1))
        .await
        .expect("Fetch failed");

    assert!(result.is_some(), "Should find work despite notifier panic");

    cleanup_schema(&schema).await;
}

/// Test that refresh query errors are handled gracefully.
#[tokio::test]
#[cfg(feature = "test-fault-injection")]
async fn fault_refresh_query_error() {
    let schema = next_schema_name();
    let database_url = get_database_url();

    let fault_injector = Arc::new(FaultInjector::new());

    let provider = PostgresProvider::new_with_fault_injection(
        &database_url,
        Some(&schema),
        LongPollConfig::default(),
        fault_injector.clone(),
    )
    .await
    .expect("Failed to create provider");

    // Make refresh queries fail
    fault_injector.set_refresh_should_error(true);

    // Wait for refresh to fail
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Insert and fetch should still work (NOTIFY still functions)
    provider
        .enqueue_for_orchestrator(
            WorkItem::StartOrchestration {
                instance: "refresh-error-test".to_string(),
                orchestration: "test-orch".to_string(),
                version: Some("1.0".to_string()),
                input: "{}".to_string(),
                parent_instance: None,
                parent_id: None,
                execution_id: 1,
            },
            None,
        )
        .await
        .expect("Failed to enqueue work");

    let result = provider
        .fetch_orchestration_item(Duration::from_secs(5), Duration::from_secs(2))
        .await
        .expect("Fetch failed");

    assert!(
        result.is_some(),
        "Should find work despite refresh query errors"
    );

    cleanup_schema(&schema).await;
}

/// Test that timers with negative delay fire immediately (no crash).
#[tokio::test]
async fn fault_heap_corruption_negative_timer() {
    let schema = next_schema_name();
    let database_url = get_database_url();

    let provider = Arc::new(
        PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("Failed to create provider"),
    );

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Insert work with visible_at in the far past (simulating heap corruption)
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to connect");

    let past = chrono::Utc::now() - chrono::Duration::hours(1);

    sqlx::query(&format!(
        r#"INSERT INTO {}.orchestrator_queue
           (instance_id, work_item, visible_at, created_at)
           VALUES ($1, $2, $3, NOW())"#,
        schema
    ))
    .bind("negative-timer-test")
    .bind(
        serde_json::to_string(&serde_json::json!({
            "StartOrchestration": {
                "instance": "negative-timer-test",
                "orchestration": "test-orch",
                "version": "1.0",
                "input": "{}",
                "execution_id": 1
            }
        }))
        .unwrap(),
    )
    .bind(past)
    .execute(&pool)
    .await
    .expect("Failed to insert work");

    // Should find work immediately (no crash)
    let start = Instant::now();
    let result = provider
        .fetch_orchestration_item(Duration::from_secs(5), Duration::from_secs(2))
        .await
        .expect("Fetch failed");

    let elapsed = start.elapsed();

    assert!(result.is_some(), "Should find work with past visible_at");
    assert!(
        elapsed < Duration::from_millis(500),
        "Past timer should be found immediately, took {:?}",
        elapsed
    );

    pool.close().await;
    cleanup_schema(&schema).await;
}
