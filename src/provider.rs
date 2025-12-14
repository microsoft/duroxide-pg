use anyhow::Result;
use chrono::{TimeZone, Utc};
use duroxide::providers::{
    ExecutionInfo, ExecutionMetadata, InstanceInfo, OrchestrationItem, Provider, ProviderAdmin,
    ProviderError, QueueDepths, SystemMetrics, WorkItem,
};
use duroxide::Event;
use sqlx::{postgres::PgPoolOptions, Error as SqlxError, PgPool};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{debug, error, instrument, warn};

use crate::migrations::MigrationRunner;

/// PostgreSQL-based provider for Duroxide durable orchestrations.
///
/// Implements the [`Provider`] and [`ProviderAdmin`] traits from Duroxide,
/// storing orchestration state, history, and work queues in PostgreSQL.
///
/// # Example
///
/// ```rust,no_run
/// use duroxide_pg::PostgresProvider;
///
/// # async fn example() -> anyhow::Result<()> {
/// // Connect using DATABASE_URL or explicit connection string
/// let provider = PostgresProvider::new("postgres://localhost/mydb").await?;
///
/// // Or use a custom schema for isolation
/// let provider = PostgresProvider::new_with_schema(
///     "postgres://localhost/mydb",
///     Some("my_app"),
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub struct PostgresProvider {
    pool: Arc<PgPool>,
    schema_name: String,
}

