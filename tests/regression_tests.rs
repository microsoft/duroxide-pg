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
            .into_activity()
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
            .filter_map(|out| match out {
                duroxide::DurableOutput::SubOrchestration(Ok(s)) => Some(s),
                _ => None,
            })
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
        Arc::new(activity_registry),
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
            Ok(runtime::OrchestrationStatus::Completed { output }) => {
                assert!(
                    output.contains(&format!("completed:{NUM_CHILDREN}")),
                    "Expected all {NUM_CHILDREN} children to complete, got: {output}"
                );
                completed += 1;
            }
            Ok(runtime::OrchestrationStatus::Failed { details }) => {
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
            .into_activity()
            .await
    };

    let parent = |ctx: OrchestrationContext, _input: String| async move {
        let futures: Vec<_> = (0..NUM_CHILDREN)
            .map(|i| ctx.schedule_sub_orchestration("StressChild", format!("c{i}")))
            .collect();
        let results = ctx.join(futures).await;
        let count = results
            .iter()
            .filter(|r| matches!(r, duroxide::DurableOutput::SubOrchestration(Ok(_))))
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
        Arc::new(activity_registry),
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
        if let Ok(runtime::OrchestrationStatus::Completed { output }) = client
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

// =============================================================================
// Bug: prune_executions_bulk skipping running instances
// =============================================================================
//
// Reporter: duroxide-pg development team
// Date: January 7, 2026
// Reference: duroxide issue #44
//
// Summary:
// Long-running orchestrations that use ContinueAsNew (e.g., eternal orchestrations)
// accumulate many historical executions. The prune_executions_bulk API should
// allow pruning these old executions even while the orchestration is Running.
//
// There were TWO related bugs:
// 1. provider.rs: prune_executions_bulk was filtering to only terminal instances
// 2. migration 0010: prune_executions stored procedure had `AND e.status != 'Running'`
//
// Fix: 
// 1. provider.rs: Changed WHERE clause from status filter to `WHERE 1=1`
// 2. migration 0010: Removed the `AND e.status != 'Running'` clause
//
// The current execution is already protected by `AND e.execution_id != v_current_execution_id`
// so the additional status check was redundant and prevented legitimate pruning.
//
// =============================================================================

/// Regression test for prune_executions_bulk including running instances.
///
/// This test creates an orchestration that does multiple ContinueAsNew cycles
/// then pauses waiting for an external event. While in the Running state with
/// multiple historical executions, we call prune_executions_bulk and verify:
/// 1. The running instance IS processed (not skipped)
/// 2. Old executions ARE pruned
/// 3. The current (running) execution is NOT pruned
#[tokio::test]
async fn test_prune_bulk_includes_running_instances() {
    use duroxide::providers::{InstanceFilter, PruneOptions};
    use std::sync::atomic::{AtomicU32, Ordering};

    let schema = unique_schema_name();
    let database_url = get_database_url();

    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("Failed to create provider");
    let store: Arc<dyn duroxide::providers::Provider> = Arc::new(provider);

    // Track which execution we're on
    let execution_counter = Arc::new(AtomicU32::new(0));
    let execution_counter_clone = execution_counter.clone();

    // Activity that signals execution number
    let activity_registry = ActivityRegistry::builder()
        .register(
            "SignalExecution",
            move |_ctx: ActivityContext, input: String| {
                let counter = execution_counter_clone.clone();
                async move {
                    let exec_num: u32 = input.parse().unwrap_or(0);
                    counter.store(exec_num, Ordering::SeqCst);
                    Ok(format!("exec-{exec_num}"))
                }
            },
        )
        .build();

    // Orchestration that does many ContinueAsNew cycles, pausing mid-chain
    let orchestration_registry = OrchestrationRegistry::builder()
        .register(
            "LongRunningWithContinueAsNew",
            |ctx: OrchestrationContext, count_str: String| async move {
                let count: u32 = count_str.parse().unwrap_or(0);

                // Signal which execution we're on
                ctx.schedule_activity("SignalExecution", count.to_string())
                    .into_activity()
                    .await?;

                if count == 5 {
                    // On execution 5, wait for external signal to proceed
                    // This keeps us "Running" with multiple old executions
                    let _signal = ctx.schedule_wait("proceed").into_event().await;
                }

                if count < 10 {
                    ctx.continue_as_new((count + 1).to_string()).await
                } else {
                    Ok(format!("Final: {count}"))
                }
            },
        )
        .build();

    let options = RuntimeOptions {
        dispatcher_min_poll_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let rt = runtime::Runtime::start_with_options(
        store.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
        options,
    )
    .await;

    let client = Client::new(store.clone());

    // Start the orchestration
    client
        .start_orchestration("bulk-prune-running", "LongRunningWithContinueAsNew", "0")
        .await
        .expect("Failed to start orchestration");

    // Wait for execution 5 (will be waiting for external event)
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while execution_counter.load(Ordering::SeqCst) < 5 {
        if std::time::Instant::now() > deadline {
            rt.shutdown(None).await;
            let _ = cleanup_schema(&schema).await;
            panic!("Orchestration never reached execution 5");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Give a moment for the orchestration to reach the wait state
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify we're running with multiple executions
    let info = client
        .get_instance_info("bulk-prune-running")
        .await
        .expect("Failed to get instance info");
    assert_eq!(
        info.status, "Running",
        "Should be running and waiting for event"
    );

    let executions_before = client
        .list_executions("bulk-prune-running")
        .await
        .expect("Failed to list executions");
    assert!(
        executions_before.len() >= 5,
        "Should have at least 5 executions, got {}",
        executions_before.len()
    );

    // KEY TEST: Use prune_executions_bulk with no filter
    // Previously this would skip running instances entirely
    let result = client
        .prune_executions_bulk(
            InstanceFilter {
                instance_ids: None, // All instances
                completed_before: None,
                limit: Some(100),
            },
            PruneOptions {
                keep_last: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("prune_executions_bulk failed");

    // Bulk prune should have processed our running instance
    assert!(
        result.instances_processed >= 1,
        "Should process at least 1 instance (the running one)"
    );

    // Old executions should have been deleted
    assert!(
        result.executions_deleted >= 3,
        "Should have pruned old executions, got {} deleted",
        result.executions_deleted
    );

    // Verify the current execution (running) is still there
    let info_after = client
        .get_instance_info("bulk-prune-running")
        .await
        .expect("Failed to get instance info after prune");
    assert_eq!(
        info_after.status, "Running",
        "Instance should still be running"
    );

    // Should have at most 2 executions now
    let executions_after = client
        .list_executions("bulk-prune-running")
        .await
        .expect("Failed to list executions after prune");
    assert!(
        executions_after.len() <= 2,
        "Should have at most 2 executions after prune, got {}",
        executions_after.len()
    );
    assert!(
        executions_after.contains(&info_after.current_execution_id),
        "Current execution must be preserved"
    );

    // Resume the orchestration by sending the external event
    client
        .raise_event("bulk-prune-running", "proceed", "go!")
        .await
        .expect("Failed to raise event");

    // Wait for completion
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(info) = client.get_instance_info("bulk-prune-running").await {
            if info.status == "Completed" {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            rt.shutdown(None).await;
            let _ = cleanup_schema(&schema).await;
            panic!("Orchestration never completed after event");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Verify final state
    let final_info = client
        .get_instance_info("bulk-prune-running")
        .await
        .expect("Failed to get final instance info");
    assert_eq!(final_info.status, "Completed");
    assert!(
        final_info
            .output
            .as_ref()
            .map(|o| o.contains("Final: 10"))
            .unwrap_or(false),
        "Should complete with final value after event, got: {:?}",
        final_info.output
    );

    rt.shutdown(None).await;
    let _ = cleanup_schema(&schema).await;
}
