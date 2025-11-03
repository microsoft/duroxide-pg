use duroxide::providers::{Provider, WorkItem, OrchestrationItem, ExecutionMetadata};
use duroxide::Event;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::Arc;
use uuid::Uuid;
use anyhow::Result;
use std::time::{SystemTime, UNIX_EPOCH};

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
        let schema_prefix = format!("{}.", self.schema_name);

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}instances (
                instance_id TEXT PRIMARY KEY,
                orchestration_name TEXT NOT NULL,
                orchestration_version TEXT NOT NULL,
                current_execution_id BIGINT NOT NULL DEFAULT 1,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}executions (
                instance_id TEXT NOT NULL,
                execution_id BIGINT NOT NULL,
                status TEXT NOT NULL DEFAULT 'Running',
                output TEXT,
                started_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                completed_at TIMESTAMPTZ,
                PRIMARY KEY (instance_id, execution_id)
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}history (
                instance_id TEXT NOT NULL,
                execution_id BIGINT NOT NULL,
                event_id BIGINT NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (instance_id, execution_id, event_id)
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}orchestrator_queue (
                id BIGSERIAL PRIMARY KEY,
                instance_id TEXT NOT NULL,
                work_item TEXT NOT NULL,
                visible_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                lock_token TEXT,
                locked_until BIGINT,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}worker_queue (
                id BIGSERIAL PRIMARY KEY,
                work_item TEXT NOT NULL,
                lock_token TEXT,
                locked_until BIGINT,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}instance_locks (
                instance_id TEXT PRIMARY KEY,
                lock_token TEXT NOT NULL,
                locked_until BIGINT NOT NULL,
                locked_at BIGINT NOT NULL
            )
            "#,
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        // Create indexes
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_visible ON {}orchestrator_queue(visible_at, lock_token)",
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_instance ON {}orchestrator_queue(instance_id)",
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_orch_lock ON {}orchestrator_queue(lock_token)",
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_worker_available ON {}worker_queue(lock_token, id)",
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_instance_locks_locked_until ON {}instance_locks(locked_until)",
            schema_prefix
        ))
        .execute(&*self.pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_history_lookup ON {}history(instance_id, execution_id, event_id)",
            schema_prefix
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
}

#[async_trait::async_trait]
impl Provider for PostgresProvider {
    async fn fetch_orchestration_item(&self) -> Option<OrchestrationItem> {
        // TODO: Implement fetch_orchestration_item
        // 1. Find next available message in orchestrator queue
        // 2. Lock ALL messages for that instance
        // 3. Load instance metadata (name, version, execution_id)
        // 4. Load history for current execution_id
        // 5. Return OrchestrationItem with unique lock_token
        todo!("Implement fetch_orchestration_item")
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
        // TODO: Implement atomic commit
        // ALL operations must be atomic (single transaction)
        todo!("Implement ack_orchestration_item")
    }

    async fn abandon_orchestration_item(
        &self,
        lock_token: &str,
        delay_ms: Option<u64>,
    ) -> Result<(), String> {
        // TODO: Release lock and optionally delay visibility
        todo!("Implement abandon_orchestration_item")
    }

    async fn read(&self, instance: &str) -> Vec<Event> {
        // Get latest execution ID
        let execution_id: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT current_execution_id FROM {}instances WHERE instance_id = $1",
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
            "SELECT event_data FROM {}history WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
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
                "INSERT INTO {}history (instance_id, execution_id, event_id, event_type, event_data) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (instance_id, execution_id, event_id) DO NOTHING",
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
        // TODO: Add item to worker queue
        todo!("Implement enqueue_worker_work")
    }

    async fn dequeue_worker_peek_lock(&self) -> Option<(WorkItem, String)> {
        // TODO: Find next unlocked item, lock it, return item + token
        todo!("Implement dequeue_worker_peek_lock")
    }

    async fn ack_worker(&self, token: &str, completion: WorkItem) -> Result<(), String> {
        // TODO: Atomically delete item and enqueue completion
        todo!("Implement ack_worker")
    }

    async fn enqueue_orchestrator_work(
        &self,
        item: WorkItem,
        delay_ms: Option<u64>,
    ) -> Result<(), String> {
        // TODO: Enqueue orchestrator work item with optional delay
        todo!("Implement enqueue_orchestrator_work")
    }

    async fn latest_execution_id(&self, instance: &str) -> Option<u64> {
        // TODO: Return MAX(execution_id) for instance
        None
    }

    async fn read_with_execution(&self, instance: &str, execution_id: u64) -> Vec<Event> {
        let event_data_rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT event_data FROM {}history WHERE instance_id = $1 AND execution_id = $2 ORDER BY event_id",
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

    async fn list_instances(&self) -> Vec<String> {
        // TODO: Return all instance IDs
        Vec::new()
    }

    async fn list_executions(&self, instance: &str) -> Vec<u64> {
        let h = self.read(instance).await;
        if h.is_empty() {
            Vec::new()
        } else {
            vec![1]
        }
    }
}