impl PostgresProvider {
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_schema(database_url, None).await
    }

    pub async fn new_with_schema(database_url: &str, schema_name: Option<&str>) -> Result<Self> {
        let max_connections = std::env::var("DUROXIDE_PG_POOL_MAX")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(10);

        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(database_url)
            .await?;

        let schema_name = schema_name.unwrap_or("public").to_string();

        let provider = Self {
            pool: Arc::new(pool),
            schema_name: schema_name.clone(),
        };

        // Run migrations to initialize schema
        let migration_runner = MigrationRunner::new(provider.pool.clone(), schema_name.clone());
        migration_runner.migrate().await?;

        Ok(provider)
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    pub async fn initialize_schema(&self) -> Result<()> {
        // Schema initialization is now handled by migrations
        // This method is kept for backward compatibility but delegates to migrations
        let migration_runner = MigrationRunner::new(self.pool.clone(), self.schema_name.clone());
        migration_runner.migrate().await?;
        Ok(())
    }

    /// Get current timestamp in milliseconds (Unix epoch)
    fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// Get schema-qualified table name
    fn table_name(&self, table: &str) -> String {
        format!("{}.{}", self.schema_name, table)
    }

    /// Get the database pool (for testing)
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Get the schema name (for testing)
    pub fn schema_name(&self) -> &str {
        &self.schema_name
    }

    /// Convert sqlx::Error to ProviderError with proper classification
    fn sqlx_to_provider_error(operation: &str, e: SqlxError) -> ProviderError {
        match e {
            SqlxError::Database(ref db_err) => {
                // PostgreSQL error codes
                let code_opt = db_err.code();
                let code = code_opt.as_deref();
                if code == Some("40P01") {
                    // Deadlock detected
                    ProviderError::retryable(operation, format!("Deadlock detected: {e}"))
                } else if code == Some("40001") {
                    // Serialization failure - permanent error (transaction conflict, not transient)
                    ProviderError::permanent(operation, format!("Serialization failure: {e}"))
                } else if code == Some("23505") {
                    // Unique constraint violation (duplicate event)
                    ProviderError::permanent(operation, format!("Duplicate detected: {e}"))
                } else if code == Some("23503") {
                    // Foreign key constraint violation
                    ProviderError::permanent(operation, format!("Foreign key violation: {e}"))
                } else {
                    ProviderError::permanent(operation, format!("Database error: {e}"))
                }
            }
            SqlxError::PoolClosed | SqlxError::PoolTimedOut => {
                ProviderError::retryable(operation, format!("Connection pool error: {e}"))
            }
            SqlxError::Io(_) => ProviderError::retryable(operation, format!("I/O error: {e}")),
            _ => ProviderError::permanent(operation, format!("Unexpected error: {e}")),
        }
    }

    /// Clean up schema after tests (drops all tables and optionally the schema)
    ///
    /// **SAFETY**: Never drops the "public" schema itself, only tables within it.
    /// Only drops the schema if it's a custom schema (not "public").
    pub async fn cleanup_schema(&self) -> Result<()> {
        // Call the stored procedure to drop all tables
        sqlx::query(&format!("SELECT {}.cleanup_schema()", self.schema_name))
            .execute(&*self.pool)
            .await?;

        // SAFETY: Never drop the "public" schema - it's a PostgreSQL system schema
        // Only drop custom schemas created for testing
        if self.schema_name != "public" {
            sqlx::query(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                self.schema_name
            ))
            .execute(&*self.pool)
            .await?;
        } else {
            // Explicit safeguard: we only drop tables from public schema, never the schema itself
            // This ensures we don't accidentally drop the default PostgreSQL schema
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for PostgresProvider {
    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn fetch_orchestration_item(
        &self,
        lock_timeout: Duration,
        _poll_timeout: Duration,
    ) -> Result<Option<(OrchestrationItem, String, u32)>, ProviderError> {
        let start = std::time::Instant::now();

        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_MS: u64 = 50;

        // Convert Duration to milliseconds
        let lock_timeout_ms = lock_timeout.as_millis() as i64;
        let mut _last_error: Option<ProviderError> = None;

        for attempt in 0..=MAX_RETRIES {
            let now_ms = Self::now_millis();

            let result: Result<
                Option<(
                    String,
                    String,
                    String,
                    i64,
                    serde_json::Value,
                    serde_json::Value,
                    String,
                    i32,
                )>,
                SqlxError,
            > = sqlx::query_as(&format!(
                "SELECT * FROM {}.fetch_orchestration_item($1, $2)",
                self.schema_name
            ))
            .bind(now_ms)
            .bind(lock_timeout_ms)
            .fetch_optional(&*self.pool)
            .await;

            let row = match result {
                Ok(r) => r,
                Err(e) => {
                    let provider_err = Self::sqlx_to_provider_error("fetch_orchestration_item", e);
                    if provider_err.is_retryable() && attempt < MAX_RETRIES {
                        warn!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            attempt = attempt + 1,
                            error = %provider_err,
                            "Retryable error, will retry"
                        );
                        _last_error = Some(provider_err);
                        sleep(std::time::Duration::from_millis(
                            RETRY_DELAY_MS * (attempt as u64 + 1),
                        ))
                        .await;
                        continue;
                    }
                    return Err(provider_err);
                }
            };

            if let Some((
                instance_id,
                orchestration_name,
                orchestration_version,
                execution_id,
                history_json,
                messages_json,
                lock_token,
                attempt_count,
            )) = row
            {
                let history: Vec<Event> = serde_json::from_value(history_json).map_err(|e| {
                    ProviderError::permanent(
                        "fetch_orchestration_item",
                        format!("Failed to deserialize history: {e}"),
                    )
                })?;

                let messages: Vec<WorkItem> =
                    serde_json::from_value(messages_json).map_err(|e| {
                        ProviderError::permanent(
                            "fetch_orchestration_item",
                            format!("Failed to deserialize messages: {e}"),
                        )
                    })?;

                let duration_ms = start.elapsed().as_millis() as u64;
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "fetch_orchestration_item",
                    instance_id = %instance_id,
                    execution_id = execution_id,
                    message_count = messages.len(),
                    history_count = history.len(),
                    attempt_count = attempt_count,
                    duration_ms = duration_ms,
                    attempts = attempt + 1,
                    "Fetched orchestration item via stored procedure"
                );

                return Ok(Some((
                    OrchestrationItem {
                        instance: instance_id,
                        orchestration_name,
                        execution_id: execution_id as u64,
                        version: orchestration_version,
                        history,
                        messages,
                    },
                    lock_token,
                    attempt_count as u32,
                )));
            }

            if attempt < MAX_RETRIES {
                sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
            }
        }

        Ok(None)
    }
    #[instrument(skip(self), fields(lock_token = %lock_token, execution_id = execution_id), target = "duroxide::providers::postgres")]
    async fn ack_orchestration_item(
        &self,
        lock_token: &str,
        execution_id: u64,
        history_delta: Vec<Event>,
        worker_items: Vec<WorkItem>,
        orchestrator_items: Vec<WorkItem>,
        metadata: ExecutionMetadata,
    ) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();

        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_MS: u64 = 50;

        let mut history_delta_payload = Vec::with_capacity(history_delta.len());
        for event in &history_delta {
            if event.event_id() == 0 {
                return Err(ProviderError::permanent(
                    "ack_orchestration_item",
                    "event_id must be set by runtime",
                ));
            }

            let event_json = serde_json::to_string(event).map_err(|e| {
                ProviderError::permanent(
                    "ack_orchestration_item",
                    format!("Failed to serialize event: {e}"),
                )
            })?;

            let event_type = format!("{event:?}")
                .split('{')
                .next()
                .unwrap_or("Unknown")
                .trim()
                .to_string();

            history_delta_payload.push(serde_json::json!({
                "event_id": event.event_id(),
                "event_type": event_type,
                "event_data": event_json,
            }));
        }

        let history_delta_json = serde_json::Value::Array(history_delta_payload);

        let worker_items_json = serde_json::to_value(&worker_items).map_err(|e| {
            ProviderError::permanent(
                "ack_orchestration_item",
                format!("Failed to serialize worker items: {e}"),
            )
        })?;

        let orchestrator_items_json = serde_json::to_value(&orchestrator_items).map_err(|e| {
            ProviderError::permanent(
                "ack_orchestration_item",
                format!("Failed to serialize orchestrator items: {e}"),
            )
        })?;

        let metadata_json = serde_json::json!({
            "orchestration_name": metadata.orchestration_name,
            "orchestration_version": metadata.orchestration_version,
            "status": metadata.status,
            "output": metadata.output,
        });

        for attempt in 0..=MAX_RETRIES {
            let result = sqlx::query(&format!(
                "SELECT {}.ack_orchestration_item($1, $2, $3, $4, $5, $6)",
                self.schema_name
            ))
            .bind(lock_token)
            .bind(execution_id as i64)
            .bind(&history_delta_json)
            .bind(&worker_items_json)
            .bind(&orchestrator_items_json)
            .bind(&metadata_json)
            .execute(&*self.pool)
            .await;

            match result {
                Ok(_) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    debug!(
                        target = "duroxide::providers::postgres",
                        operation = "ack_orchestration_item",
                        execution_id = execution_id,
                        history_count = history_delta.len(),
                        worker_items_count = worker_items.len(),
                        orchestrator_items_count = orchestrator_items.len(),
                        duration_ms = duration_ms,
                        attempts = attempt + 1,
                        "Acknowledged orchestration item via stored procedure"
                    );
                    return Ok(());
                }
                Err(e) => {
                    // Check for permanent errors first
                    if let SqlxError::Database(db_err) = &e {
                        if db_err.message().contains("Invalid lock token") {
                            return Err(ProviderError::permanent(
                                "ack_orchestration_item",
                                "Invalid lock token",
                            ));
                        }
                    } else if e.to_string().contains("Invalid lock token") {
                        return Err(ProviderError::permanent(
                            "ack_orchestration_item",
                            "Invalid lock token",
                        ));
                    }

                    let provider_err = Self::sqlx_to_provider_error("ack_orchestration_item", e);
                    if provider_err.is_retryable() && attempt < MAX_RETRIES {
                        warn!(
                            target = "duroxide::providers::postgres",
                            operation = "ack_orchestration_item",
                            attempt = attempt + 1,
                            error = %provider_err,
                            "Retryable error, will retry"
                        );
                        sleep(std::time::Duration::from_millis(
                            RETRY_DELAY_MS * (attempt as u64 + 1),
                        ))
                        .await;
                        continue;
                    }
                    return Err(provider_err);
                }
            }
        }

        // Should never reach here, but just in case
        Ok(())
    }
    #[instrument(skip(self), fields(lock_token = %lock_token), target = "duroxide::providers::postgres")]
    async fn abandon_orchestration_item(
        &self,
        lock_token: &str,
        delay: Option<Duration>,
        ignore_attempt: bool,
    ) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();
        let delay_param: Option<i64> = delay.map(|d| d.as_millis() as i64);

        let instance_id = match sqlx::query_scalar::<_, String>(&format!(
            "SELECT {}.abandon_orchestration_item($1, $2, $3)",
            self.schema_name
        ))
        .bind(lock_token)
        .bind(delay_param)
        .bind(ignore_attempt)
        .fetch_one(&*self.pool)
        .await
        {
            Ok(instance_id) => instance_id,
            Err(e) => {
                if let SqlxError::Database(db_err) = &e {
                    if db_err.message().contains("Invalid lock token") {
                        return Err(ProviderError::permanent(
                            "abandon_orchestration_item",
                            "Invalid lock token",
                        ));
                    }
                } else if e.to_string().contains("Invalid lock token") {
                    return Err(ProviderError::permanent(
                        "abandon_orchestration_item",
                        "Invalid lock token",
                    ));
                }

                return Err(Self::sqlx_to_provider_error(
                    "abandon_orchestration_item",
                    e,
                ));
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        debug!(
            target = "duroxide::providers::postgres",
            operation = "abandon_orchestration_item",
            instance_id = %instance_id,
            delay_ms = delay.map(|d| d.as_millis() as u64),
            ignore_attempt = ignore_attempt,
            duration_ms = duration_ms,
            "Abandoned orchestration item via stored procedure"
        );

        Ok(())
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn read(&self, instance: &str) -> Result<Vec<Event>, ProviderError> {
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT out_event_data FROM {}.fetch_history($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("read", e))?;

        Ok(event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect())
    }

    #[instrument(skip(self), fields(instance = %instance, execution_id = execution_id), target = "duroxide::providers::postgres")]
    async fn append_with_execution(
        &self,
        instance: &str,
        execution_id: u64,
        new_events: Vec<Event>,
    ) -> Result<(), ProviderError> {
        if new_events.is_empty() {
            return Ok(());
        }

        let mut events_payload = Vec::with_capacity(new_events.len());
        for event in &new_events {
            if event.event_id() == 0 {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "append_with_execution",
                    error_type = "validation_error",
                    instance_id = %instance,
                    execution_id = execution_id,
                    "event_id must be set by runtime"
                );
                return Err(ProviderError::permanent(
                    "append_with_execution",
                    "event_id must be set by runtime",
                ));
            }

            let event_json = serde_json::to_string(event).map_err(|e| {
                ProviderError::permanent(
                    "append_with_execution",
                    format!("Failed to serialize event: {e}"),
                )
            })?;

            let event_type = format!("{event:?}")
                .split('{')
                .next()
                .unwrap_or("Unknown")
                .trim()
                .to_string();

            events_payload.push(serde_json::json!({
                "event_id": event.event_id(),
                "event_type": event_type,
                "event_data": event_json,
            }));
        }

        let events_json = serde_json::Value::Array(events_payload);

        sqlx::query(&format!(
            "SELECT {}.append_history($1, $2, $3)",
            self.schema_name
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .bind(events_json)
        .execute(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("append_with_execution", e))?;

        debug!(
            target = "duroxide::providers::postgres",
            operation = "append_with_execution",
            instance_id = %instance,
            execution_id = execution_id,
            event_count = new_events.len(),
            "Appended history events via stored procedure"
        );

        Ok(())
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn enqueue_for_worker(&self, item: WorkItem) -> Result<(), ProviderError> {
        let work_item = serde_json::to_string(&item).map_err(|e| {
            ProviderError::permanent(
                "enqueue_worker_work",
                format!("Failed to serialize work item: {e}"),
            )
        })?;

        sqlx::query(&format!(
            "SELECT {}.enqueue_worker_work($1)",
            self.schema_name
        ))
        .bind(work_item)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            error!(
                target = "duroxide::providers::postgres",
                operation = "enqueue_worker_work",
                error_type = "database_error",
                error = %e,
                "Failed to enqueue worker work"
            );
            Self::sqlx_to_provider_error("enqueue_worker_work", e)
        })?;

        Ok(())
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn fetch_work_item(
        &self,
        lock_timeout: Duration,
        _poll_timeout: Duration,
    ) -> Result<Option<(WorkItem, String, u32)>, ProviderError> {
        let start = std::time::Instant::now();

        // Convert Duration to milliseconds
        let lock_timeout_ms = lock_timeout.as_millis() as i64;

        let row = match sqlx::query_as::<_, (String, String, i32)>(&format!(
            "SELECT * FROM {}.fetch_work_item($1, $2)",
            self.schema_name
        ))
        .bind(Self::now_millis())
        .bind(lock_timeout_ms)
        .fetch_optional(&*self.pool)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                return Err(Self::sqlx_to_provider_error("fetch_work_item", e));
            }
        };

        let (work_item_json, lock_token, attempt_count) = match row {
            Some(row) => row,
            None => return Ok(None),
        };

        let work_item: WorkItem = serde_json::from_str(&work_item_json).map_err(|e| {
            ProviderError::permanent(
                "fetch_work_item",
                format!("Failed to deserialize worker item: {e}"),
            )
        })?;

        let duration_ms = start.elapsed().as_millis() as u64;

        // Extract instance for logging - different work item types have different structures
        let instance_id = match &work_item {
            WorkItem::ActivityExecute { instance, .. } => instance.as_str(),
            WorkItem::ActivityCompleted { instance, .. } => instance.as_str(),
            WorkItem::ActivityFailed { instance, .. } => instance.as_str(),
            WorkItem::StartOrchestration { instance, .. } => instance.as_str(),
            WorkItem::TimerFired { instance, .. } => instance.as_str(),
            WorkItem::ExternalRaised { instance, .. } => instance.as_str(),
            WorkItem::CancelInstance { instance, .. } => instance.as_str(),
            WorkItem::ContinueAsNew { instance, .. } => instance.as_str(),
            WorkItem::SubOrchCompleted {
                parent_instance, ..
            } => parent_instance.as_str(),
            WorkItem::SubOrchFailed {
                parent_instance, ..
            } => parent_instance.as_str(),
        };

        debug!(
            target = "duroxide::providers::postgres",
            operation = "fetch_work_item",
            instance_id = %instance_id,
            attempt_count = attempt_count,
            duration_ms = duration_ms,
            "Fetched activity work item via stored procedure"
        );

        Ok(Some((work_item, lock_token, attempt_count as u32)))
    }

    #[instrument(skip(self), fields(token = %token), target = "duroxide::providers::postgres")]
    async fn ack_work_item(&self, token: &str, completion: WorkItem) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();

        // Extract instance ID from completion WorkItem
        let instance_id = match &completion {
            WorkItem::ActivityCompleted { instance, .. }
            | WorkItem::ActivityFailed { instance, .. } => instance,
            _ => {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_worker",
                    error_type = "invalid_completion_type",
                    "Invalid completion work item type"
                );
                return Err(ProviderError::permanent(
                    "ack_worker",
                    "Invalid completion work item type",
                ));
            }
        };

        let completion_json = serde_json::to_string(&completion).map_err(|e| {
            ProviderError::permanent("ack_worker", format!("Failed to serialize completion: {e}"))
        })?;

        // Call stored procedure to atomically delete worker item and enqueue completion
        sqlx::query(&format!(
            "SELECT {}.ack_worker($1, $2, $3)",
            self.schema_name
        ))
        .bind(token)
        .bind(instance_id)
        .bind(completion_json)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            if e.to_string().contains("Worker queue item not found") {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_worker",
                    error_type = "worker_item_not_found",
                    token = %token,
                    "Worker queue item not found or already processed"
                );
                ProviderError::permanent(
                    "ack_worker",
                    "Worker queue item not found or already processed",
                )
            } else {
                Self::sqlx_to_provider_error("ack_worker", e)
            }
        })?;

        let duration_ms = start.elapsed().as_millis() as u64;
        debug!(
            target = "duroxide::providers::postgres",
            operation = "ack_worker",
            instance_id = %instance_id,
            duration_ms = duration_ms,
            "Acknowledged worker and enqueued completion"
        );

        Ok(())
    }

    #[instrument(skip(self), fields(token = %token), target = "duroxide::providers::postgres")]
    async fn renew_work_item_lock(
        &self,
        token: &str,
        extend_for: Duration,
    ) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();

        // Get current time from application for consistent time reference
        let now_ms = Self::now_millis();

        // Convert Duration to seconds for the stored procedure
        let extend_secs = extend_for.as_secs() as i64;

        match sqlx::query(&format!(
            "SELECT {}.renew_work_item_lock($1, $2, $3)",
            self.schema_name
        ))
        .bind(token)
        .bind(now_ms)
        .bind(extend_secs)
        .execute(&*self.pool)
        .await
        {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "renew_work_item_lock",
                    token = %token,
                    extend_for_secs = extend_secs,
                    duration_ms = duration_ms,
                    "Work item lock renewed successfully"
                );
                Ok(())
            }
            Err(e) => {
                if let SqlxError::Database(db_err) = &e {
                    if db_err.message().contains("Lock token invalid") {
                        return Err(ProviderError::permanent(
                            "renew_work_item_lock",
                            "Lock token invalid, expired, or already acked",
                        ));
                    }
                } else if e.to_string().contains("Lock token invalid") {
                    return Err(ProviderError::permanent(
                        "renew_work_item_lock",
                        "Lock token invalid, expired, or already acked",
                    ));
                }

                Err(Self::sqlx_to_provider_error("renew_work_item_lock", e))
            }
        }
    }

    #[instrument(skip(self), fields(token = %token), target = "duroxide::providers::postgres")]
    async fn abandon_work_item(
        &self,
        token: &str,
        delay: Option<Duration>,
        ignore_attempt: bool,
    ) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();
        let delay_param: Option<i64> = delay.map(|d| d.as_millis() as i64);

        match sqlx::query(&format!(
            "SELECT {}.abandon_work_item($1, $2, $3)",
            self.schema_name
        ))
        .bind(token)
        .bind(delay_param)
        .bind(ignore_attempt)
        .execute(&*self.pool)
        .await
        {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "abandon_work_item",
                    token = %token,
                    delay_ms = delay.map(|d| d.as_millis() as u64),
                    ignore_attempt = ignore_attempt,
                    duration_ms = duration_ms,
                    "Abandoned work item via stored procedure"
                );
                Ok(())
            }
            Err(e) => {
                if let SqlxError::Database(db_err) = &e {
                    if db_err.message().contains("Invalid lock token")
                        || db_err.message().contains("already acked")
                    {
                        return Err(ProviderError::permanent(
                            "abandon_work_item",
                            "Invalid lock token or already acked",
                        ));
                    }
                } else if e.to_string().contains("Invalid lock token")
                    || e.to_string().contains("already acked")
                {
                    return Err(ProviderError::permanent(
                        "abandon_work_item",
                        "Invalid lock token or already acked",
                    ));
                }

                Err(Self::sqlx_to_provider_error("abandon_work_item", e))
            }
        }
    }

    #[instrument(skip(self), fields(token = %token), target = "duroxide::providers::postgres")]
    async fn renew_orchestration_item_lock(
        &self,
        token: &str,
        extend_for: Duration,
    ) -> Result<(), ProviderError> {
        let start = std::time::Instant::now();

        // Get current time from application for consistent time reference
        let now_ms = Self::now_millis();

        // Convert Duration to seconds for the stored procedure
        let extend_secs = extend_for.as_secs() as i64;

        match sqlx::query(&format!(
            "SELECT {}.renew_orchestration_item_lock($1, $2, $3)",
            self.schema_name
        ))
        .bind(token)
        .bind(now_ms)
        .bind(extend_secs)
        .execute(&*self.pool)
        .await
        {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "renew_orchestration_item_lock",
                    token = %token,
                    extend_for_secs = extend_secs,
                    duration_ms = duration_ms,
                    "Orchestration item lock renewed successfully"
                );
                Ok(())
            }
            Err(e) => {
                if let SqlxError::Database(db_err) = &e {
                    if db_err.message().contains("Lock token invalid")
                        || db_err.message().contains("expired")
                        || db_err.message().contains("already released")
                    {
                        return Err(ProviderError::permanent(
                            "renew_orchestration_item_lock",
                            "Lock token invalid, expired, or already released",
                        ));
                    }
                } else if e.to_string().contains("Lock token invalid")
                    || e.to_string().contains("expired")
                    || e.to_string().contains("already released")
                {
                    return Err(ProviderError::permanent(
                        "renew_orchestration_item_lock",
                        "Lock token invalid, expired, or already released",
                    ));
                }

                Err(Self::sqlx_to_provider_error(
                    "renew_orchestration_item_lock",
                    e,
                ))
            }
        }
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn enqueue_for_orchestrator(
        &self,
        item: WorkItem,
        delay: Option<Duration>,
    ) -> Result<(), ProviderError> {
        let work_item = serde_json::to_string(&item).map_err(|e| {
            ProviderError::permanent(
                "enqueue_orchestrator_work",
                format!("Failed to serialize work item: {e}"),
            )
        })?;

        // Extract instance ID from WorkItem enum
        let instance_id = match &item {
            WorkItem::StartOrchestration { instance, .. }
            | WorkItem::ActivityCompleted { instance, .. }
            | WorkItem::ActivityFailed { instance, .. }
            | WorkItem::TimerFired { instance, .. }
            | WorkItem::ExternalRaised { instance, .. }
            | WorkItem::CancelInstance { instance, .. }
            | WorkItem::ContinueAsNew { instance, .. } => instance,
            WorkItem::SubOrchCompleted {
                parent_instance, ..
            }
            | WorkItem::SubOrchFailed {
                parent_instance, ..
            } => parent_instance,
            WorkItem::ActivityExecute { .. } => {
                return Err(ProviderError::permanent(
                    "enqueue_orchestrator_work",
                    "ActivityExecute should go to worker queue, not orchestrator queue",
                ));
            }
        };

        // Determine visible_at: use max of fire_at_ms (for TimerFired) and delay
        let now_ms = Self::now_millis();

        let visible_at_ms = if let WorkItem::TimerFired { fire_at_ms, .. } = &item {
            if *fire_at_ms > 0 {
                // Take max of fire_at_ms and delay (if provided)
                if let Some(delay) = delay {
                    std::cmp::max(*fire_at_ms, now_ms as u64 + delay.as_millis() as u64)
                } else {
                    *fire_at_ms
                }
            } else {
                // fire_at_ms is 0, use delay or NOW()
                delay
                    .map(|d| now_ms as u64 + d.as_millis() as u64)
                    .unwrap_or(now_ms as u64)
            }
        } else {
            // Non-timer item: use delay or NOW()
            delay
                .map(|d| now_ms as u64 + d.as_millis() as u64)
                .unwrap_or(now_ms as u64)
        };

        let visible_at = Utc
            .timestamp_millis_opt(visible_at_ms as i64)
            .single()
            .ok_or_else(|| {
                ProviderError::permanent(
                    "enqueue_orchestrator_work",
                    "Invalid visible_at timestamp",
                )
            })?;

        // ⚠️ CRITICAL: DO NOT extract orchestration metadata - instance creation happens via ack_orchestration_item metadata
        // Pass NULL for orchestration_name, orchestration_version, execution_id parameters

        // Call stored procedure to enqueue work
        sqlx::query(&format!(
            "SELECT {}.enqueue_orchestrator_work($1, $2, $3, $4, $5, $6)",
            self.schema_name
        ))
        .bind(instance_id)
        .bind(&work_item)
        .bind(visible_at)
        .bind::<Option<String>>(None) // orchestration_name - NULL
        .bind::<Option<String>>(None) // orchestration_version - NULL
        .bind::<Option<i64>>(None) // execution_id - NULL
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            error!(
                target = "duroxide::providers::postgres",
                operation = "enqueue_orchestrator_work",
                error_type = "database_error",
                error = %e,
                instance_id = %instance_id,
                "Failed to enqueue orchestrator work"
            );
            Self::sqlx_to_provider_error("enqueue_orchestrator_work", e)
        })?;

        debug!(
            target = "duroxide::providers::postgres",
            operation = "enqueue_orchestrator_work",
            instance_id = %instance_id,
            delay_ms = delay.map(|d| d.as_millis() as u64),
            "Enqueued orchestrator work"
        );

        Ok(())
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn read_with_execution(
        &self,
        instance: &str,
        execution_id: u64,
    ) -> Result<Vec<Event>, ProviderError> {
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT event_data FROM {} WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
            self.table_name("history")
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_all(&*self.pool)
        .await
        .ok()
        .unwrap_or_default();

        Ok(event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect())
    }

    fn as_management_capability(&self) -> Option<&dyn ProviderAdmin> {
        Some(self)
    }
}

