//! Regression tests for duroxide-pg bugs.
//!
//! Each test in this file reproduces a specific bug that was reported and fixed.
//! These tests ensure the bugs don't regress.

use duroxide::runtime::registry::ActivityRegistry;
use duroxide::runtime::{self, RuntimeOptions};
use duroxide::{ActivityContext, Client, OrchestrationContext, OrchestrationRegistry};
use duroxide_pg::PostgresProvider;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;

mod common;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("regression_test_{suffix}")
}

async fn cleanup_schema(schema_name: &str) {
    let database_url = get_database_url();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to connect for cleanup");

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE"))
        .execute(&pool)
        .await
        .expect("Failed to drop schema");
}

// =============================================================================
// Bug: Deadlock in fetch_orchestration_item during parallel sub-orchestrations
// =============================================================================
//
// Reporter: pg_durable development team
// Date: December 7, 2025
// Reference: /Users/affandar/pg_durable/docs/DUROXIDE_PG_DEADLOCK_ISSUE.md
//
// Summary:
// When multiple sub-orchestrations complete and race to notify their parent,
// a deadlock can occur in fetch_orchestration_item due to INSERT ... ON CONFLICT
// on the instance_locks table. The deadlock happens at the B-tree index level
// when concurrent transactions try to insert/update different rows.
//
// Fix: Two-phase locking with instance-level advisory locks.
//   Phase 1: Peek (no lock) to find a candidate instance
//   Phase 2: Acquire pg_advisory_xact_lock(hashtext(instance_id))
//   Phase 3: Re-verify with FOR UPDATE to confirm availability
//
// This preserves parallelism (different instances process concurrently) while
// preventing deadlocks (same-instance operations are serialized).
//
// Also added retry logic in provider for transient deadlock errors (40P01).
//
// =============================================================================

/// Regression test for pg_durable deadlock issue.
///
/// This test creates a parent orchestration that spawns multiple children.
/// All children complete nearly simultaneously and race to notify the parent.
/// Before the fix, this would cause deadlocks ~50% of the time.
/// After the fix (advisory locks), no deadlocks should occur.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_parallel_suborchestrations_no_deadlock() {
    const NUM_PARENTS: usize = 20;
    const NUM_CHILDREN: usize = 5;
    const TIMEOUT_SECS: u64 = 60;

    let schema = unique_schema_name();
    let database_url = get_database_url();

    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Failed to create provider");
    let store: Arc<dyn duroxide::providers::Provider> = Arc::new(provider);

    // Simple activity that returns immediately
    let activity_registry = ActivityRegistry::builder()
        .register(
            "DoWork",
            |_ctx: ActivityContext, input: String| async move { Ok(format!("done:{input}")) },
        )
        .build();

    // Child orchestration - does minimal work and completes
    let child = |ctx: OrchestrationContext, input: String| async move {
        let result = ctx
            .schedule_activity("DoWork", input)
            
            .await?;
        Ok(result)
    };

    // Parent orchestration - spawns N children and waits for all
    let parent = |ctx: OrchestrationContext, input: String| async move {
        // Spawn multiple children that will all complete around the same time
        let mut futures = Vec::new();
        for i in 0..NUM_CHILDREN {
            let fut = ctx.schedule_sub_orchestration("Child", format!("{input}:{i}"));
            futures.push(fut);
        }

        // Wait for all children - this is where the deadlock used to occur
        // as all SubOrchCompleted messages arrive nearly simultaneously
        let results = ctx.join(futures).await;

        let outputs: Vec<String> = results
            .into_iter()
            .filter_map(|out| out.ok())
            .collect();

        Ok(format!("completed:{}", outputs.len()))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("Child", child)
        .register("Parent", parent)
        .build();

    // Use fast polling to increase chance of concurrent operations
    let options = RuntimeOptions {
        dispatcher_min_poll_interval: Duration::from_millis(1),
        ..Default::default()
    };

    let rt = runtime::Runtime::start_with_options(
        store.clone(),
        activity_registry,
        orchestration_registry,
        options,
    )
    .await;

    let client = Client::new(store.clone());

    // Start many parent orchestrations simultaneously
    for i in 0..NUM_PARENTS {
        client
            .start_orchestration(&format!("parent-{i}"), "Parent", format!("input-{i}"))
            .await
            .expect("Failed to start orchestration");
    }

    // Wait for all to complete
    let mut completed = 0;
    let mut failed = 0;

    for i in 0..NUM_PARENTS {
        let instance = format!("parent-{i}");
        match client
            .wait_for_orchestration(&instance, Duration::from_secs(TIMEOUT_SECS))
            .await
        {
            Ok(runtime::OrchestrationStatus::Completed { output, .. }) => {
                assert!(
                    output.contains(&format!("completed:{NUM_CHILDREN}")),
                    "Expected all {NUM_CHILDREN} children to complete, got: {output}"
                );
                completed += 1;
            }
            Ok(runtime::OrchestrationStatus::Failed { details, .. }) => {
                eprintln!("Parent {} failed: {}", instance, details.display_message());
                failed += 1;
            }
            Ok(status) => {
                eprintln!("Parent {instance} unexpected status: {status:?}");
                failed += 1;
            }
            Err(e) => {
                eprintln!("Parent {instance} error: {e}");
                failed += 1;
            }
        }
    }

    rt.shutdown(None).await;
    let _ = cleanup_schema(&schema).await;

    // All orchestrations should complete successfully
    assert_eq!(
        failed, 0,
        "Expected 0 failures, got {failed}. Completed: {completed}/{NUM_PARENTS}"
    );
    assert_eq!(
        completed, NUM_PARENTS,
        "Expected {NUM_PARENTS} completions, got {completed}"
    );
}

