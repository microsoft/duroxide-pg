use std::sync::{Arc, Once};

use duroxide::provider_validation::{
    atomicity, error_handling, instance_creation, instance_locking, lock_expiration, management,
    multi_execution, queue_semantics,
};
use duroxide::provider_validations::ProviderFactory;
use duroxide::providers::Provider;
use duroxide_pg::PostgresProvider;
use sqlx::{postgres::PgPoolOptions, Executor};
use tracing_subscriber::EnvFilter;

static INIT_LOGGING: Once = Once::new();

fn init_test_logging() {
    INIT_LOGGING.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));

        // Try to initialize, but ignore if already initialized (e.g., by duroxide runtime)
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
    });
}

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for provider validation tests")
}

fn next_schema_name() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..]; // Last 8 characters
    format!("validation_test_{}", suffix)
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
    current_schema_name: std::sync::Mutex<Option<String>>,
}

impl PostgresProviderFactory {
    pub fn new() -> Self {
        init_test_logging();
        Self {
            database_url: get_database_url(),
            lock_timeout_ms: 30_000, // 30 seconds - must match hardcoded timeout in validation tests
            current_schema_name: std::sync::Mutex::new(None),
        }
    }

    async fn create_postgres_provider(&self) -> Arc<PostgresProvider> {
        let schema_name = next_schema_name();
        reset_schema(&self.database_url, &schema_name).await;

        // Store schema name for cleanup
        *self.current_schema_name.lock().unwrap() = Some(schema_name.clone());

        let provider = PostgresProvider::new_with_schema(
            &self.database_url,
            Some(&schema_name),
        )
        .await
        .expect("Failed to create Postgres provider for validation tests");

        Arc::new(provider)
    }

    async fn cleanup_schema(&self) {
        if let Some(schema_name) = self.current_schema_name.lock().unwrap().take() {
            reset_schema(&self.database_url, &schema_name).await;
        }
    }
}

#[async_trait::async_trait]
impl ProviderFactory for PostgresProviderFactory {
    async fn create_provider(&self) -> Arc<dyn Provider> {
        self.create_postgres_provider().await as Arc<dyn Provider>
    }

    fn lock_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.lock_timeout_ms)
    }
}

macro_rules! provider_validation_test {
    ($module:ident :: $test_fn:ident) => {
        #[tokio::test]
        async fn $test_fn() {
            let factory = PostgresProviderFactory::new();
            $module::$test_fn(&factory).await;
            factory.cleanup_schema().await;
        }
    };
}

mod atomicity_tests {
    use super::*;

    provider_validation_test!(atomicity::test_atomicity_failure_rollback);
    provider_validation_test!(atomicity::test_multi_operation_atomic_ack);
    provider_validation_test!(atomicity::test_lock_released_only_on_successful_ack);
    provider_validation_test!(atomicity::test_concurrent_ack_prevention);
}

mod error_handling_tests {
    use super::*;

    provider_validation_test!(error_handling::test_invalid_lock_token_on_ack);
    provider_validation_test!(error_handling::test_duplicate_event_id_rejection);
    provider_validation_test!(error_handling::test_missing_instance_metadata);
    provider_validation_test!(error_handling::test_corrupted_serialization_data);
    provider_validation_test!(error_handling::test_lock_expiration_during_ack);
}

mod instance_creation_tests {
    use super::*;

    provider_validation_test!(instance_creation::test_instance_creation_via_metadata);
    provider_validation_test!(instance_creation::test_no_instance_creation_on_enqueue);
    provider_validation_test!(instance_creation::test_null_version_handling);
    provider_validation_test!(instance_creation::test_sub_orchestration_instance_creation);
}

mod instance_locking_tests {
    use super::*;

    provider_validation_test!(instance_locking::test_exclusive_instance_lock);
    provider_validation_test!(instance_locking::test_lock_token_uniqueness);
    provider_validation_test!(instance_locking::test_invalid_lock_token_rejection);
    provider_validation_test!(instance_locking::test_concurrent_instance_fetching);
    provider_validation_test!(instance_locking::test_completions_arriving_during_lock_blocked);
    provider_validation_test!(instance_locking::test_cross_instance_lock_isolation);
    provider_validation_test!(instance_locking::test_message_tagging_during_lock);
    provider_validation_test!(instance_locking::test_ack_only_affects_locked_messages);
    provider_validation_test!(instance_locking::test_multi_threaded_lock_contention);
    provider_validation_test!(instance_locking::test_multi_threaded_no_duplicate_processing);
    provider_validation_test!(instance_locking::test_multi_threaded_lock_expiration_recovery);
}

mod lock_expiration_tests {
    use super::*;

    provider_validation_test!(lock_expiration::test_lock_expires_after_timeout);
    provider_validation_test!(lock_expiration::test_abandon_releases_lock_immediately);
    provider_validation_test!(lock_expiration::test_lock_renewal_on_ack);
    provider_validation_test!(lock_expiration::test_concurrent_lock_attempts_respect_expiration);
    provider_validation_test!(lock_expiration::test_worker_lock_renewal_success);
    provider_validation_test!(lock_expiration::test_worker_lock_renewal_invalid_token);
    provider_validation_test!(lock_expiration::test_worker_lock_renewal_after_expiration);
    provider_validation_test!(lock_expiration::test_worker_lock_renewal_extends_timeout);
    provider_validation_test!(lock_expiration::test_worker_lock_renewal_after_ack);
}

mod multi_execution_tests {
    use super::*;

    provider_validation_test!(multi_execution::test_execution_isolation);
    provider_validation_test!(multi_execution::test_latest_execution_detection);
    provider_validation_test!(multi_execution::test_execution_id_sequencing);
    provider_validation_test!(multi_execution::test_continue_as_new_creates_new_execution);
    provider_validation_test!(multi_execution::test_execution_history_persistence);
}

mod queue_semantics_tests {
    use super::*;

    provider_validation_test!(queue_semantics::test_worker_queue_fifo_ordering);
    provider_validation_test!(queue_semantics::test_worker_peek_lock_semantics);
    provider_validation_test!(queue_semantics::test_worker_ack_atomicity);
    provider_validation_test!(queue_semantics::test_timer_delayed_visibility);
    provider_validation_test!(queue_semantics::test_lost_lock_token_handling);
}

mod management_tests {
    use super::*;

    provider_validation_test!(management::test_list_instances);
    provider_validation_test!(management::test_list_instances_by_status);
    provider_validation_test!(management::test_list_executions);
    provider_validation_test!(management::test_get_instance_info);
    provider_validation_test!(management::test_get_execution_info);
    provider_validation_test!(management::test_get_system_metrics);
    provider_validation_test!(management::test_get_queue_depths);
}
