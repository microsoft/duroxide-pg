use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use duroxide::providers::{
    ExecutionInfo, ExecutionMetadata, InstanceInfo, ManagementCapability, OrchestrationItem,
    Provider, ProviderError, QueueDepths, SystemMetrics, WorkItem,
};
use duroxide::Event;
use sqlx::{postgres::PgPoolOptions, Error as SqlxError, PgPool};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, instrument};
use uuid::Uuid;

use crate::migrations::MigrationRunner;

pub struct PostgresProvider {
    pool: Arc<PgPool>,
    lock_timeout_ms: u64,
    schema_name: String,
}

impl PostgresProvider {
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_schema(database_url, None).await
    }

    pub async fn new_with_schema(database_url: &str, schema_name: Option<&str>) -> Result<Self> {
        Self::new_with_schema_and_timeout(database_url, schema_name, 30_000).await
    }

    pub async fn new_with_schema_and_timeout(
        database_url: &str,
        schema_name: Option<&str>,
        lock_timeout_ms: u64,
    ) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;

        let schema_name = schema_name.unwrap_or("public").to_string();

        let provider = Self {
            pool: Arc::new(pool),
            lock_timeout_ms,
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

    /// Generate a unique lock token
    fn generate_lock_token() -> String {
        Uuid::new_v4().to_string()
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
                    ProviderError::retryable(operation, format!("Deadlock detected: {}", e))
                } else if code == Some("40001") {
                    // Serialization failure - permanent error (transaction conflict, not transient)
                    ProviderError::permanent(operation, format!("Serialization failure: {}", e))
                } else if code == Some("23505") {
                    // Unique constraint violation (duplicate event)
                    ProviderError::permanent(operation, format!("Duplicate detected: {}", e))
                } else if code == Some("23503") {
                    // Foreign key constraint violation
                    ProviderError::permanent(operation, format!("Foreign key violation: {}", e))
                } else {
                    ProviderError::permanent(operation, format!("Database error: {}", e))
                }
            }
            SqlxError::PoolClosed | SqlxError::PoolTimedOut => {
                ProviderError::retryable(operation, format!("Connection pool error: {}", e))
            }
            SqlxError::Io(_) => {
                ProviderError::retryable(operation, format!("I/O error: {}", e))
            }
            _ => ProviderError::permanent(operation, format!("Unexpected error: {}", e)),
        }
    }

    /// Clean up schema after tests (drops all tables and optionally the schema)
    ///
    /// **SAFETY**: Never drops the "public" schema itself, only tables within it.
    /// Only drops the schema if it's a custom schema (not "public").
    pub async fn cleanup_schema(&self) -> Result<()> {
        // Call the stored procedure to drop all tables
        sqlx::query(&format!(
            "SELECT {}.cleanup_schema()",
            self.schema_name
        ))
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
    async fn fetch_orchestration_item(&self) -> Option<OrchestrationItem> {
        let start = std::time::Instant::now();

        // Retry loop: try to acquire instance lock up to 3 times
        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_MS: u64 = 10;

        for attempt in 0..=MAX_RETRIES {
            // Recalculate now_ms for each attempt to get accurate lock expiration checks
            let now_ms = Self::now_millis();
            
            let mut tx = match self.pool.begin().await {
                Ok(tx) => tx,
                Err(e) => {
                    error!(
                        target = "duroxide::providers::postgres",
                        operation = "fetch_orchestration_item",
                        error_type = "transaction_failed",
                        error = %e,
                        "Failed to start transaction"
                    );
                    return None;
                }
            };

            // Step 1: Find an instance that has visible messages AND is not locked (or lock expired)
            // Use SELECT FOR UPDATE SKIP LOCKED for atomic lock acquisition
            let instance_id_row: Option<(String,)> = match sqlx::query_as(&format!(
                r#"
                SELECT q.instance_id
                FROM {} q
                WHERE q.visible_at <= NOW()
                  AND NOT EXISTS (
                    SELECT 1
                    FROM {} il
                    WHERE il.instance_id = q.instance_id
                      AND il.locked_until > $1
                  )
                ORDER BY q.visible_at, q.id
                LIMIT 1
                FOR UPDATE SKIP LOCKED
                "#,
                self.table_name("orchestrator_queue"),
                self.table_name("instance_locks")
            ))
            .bind(now_ms)
            .fetch_optional(&mut *tx)
            .await
            {
                Ok(row) => row,
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        debug!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            error_type = "query_failed",
                            error = %e,
                            attempt = attempt + 1,
                            "Failed to query for available instances, retrying"
                        );
                    } else {
                        tracing::warn!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            error_type = "query_failed",
                            error = %e,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            "Failed to query for available instances after all retries exhausted"
                        );
                    }
                    tx.rollback().await.ok();
                    if attempt < MAX_RETRIES {
                        sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    }
                    return None;
                }
            };

            let instance_id = match instance_id_row {
                Some((id,)) => id,
                None => {
                    // No available instances - this could be transient during retries
                    // If we're retrying, wait a bit and try again
                    // Otherwise, return None immediately
                    if attempt < MAX_RETRIES {
                        debug!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            now_ms = now_ms,
                            attempt = attempt + 1,
                            "No available instances during retry, waiting and retrying"
                        );
                        tx.rollback().await.ok();
                        sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    }
                    debug!(
                        target = "duroxide::providers::postgres",
                        operation = "fetch_orchestration_item",
                        now_ms = now_ms,
                        "No available instances"
                    );
                    tx.rollback().await.ok();
                    return None;
                }
            };

            debug!(
                target = "duroxide::providers::postgres",
                operation = "fetch_orchestration_item",
                instance_id = %instance_id,
                attempt = attempt + 1,
                "Selected available instance"
            );

            // Step 2: Atomically acquire instance lock
            let lock_token = Self::generate_lock_token();
            let locked_until = now_ms + self.lock_timeout_ms as i64;

            let lock_result = match sqlx::query(&format!(
                r#"
                INSERT INTO {} (instance_id, lock_token, locked_until, locked_at)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT(instance_id) DO UPDATE
                SET lock_token = $2, locked_until = $3, locked_at = $4
                WHERE {}.locked_until <= $4
                "#,
                self.table_name("instance_locks"),
                self.table_name("instance_locks")
            ))
            .bind(&instance_id)
            .bind(&lock_token)
            .bind(locked_until)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        debug!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            error_type = "lock_acquisition_failed",
                            error = %e,
                            instance_id = %instance_id,
                            attempt = attempt + 1,
                            "Failed to acquire instance lock due to error, retrying"
                        );
                    } else {
                        tracing::warn!(
                            target = "duroxide::providers::postgres",
                            operation = "fetch_orchestration_item",
                            error_type = "lock_acquisition_failed",
                            error = %e,
                            instance_id = %instance_id,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            "Failed to acquire instance lock after all retries exhausted"
                        );
                    }
                    tx.rollback().await.ok();
                    if attempt < MAX_RETRIES {
                        sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    }
                    return None;
                }
            };

            if lock_result.rows_affected() == 0 {
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "fetch_orchestration_item",
                    instance_id = %instance_id,
                    now_ms = now_ms,
                    locked_until = locked_until,
                    attempt = attempt + 1,
                    "Failed to acquire instance lock (race condition - instance locked between selection and lock acquisition)"
                );
                tx.rollback().await.ok();
                
                // Race condition: instance was locked between selection and lock acquisition.
                // Retry by reselecting a new instance (not the same one, as it's locked).
                // This gives other lock holders time to release.
                if attempt < MAX_RETRIES {
                    sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    continue; // Will reselect a different instance
                }
                
                // All retries exhausted, return None
                return None;
            }

            // Lock acquired successfully, continue with the rest of the operation
            // Step 3: Lock ALL visible messages for this instance
            sqlx::query(&format!(
                r#"
                UPDATE {}
                SET lock_token = $1, locked_until = $2
                WHERE instance_id = $3 AND visible_at <= NOW()
                  AND (lock_token IS NULL OR locked_until <= $4)
                "#,
                self.table_name("orchestrator_queue")
            ))
            .bind(&lock_token)
            .bind(locked_until)
            .bind(&instance_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .ok()?;

            // Step 4: Fetch all locked messages
            let message_rows: Vec<(i64, String)> = sqlx::query_as(&format!(
                r#"
                SELECT id, work_item
                FROM {}
                WHERE lock_token = $1
                ORDER BY id
                "#,
                self.table_name("orchestrator_queue")
            ))
            .bind(&lock_token)
            .fetch_all(&mut *tx)
            .await
            .ok()?;

            if message_rows.is_empty() {
                // No messages for instance (shouldn't happen), release lock and rollback
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "fetch_orchestration_item",
                    instance_id = %instance_id,
                    now_ms = now_ms,
                    lock_token = %lock_token,
                    locked_until = locked_until,
                    "No messages found for locked instance"
                );
                sqlx::query(&format!(
                    "DELETE FROM {} WHERE instance_id = $1",
                    self.table_name("instance_locks")
                ))
                .bind(&instance_id)
                .execute(&mut *tx)
                .await
                .ok();
                tx.rollback().await.ok();
                return None;
            }

            // Deserialize work items
            let messages: Vec<WorkItem> = message_rows
                .into_iter()
                .filter_map(|(_, work_item_json)| {
                    serde_json::from_str::<WorkItem>(&work_item_json).ok()
                })
                .collect();

            debug!(
                target = "duroxide::providers::postgres",
                operation = "fetch_orchestration_item",
                instance_id = %instance_id,
                message_count = messages.len(),
                "Fetched and marked messages for locked instance"
            );

            // Step 5: Load instance metadata
            let instance_info: Option<(String, Option<String>, i64)> = sqlx::query_as(&format!(
                r#"
                SELECT orchestration_name, orchestration_version, current_execution_id
                FROM {}
                WHERE instance_id = $1
                "#,
                self.table_name("instances")
            ))
            .bind(&instance_id)
            .fetch_optional(&mut *tx)
            .await
            .ok()?;

            let (orchestration_name, orchestration_version, current_execution_id, history) =
                if let Some((name, version_opt, exec_id)) = instance_info {
                    // Handle NULL version
                    let version = version_opt.unwrap_or_else(|| {
                        debug_assert!(
                            false,
                            "Instance exists with NULL version - should be set by metadata"
                        );
                        "unknown".to_string()
                    });
                    
                    // Instance exists - get history for current execution
                    let history_rows: Vec<String> = sqlx::query_scalar(&format!(
                        "SELECT event_data FROM {} WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
                        self.table_name("history")
                    ))
                    .bind(&instance_id)
                    .bind(exec_id)
                    .fetch_all(&mut *tx)
                    .await
                    .ok()
                    .unwrap_or_default();

                    let history: Vec<Event> = history_rows
                        .into_iter()
                        .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
                        .collect();

                    (name, version, exec_id as u64, history)
                } else {
                    // New instance - find StartOrchestration or ContinueAsNew in all work items
                    if let Some(start_item) = messages.iter().find(|item| {
                        matches!(item, WorkItem::StartOrchestration { .. } | WorkItem::ContinueAsNew { .. })
                    }) {
                        let (orchestration, version) = match start_item {
                            WorkItem::StartOrchestration { orchestration, version, .. }
                            | WorkItem::ContinueAsNew { orchestration, version, .. } => {
                                (orchestration.clone(), version.clone())
                            }
                            _ => unreachable!(),
                        };
                        let version = version.unwrap_or_else(|| "unknown".to_string());
                        let exec_id = match start_item {
                            WorkItem::StartOrchestration { execution_id, .. } => *execution_id,
                            WorkItem::ContinueAsNew { .. } => {
                                // ContinueAsNew doesn't have execution_id - use 1 as default for new instance
                                // The actual execution_id will be set when the instance is created via ack metadata
                                1
                            }
                            _ => unreachable!(),
                        };
                        (orchestration, version, exec_id, Vec::new())
                    } else {
                        // Shouldn't happen - no instance metadata and not StartOrchestration
                        tx.rollback().await.ok();
                        return None;
                    }
                };

            tx.commit().await.ok()?;

            let duration_ms = start.elapsed().as_millis() as u64;
            debug!(
                target = "duroxide::providers::postgres",
                operation = "fetch_orchestration_item",
                instance_id = %instance_id,
                execution_id = current_execution_id,
                message_count = messages.len(),
                history_count = history.len(),
                duration_ms = duration_ms,
                attempts = attempt + 1,
                "Fetched orchestration item"
            );

            return Some(OrchestrationItem {
                instance: instance_id,
                orchestration_name,
                execution_id: current_execution_id,
                version: orchestration_version,
                history,
                messages,
                lock_token,
            });
        }

        // Should never reach here, but return None if we do
        None
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
        // ALL operations must be atomic (single transaction)
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_orchestration_item",
                    error_type = "transaction_failed",
                    error = %e,
                    "Failed to start transaction"
                );
                return Err(Self::sqlx_to_provider_error("ack_orchestration_item", e));
            }
        };

        // Step 1: Validate lock token and get instance_id
        let instance_id_row: Option<(String,)> = sqlx::query_as(&format!(
            "SELECT instance_id FROM {} WHERE lock_token = $1 AND locked_until > $2",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .bind(Self::now_millis())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

        let instance_id = match instance_id_row {
            Some((id,)) => id,
            None => {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_orchestration_item",
                    error_type = "invalid_lock_token",
                    lock_token = %lock_token,
                    "Invalid lock token"
                );
                tx.rollback().await.ok();
                return Err(ProviderError::permanent("ack_orchestration_item", "Invalid lock token"));
            }
        };

        debug!(
            target = "duroxide::providers::postgres",
            operation = "ack_orchestration_item",
            instance_id = %instance_id,
            execution_id = execution_id,
            history_delta_len = history_delta.len(),
            worker_items_len = worker_items.len(),
            orchestrator_items_len = orchestrator_items.len(),
            "Acking orchestration item"
        );

        // Step 2: Create or update instance metadata from runtime-provided metadata
        if let (Some(name), Some(version)) = (
            &metadata.orchestration_name,
            &metadata.orchestration_version,
        ) {
            // Step 2a: Ensure instance exists (creates if new, ignores if exists)
            sqlx::query(&format!(
                "INSERT INTO {} (instance_id, orchestration_name, orchestration_version, current_execution_id)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (instance_id) DO NOTHING",
                self.table_name("instances")
            ))
            .bind(&instance_id)
            .bind(name)
            .bind(version)
            .bind(execution_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

            // Step 2b: Update instance with resolved version
            sqlx::query(&format!(
                "UPDATE {} 
                 SET orchestration_name = $1, orchestration_version = $2
                 WHERE instance_id = $3",
                self.table_name("instances")
            ))
            .bind(name)
            .bind(version)
            .bind(&instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;
        }

        // Step 3: Idempotently create execution row
        sqlx::query(&format!(
            "INSERT INTO {} (instance_id, execution_id, status, started_at) VALUES ($1, $2, 'Running', NOW()) ON CONFLICT (instance_id, execution_id) DO NOTHING",
            self.table_name("executions")
        ))
        .bind(&instance_id)
        .bind(execution_id as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

        // Step 4: Update instance current_execution_id
        sqlx::query(&format!(
            "UPDATE {} SET current_execution_id = GREATEST(current_execution_id, $1) WHERE instance_id = $2",
            self.table_name("instances")
        ))
        .bind(execution_id as i64)
        .bind(&instance_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

        // Step 5: Append history_delta (validate event_id > 0)
        if !history_delta.is_empty() {
            // Validate that runtime provided event_ids
            for event in &history_delta {
                if event.event_id() == 0 {
                    tx.rollback().await.ok();
                    return Err(ProviderError::permanent("ack_orchestration_item", "event_id must be set by runtime"));
                }
            }

            let history_table = self.table_name("history");
            for event in history_delta {
                let event_json = serde_json::to_string(&event)
                    .map_err(|e| ProviderError::permanent("ack_orchestration_item", &format!("Failed to serialize event: {}", e)))?;

                // Extract event type for indexing
                let event_type = format!("{:?}", event)
                    .split('{')
                    .next()
                    .unwrap_or("Unknown")
                    .to_string();

                let insert_result = sqlx::query(&format!(
                    "INSERT INTO {} (instance_id, execution_id, event_id, event_type, event_data) VALUES ($1, $2, $3, $4, $5)",
                    history_table
                ))
                .bind(&instance_id)
                .bind(execution_id as i64)
                .bind(event.event_id() as i64)
                .bind(event_type)
                .bind(event_json)
                .execute(&mut *tx)
                .await;

                if let Err(e) = insert_result {
                    if let SqlxError::Database(db_err) = &e {
                        if db_err.code().as_deref() == Some("23505") {
                            tx.rollback().await.ok();
                            return Err(ProviderError::permanent("ack_orchestration_item", "Duplicate event_id detected"));
                        }
                    }
                    tx.rollback().await.ok();
                    return Err(Self::sqlx_to_provider_error("ack_orchestration_item", e));
                }
            }
        }

        // Step 6: Update execution metadata (status, output) from ExecutionMetadata
        if let Some(status) = &metadata.status {
            let completed_at = if status == "Completed" || status == "Failed" {
                Some(chrono::Utc::now())
            } else {
                None
            };

            sqlx::query(&format!(
                "UPDATE {} SET status = $1, output = $2, completed_at = $3 WHERE instance_id = $4 AND execution_id = $5",
                self.table_name("executions")
            ))
            .bind(status)
            .bind(&metadata.output)
            .bind(completed_at)
            .bind(&instance_id)
            .bind(execution_id as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;
        }

        // Step 7: Enqueue worker items
        for item in worker_items {
            let work_item = serde_json::to_string(&item)
                .map_err(|e| ProviderError::permanent("ack_orchestration_item", &format!("Failed to serialize work item: {}", e)))?;

            sqlx::query(&format!(
                "INSERT INTO {} (work_item, created_at) VALUES ($1, NOW())",
                self.table_name("worker_queue")
            ))
            .bind(work_item)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;
        }

        // Step 8: Enqueue orchestrator items (may include TimerFired with delayed visibility)
        // ⚠️ CRITICAL: DO NOT create instance here - even for StartOrchestration or ContinueAsNew
        // Instance creation happens ONLY via ack_orchestration_item metadata (step 2 above)
        for item in orchestrator_items {
            let work_item = serde_json::to_string(&item)
                .map_err(|e| ProviderError::permanent("ack_orchestration_item", &format!("Failed to serialize work item: {}", e)))?;

            // Extract instance ID from WorkItem enum
            let target_instance = match &item {
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
                        "ack_orchestration_item",
                        "ActivityExecute should go to worker queue, not orchestrator queue"
                    ));
                }
            };

            // Calculate visible_at based on item type
            let visible_at = if let WorkItem::TimerFired { fire_at_ms, .. } = &item {
                // For TimerFired, use the fire_at_ms timestamp
                DateTime::from_timestamp(*fire_at_ms as i64 / 1000, 0)
                    .ok_or_else(|| ProviderError::permanent("ack_orchestration_item", "Invalid fire_at_ms timestamp"))?
            } else {
                // Immediate visibility
                Utc::now()
            };

            sqlx::query(&format!(
                "INSERT INTO {} (instance_id, work_item, visible_at, created_at) VALUES ($1, $2, $3, NOW())",
                self.table_name("orchestrator_queue")
            ))
            .bind(target_instance)
            .bind(work_item)
            .bind(visible_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;
        }

        // Step 9: Delete locked messages and remove instance lock
        sqlx::query(&format!(
            "DELETE FROM {} WHERE lock_token = $1",
            self.table_name("orchestrator_queue")
        ))
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

        // Remove instance lock (processing complete)
        sqlx::query(&format!(
            "DELETE FROM {} WHERE instance_id = $1 AND lock_token = $2",
            self.table_name("instance_locks")
        ))
        .bind(&instance_id)
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("ack_orchestration_item", e))?;

        // Commit transaction - all operations succeed or all fail
        match tx.commit().await {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                debug!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_orchestration_item",
                    instance_id = %instance_id,
                    execution_id = execution_id,
                    duration_ms = duration_ms,
                    "Acknowledged orchestration item and released lock"
                );
                Ok(())
            }
            Err(e) => {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "ack_orchestration_item",
                    error_type = "transaction_failed",
                    error = %e,
                    instance_id = %instance_id,
                    "Failed to commit transaction"
                );
                Err(Self::sqlx_to_provider_error("ack_orchestration_item", e))
            }
        }
    }

    #[instrument(skip(self), fields(lock_token = %lock_token), target = "duroxide::providers::postgres")]
    async fn abandon_orchestration_item(
        &self,
        lock_token: &str,
        delay_ms: Option<u64>,
    ) -> Result<(), ProviderError> {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                error!(
                    target = "duroxide::providers::postgres",
                    operation = "abandon_orchestration_item",
                    error_type = "transaction_failed",
                    error = %e,
                    "Failed to start transaction"
                );
                return Err(Self::sqlx_to_provider_error("abandon_orchestration_item", e));
            }
        };

        // Step 1: Validate lock token and get instance_id
        let instance_id_row: Option<(String,)> = sqlx::query_as(&format!(
            "SELECT instance_id FROM {} WHERE lock_token = $1",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("abandon_orchestration_item", e))?;

        let instance_id = match instance_id_row {
            Some((id,)) => id,
            None => {
                tx.rollback().await.ok();
                return Err(ProviderError::permanent("abandon_orchestration_item", "Invalid lock token"));
            }
        };

        // Step 2: Clear lock_token from messages (unlock them)
        let visible_at = if let Some(delay_ms) = delay_ms {
            Utc::now() + chrono::Duration::milliseconds(delay_ms as i64)
        } else {
            Utc::now()
        };

        sqlx::query(&format!(
            "UPDATE {} SET lock_token = NULL, locked_until = NULL, visible_at = $1 WHERE lock_token = $2",
            self.table_name("orchestrator_queue")
        ))
        .bind(visible_at)
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("abandon_orchestration_item", e))?;

        // Step 3: Remove instance lock
        sqlx::query(&format!(
            "DELETE FROM {} WHERE lock_token = $1",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::sqlx_to_provider_error("abandon_orchestration_item", e))?;

        tx.commit()
            .await
            .map_err(|e| Self::sqlx_to_provider_error("abandon_orchestration_item", e))?;

        debug!(
            target = "duroxide::providers::postgres",
            operation = "abandon_orchestration_item",
            instance_id = %instance_id,
            delay_ms = delay_ms,
            "Abandoned orchestration item"
        );

        Ok(())
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn read(&self, instance: &str) -> Vec<Event> {
        // Get latest execution ID from executions table (matching SQLite provider behavior)
        // Default to execution_id 1 if no executions exist
        let execution_id: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(MAX(execution_id), 1) FROM {} WHERE instance_id = $1",
            self.table_name("executions")
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(1);

        // Load events for that execution
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT event_data FROM {} WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
            self.table_name("history")
        ))
        .bind(instance)
        .bind(execution_id)
        .fetch_all(&*self.pool)
        .await
        .ok()
        .unwrap_or_default();

        event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect()
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

        // Validate that runtime provided event_ids
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
                return Err(ProviderError::permanent("append_with_execution", "event_id must be set by runtime"));
            }
        }

        let history_table = self.table_name("history");
        let event_count = new_events.len();

        // Use a transaction for batch insert
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::sqlx_to_provider_error("append_with_execution", e))?;

        for event in new_events {
            let event_json = serde_json::to_string(&event)
                .map_err(|e| ProviderError::permanent("append_with_execution", &format!("Failed to serialize event: {}", e)))?;

            // Extract event type for indexing (discriminant name)
            let event_type = format!("{:?}", event)
                .split('{')
                .next()
                .unwrap_or("Unknown")
                .to_string();

            sqlx::query(&format!(
                "INSERT INTO {} (instance_id, execution_id, event_id, event_type, event_data) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance_id, execution_id, event_id) DO NOTHING",
                history_table
            ))
            .bind(instance)
            .bind(execution_id as i64)
            .bind(event.event_id() as i64)
            .bind(event_type)
            .bind(event_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::sqlx_to_provider_error("append_with_execution", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| Self::sqlx_to_provider_error("append_with_execution", e))?;

        debug!(
            target = "duroxide::providers::postgres",
            operation = "append_with_execution",
            instance_id = %instance,
            execution_id = execution_id,
            event_count = event_count,
            "Appended history events"
        );

        Ok(())
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn enqueue_worker_work(&self, item: WorkItem) -> Result<(), ProviderError> {
        let work_item = serde_json::to_string(&item)
            .map_err(|e| ProviderError::permanent("enqueue_worker_work", &format!("Failed to serialize work item: {}", e)))?;

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
    async fn dequeue_worker_peek_lock(&self) -> Option<(WorkItem, String)> {
        let start = std::time::Instant::now();

        // Hold the SELECT FOR UPDATE lock until we update the row with our lock token.
        let mut tx = self.pool.begin().await.ok()?;

        let row: Option<(i64, String)> = sqlx::query_as(&format!(
            r#"
            SELECT id, work_item
            FROM {}
            WHERE lock_token IS NULL OR locked_until <= $1
            ORDER BY id
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
            self.table_name("worker_queue")
        ))
        .bind(Self::now_millis())
        .fetch_optional(&mut *tx)
        .await
        .ok()?;

        let (id, work_item_json) = match row {
            Some(row) => row,
            None => {
                tx.rollback().await.ok();
                return None;
            }
        };

        // Deserialize the work item
        let work_item: WorkItem = match serde_json::from_str(&work_item_json) {
            Ok(item) => item,
            Err(_) => {
                tx.rollback().await.ok();
                return None;
            }
        };

        // Generate lock token and calculate expiration
        let lock_token = Self::generate_lock_token();
        let now_ms = Self::now_millis();
        let locked_until = now_ms + self.lock_timeout_ms as i64;

        // Update the row with lock token
        let rows_affected = sqlx::query(&format!(
            "UPDATE {} SET lock_token = $1, locked_until = $2 WHERE id = $3",
            self.table_name("worker_queue")
        ))
        .bind(&lock_token)
        .bind(locked_until)
        .bind(id)
        .execute(&mut *tx)
        .await
        .ok()?
        .rows_affected();

        if rows_affected == 0 {
            tx.rollback().await.ok();
            return None; // Row was already locked by another process
        }

        tx.commit().await.ok()?;

        let duration_ms = start.elapsed().as_millis() as u64;
        debug!(
            target = "duroxide::providers::postgres",
            operation = "dequeue_worker_peek_lock",
            duration_ms = duration_ms,
            "Dequeued worker work item"
        );

        Some((work_item, lock_token))
    }

    #[instrument(skip(self), fields(token = %token), target = "duroxide::providers::postgres")]
    async fn ack_worker(&self, token: &str, completion: WorkItem) -> Result<(), ProviderError> {
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
                return Err(ProviderError::permanent("ack_worker", "Invalid completion work item type"));
            }
        };

        let completion_json = serde_json::to_string(&completion)
            .map_err(|e| ProviderError::permanent("ack_worker", &format!("Failed to serialize completion: {}", e)))?;

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
                ProviderError::permanent("ack_worker", "Worker queue item not found or already processed")
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

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn enqueue_orchestrator_work(
        &self,
        item: WorkItem,
        delay_ms: Option<u64>,
    ) -> Result<(), ProviderError> {
        let work_item = serde_json::to_string(&item)
            .map_err(|e| ProviderError::permanent("enqueue_orchestrator_work", &format!("Failed to serialize work item: {}", e)))?;

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
                    "ActivityExecute should go to worker queue, not orchestrator queue"
                ));
            }
        };

        // Determine visible_at: use max of fire_at_ms (for TimerFired) and delay_ms
        let now_ms = Self::now_millis();

        let visible_at_ms = if let WorkItem::TimerFired { fire_at_ms, .. } = &item {
            if *fire_at_ms > 0 {
                // Take max of fire_at_ms and delay_ms (if provided)
                if let Some(delay_ms) = delay_ms {
                    std::cmp::max(*fire_at_ms, now_ms as u64 + delay_ms)
                } else {
                    *fire_at_ms
                }
            } else {
                // fire_at_ms is 0, use delay_ms or NOW()
                delay_ms.map(|d| now_ms as u64 + d).unwrap_or(now_ms as u64)
            }
        } else {
            // Non-timer item: use delay_ms or NOW()
            delay_ms.map(|d| now_ms as u64 + d).unwrap_or(now_ms as u64)
        };

        let visible_at = Utc
            .timestamp_millis_opt(visible_at_ms as i64)
            .single()
            .ok_or_else(|| ProviderError::permanent("enqueue_orchestrator_work", "Invalid visible_at timestamp"))?;

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
        .bind::<Option<String>>(None)  // orchestration_name - NULL
        .bind::<Option<String>>(None)  // orchestration_version - NULL
        .bind::<Option<i64>>(None)     // execution_id - NULL
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
            delay_ms = delay_ms,
            "Enqueued orchestrator work"
        );

        Ok(())
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn read_with_execution(&self, instance: &str, execution_id: u64) -> Vec<Event> {
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

        event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect()
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn latest_execution_id(&self, instance: &str) -> Option<u64> {
        sqlx::query_scalar(&format!(
            "SELECT {}.latest_execution_id($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .ok()
        .flatten()
        .map(|id: i64| id as u64)
    }

    #[instrument(skip(self), target = "duroxide::providers::postgres")]
    async fn list_instances(&self) -> Vec<String> {
        sqlx::query_scalar(&format!(
            "SELECT instance_id FROM {}.list_instances()",
            self.schema_name
        ))
        .fetch_all(&*self.pool)
        .await
        .ok()
        .unwrap_or_default()
    }

    #[instrument(skip(self), fields(instance = %instance), target = "duroxide::providers::postgres")]
    async fn list_executions(&self, instance: &str) -> Vec<u64> {
        let execution_ids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT execution_id FROM {}.list_executions($1)",
            self.schema_name
        ))
        .bind(instance)
        .fetch_all(&*self.pool)
        .await
        .ok()
        .unwrap_or_default();

        execution_ids.into_iter().map(|id| id as u64).collect()
    }

    fn as_management_capability(&self) -> Option<&dyn ManagementCapability> {
        Some(self)
    }
}

#[async_trait::async_trait]
impl ManagementCapability for PostgresProvider {
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
    async fn read_execution(
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
        ) = row.ok_or_else(|| ProviderError::permanent("get_instance_info", "Instance not found"))?;

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

        let (exec_id, status, output, started_at, completed_at, event_count) =
            row.ok_or_else(|| ProviderError::permanent("get_execution_info", "Execution not found"))?;

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
        let row: Option<(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        )> = sqlx::query_as(&format!(
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
        ) = row.ok_or_else(|| ProviderError::permanent("get_system_metrics", "Failed to get system metrics"))?;

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

        let (orchestrator_queue, worker_queue) =
            row.ok_or_else(|| ProviderError::permanent("get_queue_depths", "Failed to get queue depths"))?;

        Ok(QueueDepths {
            orchestrator_queue: orchestrator_queue as usize,
            worker_queue: worker_queue as usize,
            timer_queue: 0, // Timers are in orchestrator queue with delayed visibility
        })
    }
}
