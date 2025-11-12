//! PostgreSQL Provider Stress Tests for Duroxide
//!
//! This library provides PostgreSQL-specific stress test implementations for Duroxide,
//! using the provider stress test infrastructure from the main crate.
//!
//! # Quick Start
//!
//! Run the stress test binary:
//!
//! ```bash
//! cargo run --release --package duroxide-pg-stress --bin pg-stress [DURATION]
//! ```

use duroxide::provider_stress_tests::parallel_orchestrations::{
    run_parallel_orchestrations_test_with_config, ProviderStressFactory,
};
use duroxide::provider_stress_tests::StressTestConfig;
use duroxide::providers::Provider;
use duroxide_pg::PostgresProvider;
use std::sync::Arc;
use tracing::info;

// Re-export the stress test infrastructure for convenience
pub use duroxide::provider_stress_tests::{StressTestConfig as Config, StressTestResult};

/// Factory for creating PostgreSQL providers for stress testing
pub struct PostgresStressFactory {
    database_url: String,
    use_unique_schemas: bool,
}

impl PostgresStressFactory {
    pub fn new(database_url: String) -> Self {
        Self {
            database_url,
            use_unique_schemas: true,
        }
    }

    #[allow(dead_code)]
    pub fn with_shared_schema(mut self) -> Self {
        self.use_unique_schemas = false;
        self
    }
}

#[async_trait::async_trait]
impl ProviderStressFactory for PostgresStressFactory {
    async fn create_provider(&self) -> Arc<dyn Provider> {
        let schema_name = if self.use_unique_schemas {
            let guid = uuid::Uuid::new_v4().to_string();
            let suffix = &guid[guid.len() - 8..];
            format!("stress_test_{}", suffix)
        } else {
            "stress_test_shared".to_string()
        };

        info!("Creating PostgreSQL provider with schema: {}", schema_name);

        Arc::new(
            PostgresProvider::new_with_schema_and_timeout(
                &self.database_url,
                Some(&schema_name),
                30_000, // 30 second lock timeout
            )
            .await
            .expect("Failed to create PostgreSQL provider for stress test"),
        )
    }
}

/// Extract hostname from PostgreSQL connection URL
fn extract_hostname(url: &str) -> String {
    // Parse URL to extract hostname
    // Format: postgresql://user:pass@hostname:port/db
    if let Some(at_pos) = url.find('@') {
        let after_at = &url[at_pos + 1..];
        if let Some(colon_pos) = after_at.find(':') {
            let hostname = &after_at[..colon_pos];
            // Get first subdomain (e.g., "localhost" or "duroxide-pg" from "duroxide-pg.postgres.database.azure.com")
            if let Some(dot_pos) = hostname.find('.') {
                return hostname[..dot_pos].to_string();
            }
            return hostname.to_string();
        }
    }
    "unknown".to_string()
}