/// Stress test with higher parallelism.
///
/// Runs more orchestrations with more children to stress test the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn test_parallel_suborchestrations_stress() {
    const NUM_PARENTS: usize = 30;
    const NUM_CHILDREN: usize = 4;
    const TIMEOUT_SECS: u64 = 90;

    let schema = unique_schema_name();
    let database_url = get_database_url();

    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Failed to create provider");
    let store: Arc<dyn duroxide::providers::Provider> = Arc::new(provider);

    let activity_registry = ActivityRegistry::builder()
        .register("Work", |_ctx: ActivityContext, input: String| async move {
            Ok(input)
        })
        .build();

    let child = |ctx: OrchestrationContext, input: String| async move {
        ctx.schedule_activity("Work", input.clone())
            
            .await
    };

    let parent = |ctx: OrchestrationContext, _input: String| async move {
        let futures: Vec<_> = (0..NUM_CHILDREN)
            .map(|i| ctx.schedule_sub_orchestration("StressChild", format!("c{i}")))
            .collect();
        let results = ctx.join(futures).await;
        let count = results
            .iter()
            .filter(|r| r.is_ok())
            .count();
        Ok(format!("done:{count}"))
    };

    let orchestration_registry = OrchestrationRegistry::builder()
        .register("StressChild", child)
        .register("StressParent", parent)
        .build();

    let options = RuntimeOptions {
        dispatcher_min_poll_interval: Duration::from_millis(1),
        ..Default::default()
    };

    let rt = runtime::Runtime::start_with_options(
        store.clone(),
        activity_registry,
        orchestration_registry,
        options,
    )
    .await;

    let client = Client::new(store.clone());

    // Start all parents
    for i in 0..NUM_PARENTS {
        client
            .start_orchestration(&format!("stress-{i}"), "StressParent", "go")
            .await
            .expect("Failed to start");
    }

    // Collect results
    let mut success = 0;
    for i in 0..NUM_PARENTS {
        if let Ok(runtime::OrchestrationStatus::Completed { output, .. }) = client
            .wait_for_orchestration(&format!("stress-{i}"), Duration::from_secs(TIMEOUT_SECS))
            .await
        {
            if output == format!("done:{NUM_CHILDREN}") {
                success += 1;
            }
        }
    }

    rt.shutdown(None).await;
    let _ = cleanup_schema(&schema).await;

    assert_eq!(
        success, NUM_PARENTS,
        "Expected all {NUM_PARENTS} to succeed, got {success}"
    );
}
