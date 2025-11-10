use anyhow::Result;
use sqlx::PgPool;
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Migration metadata
#[derive(Debug)]
struct Migration {
    version: i64,
    name: String,
    sql: String,
}

/// Migration runner that handles schema-qualified migrations
pub struct MigrationRunner {
    pool: Arc<PgPool>,
    schema_name: String,
}

impl MigrationRunner {
    /// Create a new migration runner
    pub fn new(pool: Arc<PgPool>, schema_name: String) -> Self {
        Self { pool, schema_name }
    }

    /// Run all pending migrations
    pub async fn migrate(&self) -> Result<()> {
        // Ensure schema exists
        if self.schema_name != "public" {
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", self.schema_name))
                .execute(&*self.pool)
                .await?;
        }

        // Load migrations from filesystem
        let migrations = self.load_migrations()?;

        tracing::debug!(
            "Loaded {} migrations for schema {}",
            migrations.len(),
            self.schema_name
        );

        // Ensure migration tracking table exists (in the schema)
        self.ensure_migration_table().await?;

        // Get applied migrations
        let applied_versions = self.get_applied_versions().await?;

        tracing::debug!("Applied migrations: {:?}", applied_versions);

        // Check if key tables exist - if not, we need to re-run migrations even if marked as applied
        // This handles the case where cleanup dropped tables but not the migration tracking table
        let tables_exist = self.check_tables_exist().await.unwrap_or(false);

        // Apply pending migrations (or re-apply if tables don't exist)
        for migration in migrations {
            let should_apply = if !applied_versions.contains(&migration.version) {
                true // New migration
            } else if !tables_exist {
                // Migration was applied but tables don't exist - re-apply
                tracing::warn!(
                    "Migration {} is marked as applied but tables don't exist, re-applying",
                    migration.version
                );
                // Remove the old migration record so we can re-apply
                sqlx::query(&format!(
                    "DELETE FROM {}._duroxide_migrations WHERE version = $1",
                    self.schema_name
                ))
                .bind(migration.version)
                .execute(&*self.pool)
                .await?;
                true
            } else {
                false // Already applied and tables exist
            };

            if should_apply {
                tracing::debug!(
                    "Applying migration {}: {}",
                    migration.version,
                    migration.name
                );
                self.apply_migration(&migration).await?;
            } else {
                tracing::debug!(
                    "Skipping migration {}: {} (already applied)",
                    migration.version,
                    migration.name
                );
            }
        }

        Ok(())
    }

