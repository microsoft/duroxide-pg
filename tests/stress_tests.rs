use duroxide::provider_stress_tests::parallel_orchestrations::run_parallel_orchestrations_test_with_config;
use duroxide::provider_stress_tests::StressTestConfig;
use duroxide_pg_stress::PostgresStressFactory;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

#[tokio::test]
#[ignore] // Run with: cargo test --test stress_tests -- --ignored
async fn stress_test_parallel_orchestrations_light() {
    let database_url = get_database_url();
    let factory = PostgresStressFactory::new(database_url);

    let config = StressTestConfig {
        max_concurrent: 10,
        duration_secs: 5,
        tasks_per_instance: 3,
        activity_delay_ms: 5,
        orch_concurrency: 2,
        worker_concurrency: 2,
        wait_timeout_secs: 60,
    };

    let result = run_parallel_orchestrations_test_with_config(&factory, config)
        .await
        .expect("Stress test failed");

    // Assert quality requirements
    assert_eq!(
        result.success_rate(),
        100.0,
        "Expected 100% success rate, got {:.2}%",
        result.success_rate()
    );
    assert_eq!(
        result.failed_infrastructure, 0,
        "Infrastructure failures detected: {}",
        result.failed_infrastructure
    );
    assert!(
        result.orch_throughput > 1.0,
        "Throughput too low: {:.2} orch/sec",
        result.orch_throughput
    );
}

#[tokio::test]
#[ignore]
async fn stress_test_parallel_orchestrations_standard() {
    let database_url = get_database_url();
    let factory = PostgresStressFactory::new(database_url);

    let config = StressTestConfig {
        max_concurrent: 20,
        duration_secs: 10,
        tasks_per_instance: 5,
        activity_delay_ms: 10,
        orch_concurrency: 2,
        worker_concurrency: 2,
        wait_timeout_secs: 60,
    };

    let result = run_parallel_orchestrations_test_with_config(&factory, config)
        .await
        .expect("Stress test failed");

    assert_eq!(result.success_rate(), 100.0);
    assert_eq!(result.failed_infrastructure, 0);
}

#[tokio::test]
#[ignore]
async fn stress_test_high_concurrency() {
    let database_url = get_database_url();
    let factory = PostgresStressFactory::new(database_url);

    let config = StressTestConfig {
        max_concurrent: 50,
        duration_secs: 30,
        tasks_per_instance: 10,
        activity_delay_ms: 10,
        orch_concurrency: 4,
        worker_concurrency: 4,
        wait_timeout_secs: 60,
    };

    let result = run_parallel_orchestrations_test_with_config(&factory, config)
        .await
        .expect("Stress test failed");

    assert_eq!(result.success_rate(), 100.0);
    assert_eq!(result.failed_infrastructure, 0);

    // Validate throughput meets minimum requirements
    assert!(
        result.orch_throughput > 2.0,
        "Throughput below minimum: {:.2} orch/sec",
        result.orch_throughput
    );
}

#[tokio::test]
#[ignore]
async fn stress_test_connection_pool_limits() {
    let database_url = get_database_url();

    // Override pool size to small value
    std::env::set_var("DUROXIDE_PG_POOL_MAX", "5");

    let factory = PostgresStressFactory::new(database_url);

    let config = StressTestConfig {
        max_concurrent: 30, // More than pool size
        duration_secs: 10,
        tasks_per_instance: 5,
        activity_delay_ms: 10,
        orch_concurrency: 4,
        worker_concurrency: 4,
        wait_timeout_secs: 60,
    };

    let result = run_parallel_orchestrations_test_with_config(&factory, config)
        .await
        .expect("Stress test failed");

    // Should still succeed despite pool pressure
    assert_eq!(result.success_rate(), 100.0);
}

#[tokio::test]
#[ignore]
async fn stress_test_long_duration_stability() {
    let database_url = get_database_url();
    let factory = PostgresStressFactory::new(database_url);

    let config = StressTestConfig {
        max_concurrent: 20,
        duration_secs: 300, // 5 minutes
        tasks_per_instance: 5,
        activity_delay_ms: 10,
        orch_concurrency: 2,
        worker_concurrency: 2,
        wait_timeout_secs: 60,
    };

    let result = run_parallel_orchestrations_test_with_config(&factory, config)
        .await
        .expect("Stress test failed");

    assert_eq!(result.success_rate(), 100.0);

    // Validate sustained throughput
    assert!(
        result.orch_throughput > 1.5,
        "Throughput degraded over time: {:.2} orch/sec",
        result.orch_throughput
    );
}