#[async_trait::async_trait]
impl ProviderAdmin for PostgresProvider {
    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn list_instances(&self) -> Result<Vec<String>, ProviderError> {
        sqlx::query_scalar(&format!(
            "SELECT instance_id FROM {}.list_instances()",
            self.schema_name
        ))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("list_instances", e))
    }

    #[instrument(skip(self), fields(status = %status), target = "duroxide::providers::postgres")]
    async fn list_instances_by_status(&self, status: &str) -> Result<Vec<String>, ProviderError> {
        sqlx::query_scalar(&format!(
            "SELECT instance_id FROM {}.list_instances_by_status($1)",
            self.schema_name
        ))
        .bind(status)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("list_instances_by_status", e))
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn list_executions(&self, instance: &str) -> Result<Vec<u64>, ProviderError> {
        let execution_ids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT execution_id FROM {}.list_executions($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("list_executions", e))?;

        Ok(execution_ids.into_iter().map(|id| id as u64).collect())
    }

    #[instrument(skip(self), fields(instance = %instance, execution_id = execution_id), target = "duroxide::providers::postgres")]
    async fn read_history_with_execution_id(
        &self,
        instance: &str,
        execution_id: u64,
    ) -> Result<Vec<Event>, ProviderError> {
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT out_event_data FROM {}.fetch_history_with_execution($1, $2)",
            self.schema_name
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("read_execution", e))?;

        event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect::<Vec<Event>>()
            .into_iter()
            .map(Ok)
            .collect()
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn read_history(&self, instance: &str) -> Result<Vec<Event>, ProviderError> {
        let execution_id = self.latest_execution_id(instance).await?;
        self.read_history_with_execution_id(instance, execution_id)
            .await
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn latest_execution_id(&self, instance: &str) -> Result<u64, ProviderError> {
        sqlx::query_scalar(&format!(
            "SELECT {}.latest_execution_id($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("latest_execution_id", e))?
        .map(|id: i64| id as u64)
        .ok_or_else(|| ProviderError::permanent("latest_execution_id", "Instance not found"))
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn get_instance_info(&self, instance: &str) -> Result<InstanceInfo, ProviderError> {
        let row: Option<(
            String,
            String,
            String,
            i64,
            chrono::DateTime<Utc>,
            Option<chrono::DateTime<Utc>>,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(&format!(
            "SELECT * FROM {}.get_instance_info($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("get_instance_info", e))?;

        let (
            instance_id,
            orchestration_name,
            orchestration_version,
            current_execution_id,
            created_at,
            updated_at,
            status,
            output,
        ) =
            row.ok_or_else(|| ProviderError::permanent("get_instance_info", "Instance not found"))?;

        Ok(InstanceInfo {
            instance_id,
            orchestration_name,
            orchestration_version,
            current_execution_id: current_execution_id as u64,
            status: status.unwrap_or_else(|| "Running".to_string()),
            output,
            created_at: created_at.timestamp_millis() as u64,
            updated_at: updated_at
                .map(|dt| dt.timestamp_millis() as u64)
                .unwrap_or(created_at.timestamp_millis() as u64),
        })
    }

    #[instrument(skip(self), fields(instance = %instance, execution_id = execution_id), target = "duroxide::providers::postgres")]
    async fn get_execution_info(
        &self,
        instance: &str,
        execution_id: u64,
    ) -> Result<ExecutionInfo, ProviderError> {
        let row: Option<(
            i64,
            String,
            Option<String>,
            chrono::DateTime<Utc>,
            Option<chrono::DateTime<Utc>>,
            i64,
        )> = sqlx::query_as(&format!(
            "SELECT * FROM {}.get_execution_info($1, $2)",
            self.schema_name
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("get_execution_info", e))?;

        let (exec_id, status, output, started_at, completed_at, event_count) = row
            .ok_or_else(|| ProviderError::permanent("get_execution_info", "Execution not found"))?;

        Ok(ExecutionInfo {
            execution_id: exec_id as u64,
            status,
            output,
            started_at: started_at.timestamp_millis() as u64,
            completed_at: completed_at.map(|dt| dt.timestamp_millis() as u64),
            event_count: event_count as usize,
        })
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn get_system_metrics(&self) -> Result<SystemMetrics, ProviderError> {
        let row: Option<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(&format!(
            "SELECT * FROM {}.get_system_metrics()",
            self.schema_name
        ))
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("get_system_metrics", e))?;

        let (
            total_instances,
            total_executions,
            running_instances,
            completed_instances,
            failed_instances,
            total_events,
        ) = row.ok_or_else(|| {
            ProviderError::permanent("get_system_metrics", "Failed to get system metrics")
        })?;

        Ok(SystemMetrics {
            total_instances: total_instances as u64,
            total_executions: total_executions as u64,
            running_instances: running_instances as u64,
            completed_instances: completed_instances as u64,
            failed_instances: failed_instances as u64,
            total_events: total_events as u64,
        })
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn get_queue_depths(&self) -> Result<QueueDepths, ProviderError> {
        let now_ms = Self::now_millis();

        let row: Option<(i64, i64)> = sqlx::query_as(&format!(
            "SELECT * FROM {}.get_queue_depths($1)",
            self.schema_name
        ))
        .bind(now_ms)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("get_queue_depths", e))?;

        let (orchestrator_queue, worker_queue) = row.ok_or_else(|| {
            ProviderError::permanent("get_queue_depths", "Failed to get queue depths")
        })?;

        Ok(QueueDepths {
            orchestrator_queue: orchestrator_queue as usize,
            worker_queue: worker_queue as usize,
            timer_queue: 0, // Timers are in orchestrator queue with delayed visibility
        })
    }
}
