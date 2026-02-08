use std::sync::{Arc, Once};

use duroxide::provider_validation::{
    atomicity, cancellation, capability_filtering, error_handling, instance_creation,
    instance_locking, lock_expiration, management, multi_execution, queue_semantics, deletion,
    prune, bulk_deletion,
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
    format!("validation_test_{suffix}")
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
            let qualified = format!("public.{table}");
            pool.execute(format!("DROP TABLE IF EXISTS {qualified} CASCADE").as_str())
                .await
                .expect("Failed to drop table in public schema");
        }
    } else {
        pool.execute(format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE").as_str())
            .await
            .expect("Failed to drop validation schema");
    }
}

pub struct PostgresProviderFactory {
    database_url: String,
    lock_timeout_ms: u64,
    current_schema_name: std::sync::Mutex<Option<String>>,
}

impl Default for PostgresProviderFactory {
    fn default() -> Self {
        Self::new()
    }
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

        let provider = PostgresProvider::new_with_schema(&self.database_url, Some(&schema_name))
            .await
            .expect("Failed to create Postgres provider for validation tests");

        Arc::new(provider)
    }

    async fn cleanup_schema(&self) {
        let schema_name = self.current_schema_name.lock().unwrap().take();
        if let Some(schema_name) = schema_name {
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

    async fn corrupt_instance_history(&self, instance: &str) {
        let schema = self.current_schema_name.lock().unwrap().clone().unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await
            .expect("Failed to connect for corruption");

        let query = format!(
            "UPDATE {schema}.history SET event_data = '{{\"garbage\": true}}' WHERE instance_id = $1"
        );
        sqlx::query(&query)
            .bind(instance)
            .execute(&pool)
            .await
            .expect("Failed to corrupt history");
    }

    async fn get_max_attempt_count(&self, instance: &str) -> u32 {
        let schema = self.current_schema_name.lock().unwrap().clone().unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await
            .expect("Failed to connect for attempt count");

        let query = format!(
            "SELECT COALESCE(MAX(attempt_count), 0) FROM {schema}.orchestrator_queue WHERE instance_id = $1"
        );
        let count: (i32,) = sqlx::query_as(&query)
            .bind(instance)
            .fetch_one(&pool)
            .await
            .expect("Failed to get attempt count");
        count.0 as u32
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
    provider_validation_test!(lock_expiration::test_abandon_work_item_releases_lock);
    provider_validation_test!(lock_expiration::test_abandon_work_item_with_delay);
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
    provider_validation_test!(queue_semantics::test_worker_item_immediate_visibility);
    provider_validation_test!(queue_semantics::test_worker_delayed_visibility_skips_future_items);
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

mod poison_message_tests {
    use super::*;
    use duroxide::provider_validation::poison_message;

    provider_validation_test!(poison_message::orchestration_attempt_count_starts_at_one);
    provider_validation_test!(poison_message::orchestration_attempt_count_increments_on_refetch);
    provider_validation_test!(poison_message::worker_attempt_count_starts_at_one);
    provider_validation_test!(poison_message::worker_attempt_count_increments_on_lock_expiry);
    provider_validation_test!(poison_message::attempt_count_is_per_message);
    provider_validation_test!(poison_message::abandon_work_item_ignore_attempt_decrements);
    provider_validation_test!(poison_message::abandon_orchestration_item_ignore_attempt_decrements);
    provider_validation_test!(poison_message::ignore_attempt_never_goes_negative);
    provider_validation_test!(poison_message::max_attempt_count_across_message_batch);
}

mod long_polling_tests {
    use super::*;
    use duroxide::provider_validation::long_polling;

    // PostgreSQL provider uses short polling (returns immediately when no work)
    //
    // STOPGAP: These tests require warmup queries because duroxide's short-poll
    // validation has a hardcoded 100ms threshold (see duroxide issue #51).
    // PostgreSQL stored procedure calls take ~70-80ms over network, and first
    // queries have additional latency from query plan compilation.
    //
    // Once duroxide #51 is fixed to make the threshold configurable, remove the
    // warmup queries and use the standard provider_validation_test! macro.
    // See: https://github.com/affandar/duroxide/issues/51
    #[tokio::test]
    async fn test_short_poll_returns_immediately() {
        let factory = PostgresProviderFactory::new();
        let provider = factory.create_provider().await;
        // STOPGAP for duroxide #51: Warm up connection pool and query plan cache
        // Remove once short_poll_threshold() is configurable via ProviderFactory
        let _ = provider
            .fetch_orchestration_item(
                std::time::Duration::from_secs(1),
                std::time::Duration::ZERO,
                None,
            )
            .await;
        let _ = provider
            .fetch_orchestration_item(
                std::time::Duration::from_secs(1),
                std::time::Duration::ZERO,
                None,
            )
            .await;
        long_polling::test_short_poll_returns_immediately(&*provider).await;
        factory.cleanup_schema().await;
    }

    #[tokio::test]
    async fn test_short_poll_work_item_returns_immediately() {
        let factory = PostgresProviderFactory::new();
        let provider = factory.create_provider().await;
        // STOPGAP for duroxide #51: Warm up connection pool and query plan cache
        // Remove once short_poll_threshold() is configurable via ProviderFactory
        let _ = provider
            .fetch_work_item(std::time::Duration::from_secs(1), std::time::Duration::ZERO)
            .await;
        let _ = provider
            .fetch_work_item(std::time::Duration::from_secs(1), std::time::Duration::ZERO)
            .await;
        long_polling::test_short_poll_work_item_returns_immediately(&*provider).await;
        factory.cleanup_schema().await;
    }

    #[tokio::test]
    async fn test_fetch_respects_timeout_upper_bound() {
        let factory = PostgresProviderFactory::new();
        let provider = factory.create_provider().await;
        long_polling::test_fetch_respects_timeout_upper_bound(&*provider).await;
        factory.cleanup_schema().await;
    }
}

mod cancellation_tests {
    use super::*;

    provider_validation_test!(cancellation::test_fetch_returns_running_state_for_active_orchestration);
    provider_validation_test!(cancellation::test_fetch_returns_terminal_state_when_orchestration_completed);
    provider_validation_test!(cancellation::test_fetch_returns_terminal_state_when_orchestration_failed);
    provider_validation_test!(cancellation::test_fetch_returns_terminal_state_when_orchestration_continued_as_new);
    provider_validation_test!(cancellation::test_fetch_returns_missing_state_when_instance_deleted);
    provider_validation_test!(cancellation::test_renew_returns_running_when_orchestration_active);
    provider_validation_test!(cancellation::test_renew_returns_terminal_when_orchestration_completed);
    provider_validation_test!(cancellation::test_renew_returns_missing_when_instance_deleted);
    provider_validation_test!(cancellation::test_ack_work_item_none_deletes_without_enqueue);
    // Lock-stealing tests
    provider_validation_test!(cancellation::test_cancelled_activities_deleted_from_worker_queue);
    provider_validation_test!(cancellation::test_ack_work_item_fails_when_entry_deleted);
    provider_validation_test!(cancellation::test_renew_fails_when_entry_deleted);
    provider_validation_test!(cancellation::test_cancelling_nonexistent_activities_is_idempotent);
    provider_validation_test!(cancellation::test_batch_cancellation_deletes_multiple_activities);
    provider_validation_test!(cancellation::test_same_activity_in_worker_items_and_cancelled_is_noop);
}

mod deletion_tests {
    use super::*;

    provider_validation_test!(deletion::test_delete_terminal_instances);
    provider_validation_test!(deletion::test_delete_running_rejected_force_succeeds);
    provider_validation_test!(deletion::test_delete_nonexistent_instance);
    provider_validation_test!(deletion::test_delete_cleans_queues_and_locks);
    provider_validation_test!(deletion::test_cascade_delete_hierarchy);
    provider_validation_test!(deletion::test_list_children);
    provider_validation_test!(deletion::test_delete_get_parent_id);
    provider_validation_test!(deletion::test_delete_get_instance_tree);
    provider_validation_test!(deletion::test_delete_instances_atomic);
    provider_validation_test!(deletion::test_delete_instances_atomic_force);
    provider_validation_test!(deletion::test_delete_instances_atomic_orphan_detection);
    provider_validation_test!(deletion::test_force_delete_prevents_ack_recreation);
    provider_validation_test!(deletion::test_stale_activity_after_delete_recreate);
}

mod prune_tests {
    use super::*;

    provider_validation_test!(prune::test_prune_options_combinations);
    provider_validation_test!(prune::test_prune_safety);
    provider_validation_test!(prune::test_prune_bulk);
}

mod bulk_deletion_tests {
    use super::*;

    provider_validation_test!(bulk_deletion::test_delete_instance_bulk_filter_combinations);
    provider_validation_test!(bulk_deletion::test_delete_instance_bulk_safety_and_limits);
    provider_validation_test!(bulk_deletion::test_delete_instance_bulk_completed_before_filter);
    provider_validation_test!(bulk_deletion::test_delete_instance_bulk_cascades_to_children);
}

mod capability_filtering_tests {
    use super::*;

    provider_validation_test!(capability_filtering::test_fetch_with_filter_none_returns_any_item);
    provider_validation_test!(capability_filtering::test_fetch_with_compatible_filter_returns_item);
    provider_validation_test!(capability_filtering::test_fetch_with_incompatible_filter_skips_item);
    provider_validation_test!(capability_filtering::test_fetch_filter_skips_incompatible_selects_compatible);
    provider_validation_test!(capability_filtering::test_fetch_filter_does_not_lock_skipped_instances);
    provider_validation_test!(capability_filtering::test_fetch_filter_null_pinned_version_always_compatible);
    provider_validation_test!(capability_filtering::test_fetch_filter_boundary_versions);
    provider_validation_test!(capability_filtering::test_pinned_version_stored_via_ack_metadata);
    provider_validation_test!(capability_filtering::test_pinned_version_immutable_across_ack_cycles);
    provider_validation_test!(capability_filtering::test_continue_as_new_execution_gets_own_pinned_version);
    provider_validation_test!(capability_filtering::test_filter_with_empty_supported_versions_returns_nothing);
    provider_validation_test!(capability_filtering::test_concurrent_filtered_fetch_no_double_lock);
    provider_validation_test!(capability_filtering::test_ack_stores_pinned_version_via_metadata_update);
    provider_validation_test!(capability_filtering::test_provider_updates_pinned_version_when_told);
    provider_validation_test!(capability_filtering::test_fetch_corrupted_history_filtered_vs_unfiltered);
    provider_validation_test!(capability_filtering::test_fetch_deserialization_error_increments_attempt_count);
    provider_validation_test!(capability_filtering::test_fetch_deserialization_error_eventually_reaches_poison);
    provider_validation_test!(capability_filtering::test_fetch_filter_applied_before_history_deserialization);
    provider_validation_test!(capability_filtering::test_fetch_single_range_only_uses_first_range);
    provider_validation_test!(capability_filtering::test_ack_appends_event_to_corrupted_history);
}
