use duroxide::providers::{Provider, WorkItem, OrchestrationItem, ExecutionMetadata, ManagementCapability, InstanceInfo, ExecutionInfo, SystemMetrics, QueueDepths};
use duroxide::Event;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::Arc;
use uuid::Uuid;
use anyhow::Result;
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Utc};

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
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;

        let schema_name = schema_name.unwrap_or("public").to_string();

        let provider = Self {
            pool: Arc::new(pool),
            lock_timeout_ms: 30000, // 30 seconds
            schema_name: schema_name.clone(),
        };

        // Create schema if it doesn't exist (unless using default "public")
        if schema_name != "public" {
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", schema_name))
                .execute(&*provider.pool)
                .await?;
        }

        // Initialize schema (create tables)
        provider.initialize_schema().await?;

        Ok(provider)
    }

    pub async fn initialize_schema(&self) -> Result<()> {
        // Create tables using schema-qualified names

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                instance_id TEXT PRIMARY KEY,
                orchestration_name TEXT NOT NULL,
                orchestration_version TEXT NOT NULL,
                current_execution_id BIGINT NOT NULL DEFAULT 1,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            self.table_name("instances")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                instance_id TEXT NOT NULL,
                execution_id BIGINT NOT NULL,
                status TEXT NOT NULL DEFAULT 'Running',
                output TEXT,
                started_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                completed_at TIMESTAMPTZ,
                PRIMARY KEY (instance_id, execution_id)
            )
            "#,
            self.table_name("executions")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                instance_id TEXT NOT NULL,
                execution_id BIGINT NOT NULL,
                event_id BIGINT NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (instance_id, execution_id, event_id)
            )
            "#,
            self.table_name("history")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                id BIGSERIAL PRIMARY KEY,
                instance_id TEXT NOT NULL,
                work_item TEXT NOT NULL,
                visible_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                lock_token TEXT,
                locked_until BIGINT,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            self.table_name("orchestrator_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                id BIGSERIAL PRIMARY KEY,
                work_item TEXT NOT NULL,
                lock_token TEXT,
                locked_until BIGINT,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            self.table_name("worker_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                instance_id TEXT PRIMARY KEY,
                lock_token TEXT NOT NULL,
                locked_until BIGINT NOT NULL,
                locked_at BIGINT NOT NULL
            )
            "#,
            self.table_name("instance_locks")
        ))
        .execute(&*self.pool)
        .await?;

        // Create indexes
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_visible ON {}(visible_at, lock_token)",
            self.table_name("orchestrator_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_instance ON {}(instance_id)",
            self.table_name("orchestrator_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_lock ON {}(lock_token)",
            self.table_name("orchestrator_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_worker_available ON {}(lock_token, id)",
            self.table_name("worker_queue")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_instance_locks_locked_until ON {}(locked_until)",
            self.table_name("instance_locks")
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_history_lookup ON {}(instance_id, execution_id, event_id)",
            self.table_name("history")
        ))
        .execute(&*self.pool)
        .await?;

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

    /// Get future timestamp in milliseconds
    fn timestamp_after_ms(delay_ms: u64) -> i64 {
        Self::now_millis() + delay_ms as i64
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

    /// Clean up schema after tests (drops all tables and optionally the schema)
    /// 
    /// **SAFETY**: Never drops the "public" schema itself, only tables within it.
    /// Only drops the schema if it's a custom schema (not "public").
    pub async fn cleanup_schema(&self) -> Result<()> {
        let tables = vec![
            "instances",
            "executions",
            "history",
            "orchestrator_queue",
            "worker_queue",
            "instance_locks",
        ];

        for table in tables {
            sqlx::query(&format!(
                "DROP TABLE IF EXISTS {} CASCADE",
                self.table_name(table)
            ))
            .execute(&*self.pool)
            .await?;
        }

        // SAFETY: Never drop the "public" schema - it's a PostgreSQL system schema
        // Only drop custom schemas created for testing
        if self.schema_name != "public" {
            sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema_name))
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
    async fn fetch_orchestration_item(&self) -> Option<OrchestrationItem> {
        let mut tx = self.pool.begin().await.ok()?;
        let now_ms = Self::now_millis();

        // Step 1: Find an instance that has visible messages AND is not locked (or lock expired)
        // Use SELECT FOR UPDATE SKIP LOCKED for atomic lock acquisition
        let instance_id_row: Option<(String,)> = sqlx::query_as(&format!(
            r#"
            SELECT DISTINCT q.instance_id
            FROM {} q
            LEFT JOIN {} il ON q.instance_id = il.instance_id
            WHERE q.visible_at <= NOW()
              AND (il.instance_id IS NULL OR il.locked_until <= $1)
            ORDER BY q.id
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
            self.table_name("orchestrator_queue"),
            self.table_name("instance_locks")
        ))
        .bind(now_ms)
        .fetch_optional(&mut *tx)
        .await
        .ok()?;

        let instance_id = match instance_id_row {
            Some((id,)) => id,
            None => {
                tx.rollback().await.ok();
                return None;
            }
        };

        // Step 2: Atomically acquire instance lock
        let lock_token = Self::generate_lock_token();
        let locked_until = Self::timestamp_after_ms(self.lock_timeout_ms);

        let lock_result = sqlx::query(&format!(
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
        .ok()?;

        if lock_result.rows_affected() == 0 {
            // Failed to acquire lock (lock still held by another worker)
            tx.rollback().await.ok();
            return None;
        }

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

        // Step 5: Load instance metadata
        let instance_info: Option<(String, String, i64)> = sqlx::query_as(&format!(
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
            if let Some((name, version, exec_id)) = instance_info {
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
                // New instance - derive from first message if it's StartOrchestration
                if let Some(WorkItem::StartOrchestration { orchestration, version, execution_id, .. }) = messages.first() {
                    let name = orchestration.clone();
                    let version = version.as_deref().unwrap_or("1.0.0").to_string();
                    let exec_id = *execution_id;
                    (name, version, exec_id, Vec::new())
                } else {
                    // Shouldn't happen - no instance metadata and not StartOrchestration
                    tx.rollback().await.ok();
                    return None;
                }
            };

        tx.commit().await.ok()?;

        Some(OrchestrationItem {
            instance: instance_id,
            orchestration_name,
            execution_id: current_execution_id,
            version: orchestration_version,
            history,
            messages,
            lock_token,
        })
    }

    async fn ack_orchestration_item(
        &self,
        lock_token: &str,
        execution_id: u64,
        history_delta: Vec<Event>,
        worker_items: Vec<WorkItem>,
        orchestrator_items: Vec<WorkItem>,
        metadata: ExecutionMetadata,
    ) -> Result<(), String> {
        // ALL operations must be atomic (single transaction)
        let mut tx = self.pool.begin().await
            .map_err(|e| format!("Failed to start transaction: {}", e))?;

        // Step 1: Validate lock token and get instance_id
        let instance_id_row: Option<(String,)> = sqlx::query_as(&format!(
            "SELECT instance_id FROM {} WHERE lock_token = $1 AND locked_until > $2",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .bind(Self::now_millis())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("Failed to validate lock: {}", e))?;

        let instance_id = match instance_id_row {
            Some((id,)) => id,
            None => {
                tx.rollback().await.ok();
                return Err("Invalid or expired lock token".to_string());
            }
        };

        // Step 2: Remove instance lock (will be deleted at end, but validate now)
        // We'll delete it at the end after all operations succeed

        // Step 3: Idempotently create execution row
        sqlx::query(&format!(
            "INSERT INTO {} (instance_id, execution_id, status, started_at) VALUES ($1, $2, 'Running', NOW()) ON CONFLICT (instance_id, execution_id) DO NOTHING",
            self.table_name("executions")
        ))
        .bind(&instance_id)
        .bind(execution_id as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to create execution: {}", e))?;

        // Step 4: Update instance current_execution_id
        sqlx::query(&format!(
            "UPDATE {} SET current_execution_id = GREATEST(current_execution_id, $1) WHERE instance_id = $2",
            self.table_name("instances")
        ))
        .bind(execution_id as i64)
        .bind(&instance_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to update current_execution_id: {}", e))?;

        // Update instance metadata from runtime-provided metadata (no event inspection)
        if let (Some(name), Some(version)) = (&metadata.orchestration_name, &metadata.orchestration_version) {
            sqlx::query(&format!(
                "UPDATE {} SET orchestration_name = $1, orchestration_version = $2 WHERE instance_id = $3",
                self.table_name("instances")
            ))
            .bind(name)
            .bind(version)
            .bind(&instance_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed to update instance metadata: {}", e))?;
        }

        // Step 5: Append history_delta (validate event_id > 0)
        if !history_delta.is_empty() {
            // Validate that runtime provided event_ids
            for event in &history_delta {
                if event.event_id() == 0 {
                    tx.rollback().await.ok();
                    return Err("event_id must be set by runtime".to_string());
                }
            }

            let history_table = self.table_name("history");
            for event in history_delta {
                let event_json = serde_json::to_string(&event)
                    .map_err(|e| format!("Failed to serialize event: {}", e))?;
                
                // Extract event type for indexing
                let event_type = format!("{:?}", event).split('{').next().unwrap_or("Unknown").to_string();

                sqlx::query(&format!(
                    "INSERT INTO {} (instance_id, execution_id, event_id, event_type, event_data) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance_id, execution_id, event_id) DO NOTHING",
                    history_table
                ))
                .bind(&instance_id)
                .bind(execution_id as i64)
                .bind(event.event_id() as i64)
                .bind(event_type)
                .bind(event_json)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to append history: {}", e))?;
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
            .map_err(|e| format!("Failed to update execution metadata: {}", e))?;
        }

        // Step 7: Enqueue worker items
        for item in worker_items {
            let work_item = serde_json::to_string(&item)
                .map_err(|e| format!("Failed to serialize work item: {}", e))?;

            sqlx::query(&format!(
                "INSERT INTO {} (work_item, created_at) VALUES ($1, NOW())",
                self.table_name("worker_queue")
            ))
            .bind(work_item)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed to enqueue worker item: {}", e))?;
        }

        // Step 8: Enqueue orchestrator items (may include TimerFired with delayed visibility)
        for item in orchestrator_items {
            let work_item = serde_json::to_string(&item)
                .map_err(|e| format!("Failed to serialize work item: {}", e))?;

            // Extract instance ID from WorkItem enum
            let target_instance = match &item {
                WorkItem::StartOrchestration { instance, .. } |
                WorkItem::ActivityCompleted { instance, .. } |
                WorkItem::ActivityFailed { instance, .. } |
                WorkItem::TimerFired { instance, .. } |
                WorkItem::ExternalRaised { instance, .. } |
                WorkItem::CancelInstance { instance, .. } |
                WorkItem::ContinueAsNew { instance, .. } => instance,
                WorkItem::SubOrchCompleted { parent_instance, .. } |
                WorkItem::SubOrchFailed { parent_instance, .. } => parent_instance,
                WorkItem::ActivityExecute { .. } => {
                    return Err("ActivityExecute should go to worker queue, not orchestrator queue".to_string());
                }
            };

            // Handle StartOrchestration - create instance and execution rows
            if let WorkItem::StartOrchestration {
                orchestration,
                version,
                execution_id: start_exec_id,
                ..
            } = &item
            {
                let version = version.as_deref().unwrap_or("1.0.0");
                
                sqlx::query(&format!(
                    "INSERT INTO {} (instance_id, orchestration_name, orchestration_version, current_execution_id) VALUES ($1, $2, $3, $4) ON CONFLICT (instance_id) DO NOTHING",
                    self.table_name("instances")
                ))
                .bind(target_instance)
                .bind(orchestration)
                .bind(version)
                .bind(*start_exec_id as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to create instance: {}", e))?;

                sqlx::query(&format!(
                    "INSERT INTO {} (instance_id, execution_id, status) VALUES ($1, $2, 'Running') ON CONFLICT (instance_id, execution_id) DO NOTHING",
                    self.table_name("executions")
                ))
                .bind(target_instance)
                .bind(*start_exec_id as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to create execution: {}", e))?;
            }

            // Calculate visible_at based on item type
            let visible_at = if let WorkItem::TimerFired { fire_at_ms, .. } = &item {
                // For TimerFired, use the fire_at_ms timestamp
                DateTime::from_timestamp(*fire_at_ms as i64 / 1000, 0)
                    .ok_or_else(|| "Invalid fire_at_ms timestamp".to_string())?
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
            .map_err(|e| format!("Failed to enqueue orchestrator item: {}", e))?;
        }

        // Step 9: Delete locked messages and remove instance lock
        sqlx::query(&format!(
            "DELETE FROM {} WHERE lock_token = $1",
            self.table_name("orchestrator_queue")
        ))
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to delete locked messages: {}", e))?;

        // Remove instance lock (processing complete)
        sqlx::query(&format!(
            "DELETE FROM {} WHERE instance_id = $1 AND lock_token = $2",
            self.table_name("instance_locks")
        ))
        .bind(&instance_id)
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to remove instance lock: {}", e))?;

        // Commit transaction - all operations succeed or all fail
        tx.commit().await
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;

        Ok(())
    }

    async fn abandon_orchestration_item(
        &self,
        lock_token: &str,
        delay_ms: Option<u64>,
    ) -> Result<(), String> {
        let mut tx = self.pool.begin().await
            .map_err(|e| format!("Failed to start transaction: {}", e))?;

        // Step 1: Validate lock token and get instance_id
        let instance_id_row: Option<(String,)> = sqlx::query_as(&format!(
            "SELECT instance_id FROM {} WHERE lock_token = $1",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("Failed to validate lock: {}", e))?;

        let instance_id = match instance_id_row {
            Some((id,)) => id,
            None => {
                // Lock already released or expired - idempotent, return Ok
                tx.rollback().await.ok();
                return Ok(());
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
        .map_err(|e| format!("Failed to unlock messages: {}", e))?;

        // Step 3: Remove instance lock
        sqlx::query(&format!(
            "DELETE FROM {} WHERE lock_token = $1",
            self.table_name("instance_locks")
        ))
        .bind(lock_token)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to remove instance lock: {}", e))?;

        tx.commit().await
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;

        Ok(())
    }

    async fn read(&self, instance: &str) -> Vec<Event> {
        // Get latest execution ID
        let execution_id: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT current_execution_id FROM {} WHERE instance_id = $1",
            self.table_name("instances")
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .ok()
        .flatten();

        let execution_id = match execution_id {
            Some(id) => id,
            None => return Vec::new(), // Instance doesn't exist
        };

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

    async fn append_with_execution(
        &self,
        instance: &str,
        execution_id: u64,
        new_events: Vec<Event>,
    ) -> Result<(), String> {
        if new_events.is_empty() {
            return Ok(());
        }

        // Validate that runtime provided event_ids
        for event in &new_events {
            if event.event_id() == 0 {
                return Err("event_id must be set by runtime".to_string());
            }
        }

        let history_table = self.table_name("history");

        // Use a transaction for batch insert
        let mut tx = self.pool.begin().await.map_err(|e| format!("Failed to start transaction: {}", e))?;

        for event in new_events {
            let event_json = serde_json::to_string(&event)
                .map_err(|e| format!("Failed to serialize event: {}", e))?;
            
            // Extract event type for indexing (discriminant name)
            let event_type = format!("{:?}", event).split('{').next().unwrap_or("Unknown").to_string();

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
            .map_err(|e| format!("Failed to append history: {}", e))?;
        }

        tx.commit().await.map_err(|e| format!("Failed to commit transaction: {}", e))?;

        Ok(())
    }

    async fn enqueue_worker_work(&self, item: WorkItem) -> Result<(), String> {
        let work_item = serde_json::to_string(&item)
            .map_err(|e| format!("Failed to serialize work item: {}", e))?;

        sqlx::query(&format!(
            "INSERT INTO {} (work_item, created_at) VALUES ($1, NOW())",
            self.table_name("worker_queue")
        ))
        .bind(work_item)
        .execute(&*self.pool)
        .await
        .map_err(|e| format!("Failed to enqueue worker work: {}", e))?;

        Ok(())
    }

    async fn dequeue_worker_peek_lock(&self) -> Option<(WorkItem, String)> {
        // Use SELECT FOR UPDATE SKIP LOCKED for atomic lock acquisition
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
        .fetch_optional(&*self.pool)
        .await
        .ok()?;

        let (id, work_item_json) = row?;

        // Deserialize the work item
        let work_item: WorkItem = serde_json::from_str(&work_item_json)
            .ok()?;

        // Generate lock token and calculate expiration
        let lock_token = Self::generate_lock_token();
        let locked_until = Self::timestamp_after_ms(self.lock_timeout_ms);

        // Update the row with lock token
        let rows_affected = sqlx::query(&format!(
            "UPDATE {} SET lock_token = $1, locked_until = $2 WHERE id = $3",
            self.table_name("worker_queue")
        ))
        .bind(&lock_token)
        .bind(locked_until)
        .bind(id)
        .execute(&*self.pool)
        .await
        .ok()?
        .rows_affected();

        if rows_affected == 0 {
            return None; // Row was already locked by another process
        }

        Some((work_item, lock_token))
    }

    async fn ack_worker(&self, token: &str, completion: WorkItem) -> Result<(), String> {
        // Start transaction for atomic delete + enqueue
        let mut tx = self.pool.begin().await
            .map_err(|e| format!("Failed to start transaction: {}", e))?;

        // Delete the worker queue item
        let rows_affected = sqlx::query(&format!(
            "DELETE FROM {} WHERE lock_token = $1",
            self.table_name("worker_queue")
        ))
        .bind(token)
        .execute(&mut *tx)
        .await
            .map_err(|e| format!("Failed to delete worker queue item: {}", e))?
            .rows_affected();

        if rows_affected == 0 {
            return Err("Worker queue item not found or already processed".to_string());
        }

        // Enqueue completion to orchestrator queue
        let completion_json = serde_json::to_string(&completion)
            .map_err(|e| format!("Failed to serialize completion: {}", e))?;

        // Extract instance ID from completion WorkItem
        let instance_id = match &completion {
            WorkItem::ActivityCompleted { instance, .. } |
            WorkItem::ActivityFailed { instance, .. } => instance,
            _ => return Err("Invalid completion work item type".to_string()),
        };

        sqlx::query(&format!(
            "INSERT INTO {} (instance_id, work_item, visible_at, created_at) VALUES ($1, $2, NOW(), NOW())",
            self.table_name("orchestrator_queue")
        ))
        .bind(instance_id)
        .bind(completion_json)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to enqueue completion: {}", e))?;

        tx.commit().await
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;

        Ok(())
    }

    async fn enqueue_orchestrator_work(
        &self,
        item: WorkItem,
        delay_ms: Option<u64>,
    ) -> Result<(), String> {
        let work_item = serde_json::to_string(&item)
            .map_err(|e| format!("Failed to serialize work item: {}", e))?;

        // Extract instance ID from WorkItem enum
        let instance_id = match &item {
            WorkItem::StartOrchestration { instance, .. } |
            WorkItem::ActivityCompleted { instance, .. } |
            WorkItem::ActivityFailed { instance, .. } |
            WorkItem::TimerFired { instance, .. } |
            WorkItem::ExternalRaised { instance, .. } |
            WorkItem::CancelInstance { instance, .. } |
            WorkItem::ContinueAsNew { instance, .. } => instance,
            WorkItem::SubOrchCompleted { parent_instance, .. } |
            WorkItem::SubOrchFailed { parent_instance, .. } => parent_instance,
            WorkItem::ActivityExecute { .. } => {
                return Err("ActivityExecute should go to worker queue, not orchestrator queue".to_string());
            }
        };

        // Handle StartOrchestration - create instance and execution rows
        if let WorkItem::StartOrchestration {
            orchestration,
            version,
            execution_id,
            ..
        } = &item
        {
            let version = version.as_deref().unwrap_or("1.0.0");
            
            sqlx::query(&format!(
                "INSERT INTO {} (instance_id, orchestration_name, orchestration_version, current_execution_id) VALUES ($1, $2, $3, $4) ON CONFLICT (instance_id) DO NOTHING",
                self.table_name("instances")
            ))
            .bind(instance_id)
            .bind(orchestration)
            .bind(version)
            .bind(*execution_id as i64)
            .execute(&*self.pool)
            .await
            .map_err(|e| format!("Failed to create instance: {}", e))?;

            sqlx::query(&format!(
                "INSERT INTO {} (instance_id, execution_id, status) VALUES ($1, $2, 'Running') ON CONFLICT (instance_id, execution_id) DO NOTHING",
                self.table_name("executions")
            ))
            .bind(instance_id)
            .bind(*execution_id as i64)
            .execute(&*self.pool)
            .await
            .map_err(|e| format!("Failed to create execution: {}", e))?;
        }

        // Calculate visible_at based on delay_ms or TimerFired.fire_at_ms
        let visible_at = if let WorkItem::TimerFired { fire_at_ms, .. } = &item {
            // For TimerFired, use the fire_at_ms timestamp
            // Convert milliseconds Unix timestamp to TIMESTAMPTZ
            DateTime::from_timestamp(*fire_at_ms as i64 / 1000, 0)
                .ok_or_else(|| "Invalid fire_at_ms timestamp".to_string())?
        } else if let Some(delay_ms) = delay_ms {
            // For delayed items, use NOW() + delay_ms
            Utc::now() + chrono::Duration::milliseconds(delay_ms as i64)
        } else {
            // Immediate visibility
            Utc::now()
        };

        sqlx::query(&format!(
            "INSERT INTO {} (instance_id, work_item, visible_at, created_at) VALUES ($1, $2, $3, NOW())",
            self.table_name("orchestrator_queue")
        ))
        .bind(instance_id)
        .bind(work_item)
        .bind(visible_at)
        .execute(&*self.pool)
        .await
        .map_err(|e| format!("Failed to enqueue orchestrator work: {}", e))?;

        Ok(())
    }

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

    async fn latest_execution_id(&self, instance: &str) -> Option<u64> {
        sqlx::query_scalar(&format!(
            "SELECT current_execution_id FROM {} WHERE instance_id = $1",
            self.table_name("instances")
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .ok()
        .flatten()
        .map(|id: i64| id as u64)
    }

    async fn list_instances(&self) -> Vec<String> {
        sqlx::query_scalar(&format!(
            "SELECT instance_id FROM {} ORDER BY created_at DESC",
            self.table_name("instances")
        ))
        .fetch_all(&*self.pool)
        .await
        .ok()
        .unwrap_or_default()
    }

    async fn list_executions(&self, instance: &str) -> Vec<u64> {
        let execution_ids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT execution_id FROM {} WHERE instance_id = $1 ORDER BY execution_id",
            self.table_name("executions")
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
    async fn list_instances(&self) -> Result<Vec<String>, String> {
        sqlx::query_scalar(&format!(
            "SELECT instance_id FROM {} ORDER BY created_at DESC",
            self.table_name("instances")
        ))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| format!("Failed to list instances: {}", e))
    }

    async fn list_instances_by_status(&self, status: &str) -> Result<Vec<String>, String> {
        sqlx::query_scalar(&format!(
            r#"
            SELECT DISTINCT i.instance_id 
            FROM {} i
            JOIN {} e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE e.status = $1
            ORDER BY i.created_at DESC
            "#,
            self.table_name("instances"),
            self.table_name("executions")
        ))
        .bind(status)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| format!("Failed to list instances by status: {}", e))
    }

    async fn list_executions(&self, instance: &str) -> Result<Vec<u64>, String> {
        let execution_ids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT execution_id FROM {} WHERE instance_id = $1 ORDER BY execution_id",
            self.table_name("executions")
        ))
        .bind(instance)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| format!("Failed to list executions: {}", e))?;

        Ok(execution_ids.into_iter().map(|id| id as u64).collect())
    }

    async fn read_execution(&self, instance: &str, execution_id: u64) -> Result<Vec<Event>, String> {
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT event_data FROM {} WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
            self.table_name("history")
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| format!("Failed to read execution: {}", e))?;

        event_data_rows
            .into_iter()
            .filter_map(|event_data| serde_json::from_str::<Event>(&event_data).ok())
            .collect::<Vec<Event>>()
            .into_iter()
            .map(Ok)
            .collect()
    }

    async fn latest_execution_id(&self, instance: &str) -> Result<u64, String> {
        sqlx::query_scalar(&format!(
            "SELECT current_execution_id FROM {} WHERE instance_id = $1",
            self.table_name("instances")
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get latest execution: {}", e))?
        .map(|id: i64| id as u64)
        .ok_or_else(|| "Instance not found".to_string())
    }

    async fn get_instance_info(&self, instance: &str) -> Result<InstanceInfo, String> {
        let row: Option<(String, String, String, i64, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>, Option<String>, Option<String>)> = sqlx::query_as(&format!(
            r#"
            SELECT i.instance_id, i.orchestration_name, i.orchestration_version, 
                   i.current_execution_id, i.created_at, i.updated_at,
                   e.status, e.output
            FROM {} i
            LEFT JOIN {} e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE i.instance_id = $1
            "#,
            self.table_name("instances"),
            self.table_name("executions")
        ))
        .bind(instance)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get instance info: {}", e))?;

        let (instance_id, orchestration_name, orchestration_version, current_execution_id, created_at, updated_at, status, output) = row
            .ok_or_else(|| "Instance not found".to_string())?;

        Ok(InstanceInfo {
            instance_id,
            orchestration_name,
            orchestration_version,
            current_execution_id: current_execution_id as u64,
            status: status.unwrap_or_else(|| "Running".to_string()),
            output,
            created_at: created_at.timestamp_millis() as u64,
            updated_at: updated_at.map(|dt| dt.timestamp_millis() as u64).unwrap_or(created_at.timestamp_millis() as u64),
        })
    }

    async fn get_execution_info(&self, instance: &str, execution_id: u64) -> Result<ExecutionInfo, String> {
        let row: Option<(i64, String, Option<String>, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(&format!(
            r#"
            SELECT e.execution_id, e.status, e.output, 
                   e.started_at, e.completed_at
            FROM {} e
            WHERE e.instance_id = $1 AND e.execution_id = $2
            "#,
            self.table_name("executions")
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get execution info: {}", e))?;

        let (exec_id, status, output, started_at, completed_at) = row
            .ok_or_else(|| "Execution not found".to_string())?;

        // Get event count for this execution
        let event_count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE instance_id = $1 AND execution_id = $2",
            self.table_name("history")
        ))
        .bind(instance)
        .bind(execution_id as i64)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get event count: {}", e))?;

        Ok(ExecutionInfo {
            execution_id: exec_id as u64,
            status,
            output,
            started_at: started_at.timestamp_millis() as u64,
            completed_at: completed_at.map(|dt| dt.timestamp_millis() as u64),
            event_count: event_count as usize,
        })
    }

    async fn get_system_metrics(&self) -> Result<SystemMetrics, String> {
        // Get total instances
        let total_instances: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {}",
            self.table_name("instances")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        // Get total executions
        let total_executions: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {}",
            self.table_name("executions")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        // Get running instances
        let running: i64 = sqlx::query_scalar(&format!(
            r#"
            SELECT COUNT(DISTINCT i.instance_id)
            FROM {} i
            JOIN {} e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE e.status = 'Running'
            "#,
            self.table_name("instances"),
            self.table_name("executions")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        // Get completed instances
        let completed: i64 = sqlx::query_scalar(&format!(
            r#"
            SELECT COUNT(DISTINCT i.instance_id)
            FROM {} i
            JOIN {} e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE e.status = 'Completed'
            "#,
            self.table_name("instances"),
            self.table_name("executions")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        // Get failed instances
        let failed: i64 = sqlx::query_scalar(&format!(
            r#"
            SELECT COUNT(DISTINCT i.instance_id)
            FROM {} i
            JOIN {} e ON i.instance_id = e.instance_id 
              AND i.current_execution_id = e.execution_id
            WHERE e.status = 'Failed'
            "#,
            self.table_name("instances"),
            self.table_name("executions")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        // Get total events
        let total_events: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {}",
            self.table_name("history")
        ))
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get metrics: {}", e))?;

        Ok(SystemMetrics {
            total_instances: total_instances as u64,
            total_executions: total_executions as u64,
            running_instances: running as u64,
            completed_instances: completed as u64,
            failed_instances: failed as u64,
            total_events: total_events as u64,
        })
    }

    async fn get_queue_depths(&self) -> Result<QueueDepths, String> {
        let now_ms = Self::now_millis();

        let orchestrator_queue: i64 = sqlx::query_scalar(&format!(
            r#"
            SELECT COUNT(*) FROM {} 
            WHERE lock_token IS NULL OR locked_until <= $1
            "#,
            self.table_name("orchestrator_queue")
        ))
        .bind(now_ms)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get queue depths: {}", e))?;

        let worker_queue: i64 = sqlx::query_scalar(&format!(
            r#"
            SELECT COUNT(*) FROM {} 
            WHERE lock_token IS NULL OR locked_until <= $1
            "#,
            self.table_name("worker_queue")
        ))
        .bind(now_ms)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| format!("Failed to get queue depths: {}", e))?;

        Ok(QueueDepths {
            orchestrator_queue: orchestrator_queue as usize,
            worker_queue: worker_queue as usize,
            timer_queue: 0, // Timers are in orchestrator queue with delayed visibility
        })
    }
}

