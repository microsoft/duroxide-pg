use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Once,
};

use duroxide::provider_validations::{
    run_atomicity_tests, run_error_handling_tests, run_instance_locking_tests,
    run_lock_expiration_tests, run_management_tests, run_multi_execution_tests,
    run_queue_semantics_tests, ProviderFactory,
};
use duroxide::providers::Provider;
use duroxide_pg::PostgresProvider;
use sqlx::{postgres::PgPoolOptions, Executor};
use tracing_subscriber::EnvFilter;

static VALIDATION_SCHEMA_COUNTER: AtomicU64 = AtomicU64::new(0);
static INIT_LOGGING: Once = Once::new();

fn init_test_logging() {
    INIT_LOGGING.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .init();
    });
}

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for provider validation tests")
}

fn next_schema_name() -> String {
    let counter = VALIDATION_SCHEMA_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("validation_test_{}", counter)
}

async fn reset_schema(database_url: &str, schema_name: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("Failed to connect to database for schema reset");

    if schema_name == "public" {
        let tables = [
            "instances",
            "executions",
            "history",
            "orchestrator_queue",
            "worker_queue",
            "instance_locks",
        ];

        for table in tables {
            let qualified = format!("public.{}", table);
            pool.execute(format!("DROP TABLE IF EXISTS {} CASCADE", qualified).as_str())
                .await
                .expect("Failed to drop table in public schema");
        }
    } else {
        pool.execute(format!("DROP SCHEMA IF EXISTS {} CASCADE", schema_name).as_str())
            .await
            .expect("Failed to drop validation schema");
    }
}

pub struct PostgresProviderFactory {
    database_url: String,
    lock_timeout_ms: u64,
}

impl PostgresProviderFactory {
    pub fn new() -> Self {
        init_test_logging();
        Self {
            database_url: get_database_url(),
            lock_timeout_ms: 5_000,
        }
    }

    async fn create_postgres_provider(&self) -> Arc<PostgresProvider> {
        let schema_name = next_schema_name();
        reset_schema(&self.database_url, &schema_name).await;

        let provider = PostgresProvider::new_with_schema_and_timeout(
            &self.database_url,
            Some(&schema_name),
            self.lock_timeout_ms,
        )
        .await
        .expect("Failed to create Postgres provider for validation tests");

        Arc::new(provider)
    }
}

#[async_trait::async_trait]
impl ProviderFactory for PostgresProviderFactory {
    async fn create_provider(&self) -> Arc<dyn Provider> {
        self.create_postgres_provider().await as Arc<dyn Provider>
    }

    fn lock_timeout_ms(&self) -> u64 {
        self.lock_timeout_ms
    }
}

#[tokio::test]
async fn test_postgres_provider_atomicity() {
    let factory = PostgresProviderFactory::new();
    run_atomicity_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_error_handling() {
    let factory = PostgresProviderFactory::new();
    run_error_handling_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_instance_locking() {
    let factory = PostgresProviderFactory::new();
    run_instance_locking_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_lock_expiration() {
    let factory = PostgresProviderFactory::new();
    run_lock_expiration_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_multi_execution() {
    let factory = PostgresProviderFactory::new();
    run_multi_execution_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_queue_semantics() {
    let factory = PostgresProviderFactory::new();
    run_queue_semantics_tests(&factory).await;
}

#[tokio::test]
async fn test_postgres_provider_management() {
    let factory = PostgresProviderFactory::new();
    run_management_tests(&factory).await;
}