    /// Load migrations from the migrations directory
    fn load_migrations(&self) -> Result<Vec<Migration>> {
        // Try multiple possible locations for migrations directory
        let possible_paths: Vec<std::path::PathBuf> = vec![
            Path::new("migrations").to_path_buf(),
            Path::new("./migrations").to_path_buf(),
            Path::new("../migrations").to_path_buf(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"),
        ];

        let migrations_dir = possible_paths
            .iter()
            .find(|path| path.exists() && path.is_dir())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find migrations directory. Tried: {:?}",
                    possible_paths
                )
            })?;

        let mut migrations = Vec::new();

        let mut entries: Vec<_> = fs::read_dir(migrations_dir)?
            .filter_map(|entry| entry.ok())
            .collect();

        entries.sort_by_key(|e| e.path());

        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("sql") {
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    let sql = fs::read_to_string(&path)?;
                    let version = self.parse_version(file_name)?;
                    let name = file_name.to_string();

                    migrations.push(Migration { version, name, sql });
                }
            }
        }

        Ok(migrations)
    }

    /// Parse version number from migration filename (e.g., "0001_initial.sql" -> 1)
    fn parse_version(&self, filename: &str) -> Result<i64> {
        let version_str = filename
            .split('_')
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid migration filename: {}", filename))?;

        version_str
            .parse::<i64>()
            .map_err(|e| anyhow::anyhow!("Invalid migration version {}: {}", version_str, e))
    }

    /// Ensure migration tracking table exists
    async fn ensure_migration_table(&self) -> Result<()> {
        // Create migration table in the target schema
        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}._duroxide_migrations (
                version BIGINT PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            )
            "#,
            self.schema_name
        ))
        .execute(&*self.pool)
        .await?;

        Ok(())
    }

    /// Check if key tables exist
    async fn check_tables_exist(&self) -> Result<bool> {
        // Check if instances table exists (as a proxy for all tables)
        let exists: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = 'instances')",
        ))
        .bind(&self.schema_name)
        .fetch_one(&*self.pool)
        .await?;

        Ok(exists)
    }

    /// Get list of applied migration versions
    async fn get_applied_versions(&self) -> Result<Vec<i64>> {
        let versions: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT version FROM {}._duroxide_migrations ORDER BY version",
            self.schema_name
        ))
        .fetch_all(&*self.pool)
        .await?;

        Ok(versions)
    }

    /// Split SQL into statements, respecting dollar-quoted strings ($$...$$)
    /// This handles stored procedures and other constructs that use dollar-quoting
    fn split_sql_statements(sql: &str) -> Vec<String> {
        let mut statements = Vec::new();
        let mut current_statement = String::new();
        let chars: Vec<char> = sql.chars().collect();
        let mut i = 0;
        let mut in_dollar_quote = false;
        let mut dollar_tag: Option<String> = None;

        while i < chars.len() {
            let ch = chars[i];

            if !in_dollar_quote {
                // Check for start of dollar-quoted string
                if ch == '$' {
                    let mut tag = String::new();
                    tag.push(ch);
                    i += 1;

                    // Collect the tag (e.g., $$, $tag$, $function$)
                    while i < chars.len() {
                        let next_ch = chars[i];
                        if next_ch == '$' {
                            tag.push(next_ch);
                            dollar_tag = Some(tag.clone());
                            in_dollar_quote = true;
                            current_statement.push_str(&tag);
                            i += 1;
                            break;
                        } else if next_ch.is_alphanumeric() || next_ch == '_' {
                            tag.push(next_ch);
                            i += 1;
                        } else {
                            // Not a dollar quote, just a $ character
                            current_statement.push(ch);
                            break;
                        }
                    }
                } else if ch == ';' {
                    // End of statement (only if not in dollar quote)
                    current_statement.push(ch);
                    let trimmed = current_statement.trim().to_string();
                    if !trimmed.is_empty() {
                        statements.push(trimmed);
                    }
                    current_statement.clear();
                    i += 1;
                } else {
                    current_statement.push(ch);
                    i += 1;
                }
            } else {
                // Inside dollar-quoted string
                current_statement.push(ch);

                // Check for end of dollar-quoted string
                if ch == '$' {
                    let tag = dollar_tag.as_ref().unwrap();
                    let mut matches = true;

                    // Check if the following characters match the closing tag
                    for (j, tag_char) in tag.chars().enumerate() {
                        if j == 0 {
                            continue; // Skip first $ (we already matched it)
                        }
                        if i + j >= chars.len() || chars[i + j] != tag_char {
                            matches = false;
                            break;
                        }
                    }

                    if matches {
                        // Found closing tag - consume remaining tag characters
                        for _ in 0..(tag.len() - 1) {
                            if i + 1 < chars.len() {
                                current_statement.push(chars[i + 1]);
                                i += 1;
                            }
                        }
                        in_dollar_quote = false;
                        dollar_tag = None;
                    }
                }
                i += 1;
            }
        }

        // Add remaining statement if any
        let trimmed = current_statement.trim().to_string();
        if !trimmed.is_empty() {
            statements.push(trimmed);
        }

        statements
    }

    /// Apply a single migration
    async fn apply_migration(&self, migration: &Migration) -> Result<()> {
        // Start transaction
        let mut tx = self.pool.begin().await?;

        // Set search_path for this transaction
        sqlx::query(&format!("SET LOCAL search_path TO {}", self.schema_name))
            .execute(&mut *tx)
            .await?;

        // Remove comment lines and split SQL into individual statements
        let sql = migration.sql.trim();
        let cleaned_sql: String = sql
            .lines()
            .map(|line| {
                // Remove full-line comments
                if let Some(idx) = line.find("--") {
                    // Check if -- is inside a string (simple check)
                    let before = &line[..idx];
                    if before.matches('\'').count() % 2 == 0 {
                        // Even number of quotes means -- is not in a string
                        line[..idx].trim()
                    } else {
                        line
                    }
                } else {
                    line
                }
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        // Split by semicolon, but respect dollar-quoted strings ($$...$$)
        let statements = Self::split_sql_statements(&cleaned_sql);

        tracing::debug!(
            "Executing {} statements for migration {}",
            statements.len(),
            migration.version
        );

        for (idx, statement) in statements.iter().enumerate() {
            if !statement.trim().is_empty() {
                tracing::debug!(
                    "Executing statement {} of {}: {}...",
                    idx + 1,
                    statements.len(),
                    &statement.chars().take(50).collect::<String>()
                );
                sqlx::query(statement)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to execute statement {} in migration {}: {}\nStatement: {}",
                            idx + 1,
                            migration.version,
                            e,
                            statement
                        )
                    })?;
            }
        }

        // Record migration as applied
        sqlx::query(&format!(
            "INSERT INTO {}._duroxide_migrations (version, name) VALUES ($1, $2)",
            self.schema_name
        ))
        .bind(migration.version)
        .bind(&migration.name)
        .execute(&mut *tx)
        .await?;

        // Commit transaction
        tx.commit().await?;

        tracing::info!(
            "Applied migration {}: {}",
            migration.version,
            migration.name
        );

        Ok(())
    }
}