/// Run a single stress test with custom configuration
pub async fn run_single_test(
    database_url: String,
    duration_secs: u64,
    orch_conc: usize,
    worker_conc: usize,
    idle_sleep_ms: u64,
) -> Result<StressTestResult, Box<dyn std::error::Error>> {
    let factory = PostgresStressFactory::new(database_url);
    
    let config = StressTestConfig {
        max_concurrent: 20,
        duration_secs,
        tasks_per_instance: 5,
        activity_delay_ms: 10,
        orch_concurrency: orch_conc,
        worker_concurrency: worker_conc,
    };
    
    // We need to create our own runtime with custom options
    // since the stress test framework hardcodes dispatcher_idle_sleep_ms
    let provider = factory.create_provider().await;
    
    use duroxide::runtime::registry::ActivityRegistry;
    use duroxide::runtime::RuntimeOptions;
    use duroxide::OrchestrationRegistry;
    use duroxide::{ActivityContext, OrchestrationContext};
    
    let activity_registry = ActivityRegistry::builder()
        .register("StressTask", |_ctx: ActivityContext, input: String| async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(format!("done: {}", input))
        })
        .build();
    
    let orchestration = |ctx: OrchestrationContext, input: String| async move {
        let task_count: usize = serde_json::from_str::<serde_json::Value>(&input)
            .ok()
            .and_then(|v| v.get("task_count").and_then(|tc| tc.as_u64()).map(|n| n as usize))
            .unwrap_or(5);
        
        let mut handles = Vec::new();
        for i in 0..task_count {
            handles.push(ctx.schedule_activity("StressTask", format!("task-{}", i)));
        }
        for handle in handles {
            handle.into_activity().await?;
        }
        Ok("done".to_string())
    };
    
    let orchestration_registry = OrchestrationRegistry::builder()
        .register("FanoutWorkflow", orchestration)
        .build();
    
    // Use custom runtime options with specified idle sleep
    let options = RuntimeOptions {
        dispatcher_idle_sleep_ms: idle_sleep_ms,
        orchestration_concurrency: orch_conc,
        worker_concurrency: worker_conc,
        ..Default::default()
    };
    
    let rt = duroxide::runtime::Runtime::start_with_options(
        provider.clone(),
        Arc::new(activity_registry),
        orchestration_registry,
        options,
    ).await;
    
    // Run the test
    let client = Arc::new(duroxide::Client::new(provider.clone()));
    let launched = Arc::new(tokio::sync::Mutex::new(0_usize));
    let completed = Arc::new(tokio::sync::Mutex::new(0_usize));
    let start_time = std::time::Instant::now();
    let end_time = start_time + std::time::Duration::from_secs(duration_secs);
    
    let mut instance_id = 0_usize;
    
    loop {
        if std::time::Instant::now() >= end_time {
            break;
        }
        
        let current_launched = *launched.lock().await;
        if current_launched >= config.max_concurrent {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        }
        
        instance_id += 1;
        let instance = format!("bench-{}", instance_id);
        *launched.lock().await += 1;
        
        let client_clone = Arc::clone(&client);
        let completed_clone = Arc::clone(&completed);
        
        tokio::spawn(async move {
            let input = serde_json::json!({"task_count": 5}).to_string();
            if client_clone.start_orchestration(&instance, "FanoutWorkflow", input).await.is_ok() {
                if let Ok(duroxide::OrchestrationStatus::Completed { .. }) = client_clone
                    .wait_for_orchestration(&instance, std::time::Duration::from_secs(60))
                    .await
                {
                    *completed_clone.lock().await += 1;
                }
            }
        });
        
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    
    // Wait for stragglers
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    
    let total_launched = *launched.lock().await;
    let total_completed = *completed.lock().await;
    let total_time = start_time.elapsed();
    
    rt.shutdown(None).await;
    
    Ok(StressTestResult {
        launched: total_launched,
        completed: total_completed,
        failed: total_launched - total_completed,
        failed_infrastructure: 0,
        failed_configuration: 0,
        failed_application: 0,
        total_time,
        orch_throughput: total_completed as f64 / total_time.as_secs_f64(),
        activity_throughput: (total_completed * config.tasks_per_instance) as f64 / total_time.as_secs_f64(),
        avg_latency_ms: if total_completed > 0 {
            total_time.as_millis() as f64 / total_completed as f64
        } else {
            0.0
        },
    })
}

/// Run the parallel orchestrations stress test suite for PostgreSQL
pub async fn run_test_suite(
    database_url: String,
    duration_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let hostname = extract_hostname(&database_url);
    
    info!("=== Duroxide PostgreSQL Stress Test Suite ===");
    info!("Database: {}", mask_password(&database_url));
    info!("Hostname: {}", hostname);
    info!("Duration: {} seconds per test", duration_secs);

    // Use 8:8 configuration as requested
    let concurrency_combos = vec![(8, 8)];
    
    let mut results = Vec::new();

    let factory = PostgresStressFactory::new(database_url);

    for (orch_conc, worker_conc) in &concurrency_combos {
        let config = StressTestConfig {
            max_concurrent: 20,
            duration_secs,
            tasks_per_instance: 5,
            activity_delay_ms: 10,
            orch_concurrency: *orch_conc,
            worker_concurrency: *worker_conc,
        };

        info!(
            "\n--- Running PostgreSQL stress test (orch={}, worker={}) ---",
            orch_conc, worker_conc
        );

        let result = run_parallel_orchestrations_test_with_config(&factory, config).await?;

        info!(
            "Completed: {}, Failed: {}, Success Rate: {:.2}%",
            result.completed,
            result.failed,
            result.success_rate()
        );
        info!(
            "Throughput: {:.2} orch/sec, {:.2} activities/sec",
            result.orch_throughput, result.activity_throughput
        );
        info!("Average latency: {:.2}ms", result.avg_latency_ms);

        results.push((
            "PostgreSQL".to_string(),
            format!("{}:{}", orch_conc, worker_conc),
            result,
        ));
    }

    // Print comparison table
    info!("\n=== Stress Test Results Summary ===\n");
    duroxide::provider_stress_tests::print_comparison_table(&results);

    // Validate all tests passed
    for (provider, config, result) in &results {
        if result.success_rate() < 100.0 {
            return Err(format!(
                "Stress test {} {} had failures: {:.2}% success rate",
                provider,
                config,
                result.success_rate()
            )
            .into());
        }
    }

    info!("\n✅ All stress tests passed!");
    
    // Return hostname for result tracking
    Ok(())
}

/// Get the results filename based on database hostname
pub fn get_results_filename(database_url: &str) -> String {
    let hostname = extract_hostname(database_url);
    format!("stress-test-results-{}.md", hostname)
}

fn mask_password(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(colon_pos) = url[..at_pos].rfind(':') {
            let mut masked = url.to_string();
            masked.replace_range(colon_pos + 1..at_pos, "***");
            return masked;
        }
    }
    url.to_string()
}

