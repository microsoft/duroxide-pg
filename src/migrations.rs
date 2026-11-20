// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Result;
use include_dir::{include_dir, Dir};
use sqlx::Connection;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Update this marker when adding SQL to refresh incremental embeds: 0029.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

const BASE_TABLES: &[&str] = &[
    "instances",
    "executions",
    "history",
    "orchestrator_queue",
    "worker_queue",
    "instance_locks",
];
const ALL_CORE_TABLES: &[&str] = &[
    "instances",
    "executions",
    "history",
    "orchestrator_queue",
    "worker_queue",
    "instance_locks",
    "sessions",
    "kv_store",
    "kv_delta",
];

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

    fn advisory_lock_key(&self) -> i64 {
        // Stable 64-bit FNV-1a hash over (namespace + schema name).
        // This avoids using Rust's DefaultHasher (randomized per-process).
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;

        let mut hash = OFFSET;
        for b in b"duroxide_pg:migrations:" {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        for b in self.schema_name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(PRIME);
        }

        hash as i64
    }

    async fn lock_for_migrations(&self, conn: &mut sqlx::postgres::PgConnection) -> Result<()> {
        let key = self.advisory_lock_key();
        // Session lock (not xact lock) so it spans multiple transactions.
        // We explicitly unlock at the end.
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn unlock_for_migrations(&self, conn: &mut sqlx::postgres::PgConnection) {
        let key = self.advisory_lock_key();
        // Best-effort unlock.
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(key)
            .execute(&mut *conn)
            .await;
    }

    /// Run all pending migrations
    pub async fn migrate(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let conn = &mut *conn;
        self.lock_for_migrations(conn).await?;

        // Reject unknown migrations while holding the advisory lock so that an
        // older binary cannot rewrite a schema that is ahead of its code.
        // Short-circuit: do NOT run migrate_inner if unknown migrations are
        // detected.
        let result = match self.check_no_unknown_migrations(conn).await {
            Ok(()) => self.migrate_inner(conn).await,
            Err(e) => Err(e),
        };
        self.unlock_for_migrations(conn).await;

        result
    }

    /// Verify that the migration tracking table exists and that every embedded
    /// migration has already been applied. Does not take the migration
    /// advisory lock and does not create or modify any database objects.
    ///
    /// Returns an error if the `_duroxide_migrations` table is missing in
    /// `schema_name` or if any bundled migration version is absent from it.
    ///
    /// Intended for processes that must not perform DDL (e.g. application
    /// backends, where a separately privileged worker is responsible for
    /// applying schema changes).
    pub async fn verify(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let conn = &mut *conn;

        // Check that the tracking table exists in the target schema.
        let table_exists = self.check_migration_table_exists(conn).await?;

        if !table_exists {
            anyhow::bail!(
                "duroxide migrations not initialized: schema {:?} does not \
                 contain _duroxide_migrations. Construct a provider with \
                 MigrationPolicy::ApplyAll (the default) from a process with \
                 DDL privileges before using MigrationPolicy::VerifyOnly.",
                self.schema_name
            );
        }

        // Reject schemas that have migrations the running binary does not
        // recognize (schema is ahead of the code).
        self.check_no_unknown_migrations(conn).await?;

        let migrations = Self::load_migrations()?;
        let applied = self.get_applied_versions(conn).await?;
        self.validate_applied_prefix(&migrations, &applied)?;
        let existing_tables = self.get_existing_core_relations(conn).await?;
        self.validate_core_table_state(&applied, &existing_tables)?;
        let applied: std::collections::HashSet<i64> = applied.into_iter().collect();

        let mut missing: Vec<i64> = migrations
            .iter()
            .map(|m| m.version)
            .filter(|v| !applied.contains(v))
            .collect();
        missing.sort_unstable();

        if !missing.is_empty() {
            anyhow::bail!(
                "duroxide migrations not up to date in schema {:?}: missing \
                 versions {:?}. Run migrations from a provider configured with \
                 MigrationPolicy::ApplyAll before constructing VerifyOnly \
                 providers.",
                self.schema_name,
                missing,
            );
        }

        Ok(())
    }

    /// Check that the database has no migrations the running binary does not
    /// recognize. Used by both `migrate()` (to refuse running DDL against a
    /// schema ahead of the code) and `verify()` (to refuse claiming
    /// successful verification of an unknown schema).
    ///
    /// Returns `Ok(())` if the tracking table does not yet exist: under
    /// `ApplyAll` it will be created by `migrate_inner`, and under
    /// `VerifyOnly` the missing table is reported separately before this is
    /// called.
    async fn check_no_unknown_migrations(
        &self,
        conn: &mut sqlx::postgres::PgConnection,
    ) -> Result<()> {
        if !self.check_migration_table_exists(conn).await? {
            return Ok(());
        }

        let applied = self.get_applied_versions(conn).await?;
        let expected: std::collections::HashSet<i64> = Self::load_migrations()?
            .into_iter()
            .map(|m| m.version)
            .collect();

        let mut unknown: Vec<i64> = applied
            .into_iter()
            .filter(|v| !expected.contains(v))
            .collect();
        unknown.sort_unstable();

        if !unknown.is_empty() {
            anyhow::bail!(
                "schema {:?} has migrations not recognized by this version of \
                 the code: {:?}. The database schema is ahead of the code. \
                 Update the code to a compatible version.",
                self.schema_name,
                unknown,
            );
        }

        Ok(())
    }

    async fn check_migration_table_exists(
        &self,
        conn: &mut sqlx::postgres::PgConnection,
    ) -> Result<bool> {
        let relation_kind: Option<String> = sqlx::query_scalar(
            "SELECT c.relkind::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = '_duroxide_migrations'",
        )
        .bind(&self.schema_name)
        .fetch_optional(&mut *conn)
        .await?;

        match relation_kind.as_deref() {
            None => Ok(false),
            Some("r" | "p") => Ok(true),
            Some(kind) => anyhow::bail!(
                "duroxide migration tracking object in schema {:?} has PostgreSQL \
                 relation kind {:?}, not a base table; refusing to read or replace it",
                self.schema_name,
                kind,
            ),
        }
    }

    fn validate_applied_prefix(
        &self,
        migrations: &[Migration],
        applied_versions: &[i64],
    ) -> Result<()> {
        for (index, (migration, applied)) in
            migrations.iter().zip(applied_versions.iter()).enumerate()
        {
            if migration.version != *applied {
                anyhow::bail!(
                    "duroxide migration history is inconsistent in schema {:?}: \
                     expected version {} at ledger position {}, but found version {}. \
                     Applied migrations must be an ordered prefix of the bundled \
                     migrations; refusing to apply migrations or modify the ledger.",
                    self.schema_name,
                    migration.version,
                    index + 1,
                    applied,
                );
            }
        }

        if applied_versions.len() > migrations.len() {
            anyhow::bail!(
                "duroxide migration history is inconsistent in schema {:?}: \
                 the ledger contains {} entries, but this binary bundles only {}. \
                 Refusing to apply migrations or modify the ledger.",
                self.schema_name,
                applied_versions.len(),
                migrations.len(),
            );
        }

        Ok(())
    }

    fn expected_core_tables(applied_versions: &[i64]) -> HashSet<String> {
        let Some(latest) = applied_versions.last().copied() else {
            return HashSet::new();
        };

        let mut expected: HashSet<String> =
            BASE_TABLES.iter().map(|name| (*name).to_string()).collect();
        if latest >= 14 {
            expected.insert("sessions".to_string());
        }
        if latest >= 18 {
            expected.insert("kv_store".to_string());
        }
        if latest >= 20 {
            expected.insert("kv_delta".to_string());
        }
        expected
    }

    fn validate_core_table_state(
        &self,
        applied_versions: &[i64],
        existing_relations: &HashMap<String, String>,
    ) -> Result<()> {
        let expected_tables = Self::expected_core_tables(applied_versions);
        let mut missing: Vec<&str> = expected_tables
            .iter()
            .filter(|name| !existing_relations.contains_key(*name))
            .map(String::as_str)
            .collect();
        let mut wrong_kind: Vec<(&str, &str)> = expected_tables
            .iter()
            .filter_map(|name| {
                existing_relations.get(name).and_then(|kind| {
                    (!matches!(kind.as_str(), "r" | "p")).then_some((name.as_str(), kind.as_str()))
                })
            })
            .collect();
        let mut unexpected: Vec<&str> = existing_relations
            .keys()
            .filter(|name| !expected_tables.contains(*name))
            .map(String::as_str)
            .collect();
        missing.sort_unstable();
        wrong_kind.sort_unstable();
        unexpected.sort_unstable();

        if !wrong_kind.is_empty() {
            anyhow::bail!(
                "duroxide schema {:?} has required relations {:?} with non-table \
                 PostgreSQL relation kinds. The schema may be corrupted; refusing \
                 to apply migrations or modify the ledger automatically.",
                self.schema_name,
                wrong_kind,
            );
        }

        if !missing.is_empty() {
            anyhow::bail!(
                "duroxide migration history in schema {:?} requires tables {:?}, \
                 but they are missing. The schema may be corrupted; refusing to \
                 apply migrations or modify the ledger automatically.",
                self.schema_name,
                missing,
            );
        }

        if !unexpected.is_empty() {
            anyhow::bail!(
                "duroxide schema {:?} contains tables {:?} that are not represented \
                 by its migration ledger. The schema may be corrupted; refusing to \
                 apply migrations or modify the ledger automatically.",
                self.schema_name,
                unexpected,
            );
        }

        Ok(())
    }

    async fn migrate_inner(&self, conn: &mut sqlx::postgres::PgConnection) -> Result<()> {
        let schema_exists = self.check_schema_exists(conn).await?;
        if !schema_exists && self.schema_name != "public" {
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", self.schema_name))
                .execute(&mut *conn)
                .await?;
        }

        // Load migrations from filesystem
        let migrations = Self::load_migrations()?;

        tracing::debug!(
            "Loaded {} migrations for schema {}",
            migrations.len(),
            self.schema_name
        );

        let tracking_exists = self.check_migration_table_exists(conn).await?;
        let existing_tables = self.get_existing_core_relations(conn).await?;

        if !tracking_exists && !existing_tables.is_empty() {
            let mut relations: Vec<(&str, &str)> = existing_tables
                .iter()
                .map(|(name, kind)| (name.as_str(), kind.as_str()))
                .collect();
            relations.sort_unstable();
            anyhow::bail!(
                "duroxide relations {:?} exist in schema {:?}, but the migration \
                 tracking table is missing. The schema may be corrupted; refusing \
                 to create a new ledger or apply migrations automatically.",
                relations,
                self.schema_name,
            );
        }

        if !tracking_exists {
            self.ensure_migration_table(conn).await?;
        }

        let applied_versions = self.get_applied_versions(conn).await?;
        self.validate_applied_prefix(&migrations, &applied_versions)?;
        self.validate_core_table_state(&applied_versions, &existing_tables)?;
        tracing::debug!("Applied migrations: {:?}", applied_versions);

        for migration in migrations.into_iter().skip(applied_versions.len()) {
            tracing::debug!(
                "Applying migration {}: {}",
                migration.version,
                migration.name
            );
            self.apply_migration(conn, &migration).await?;
        }

        Ok(())
    }

    /// Load migrations from the embedded migrations directory
    fn load_migrations() -> Result<Vec<Migration>> {
        let mut migrations = Vec::new();

        // Get all files from embedded directory
        let mut files: Vec<_> = MIGRATIONS
            .files()
            .filter(|file| file.path().extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .collect();

        // Sort by path to ensure consistent ordering
        files.sort_by_key(|f| f.path());

        for file in files {
            let file_name = file
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("Invalid filename in migrations"))?;

            let sql = file
                .contents_utf8()
                .ok_or_else(|| anyhow::anyhow!("Migration file is not valid UTF-8: {file_name}"))?
                .to_string();

            let version = Self::parse_version(file_name)?;
            let name = file_name.to_string();

            migrations.push(Migration { version, name, sql });
        }

        Ok(migrations)
    }

    /// Parse version number from migration filename (e.g., "0001_initial.sql" -> 1)
    fn parse_version(filename: &str) -> Result<i64> {
        let version_str = filename
            .split('_')
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid migration filename: {filename}"))?;

        version_str
            .parse::<i64>()
            .map_err(|e| anyhow::anyhow!("Invalid migration version {version_str}: {e}"))
    }

    /// Ensure migration tracking table exists
    async fn ensure_migration_table(&self, conn: &mut sqlx::postgres::PgConnection) -> Result<()> {
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
        .execute(&mut *conn)
        .await?;

        Ok(())
    }

    async fn check_schema_exists(&self, conn: &mut sqlx::postgres::PgConnection) -> Result<bool> {
        let exists = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1)",
        )
        .bind(&self.schema_name)
        .fetch_one(&mut *conn)
        .await?;

        Ok(exists)
    }

    async fn get_existing_core_relations(
        &self,
        conn: &mut sqlx::postgres::PgConnection,
    ) -> Result<HashMap<String, String>> {
        let known_tables: Vec<String> = ALL_CORE_TABLES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let relations: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text, c.relkind::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = ANY($2::text[])",
        )
        .bind(&self.schema_name)
        .bind(&known_tables)
        .fetch_all(&mut *conn)
        .await?;

        Ok(relations.into_iter().collect())
    }

    /// Get list of applied migration versions
    async fn get_applied_versions(
        &self,
        conn: &mut sqlx::postgres::PgConnection,
    ) -> Result<Vec<i64>> {
        let versions: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT version FROM {}._duroxide_migrations ORDER BY version",
            self.schema_name
        ))
        .fetch_all(&mut *conn)
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

    /// Build the `SET LOCAL search_path` statement used while applying a migration.
    ///
    /// `pg_temp` is pinned last so it sits at the lowest priority instead of its
    /// implicit highest-priority position, preventing a temporary object from
    /// shadowing the unqualified references a migration resolves via the
    /// `search_path`. This is defense-in-depth (CVE-2018-1058); `pg_catalog` is left
    /// unlisted so PostgreSQL keeps it implicitly first.
    ///
    /// `schema_name` is validated at provider construction
    /// (`^[A-Za-z_][A-Za-z0-9_]*$`), so direct interpolation here is safe.
    fn migration_search_path_stmt(schema_name: &str) -> String {
        format!("SET LOCAL search_path TO {schema_name}, pg_temp")
    }

    /// Apply a single migration
    async fn apply_migration(
        &self,
        conn: &mut sqlx::postgres::PgConnection,
        migration: &Migration,
    ) -> Result<()> {
        // Start transaction
        let mut tx = conn.begin().await?;

        // Set search_path for this transaction.
        //
        // `pg_temp` is pinned explicitly at the lowest priority. Without it,
        // `pg_temp` keeps its implicit highest-priority position, which would let a
        // temporary object shadow the unqualified references this migration relies on
        // the search_path to resolve. This is defense-in-depth (CVE-2018-1058
        // guidance); see `migration_search_path_stmt` for the threat model.
        sqlx::query(&Self::migration_search_path_stmt(&self.schema_name))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_migration_versions_are_unique_and_increasing() {
        let migrations = MigrationRunner::load_migrations().expect("load embedded migrations");
        assert!(!migrations.is_empty(), "expected embedded SQL migrations");
        for pair in migrations.windows(2) {
            assert!(
                pair[0].version < pair[1].version,
                "embedded migrations must have unique, increasing versions: {} then {}",
                pair[0].name,
                pair[1].name,
            );
        }
    }

    #[test]
    fn migration_search_path_pins_pg_temp_last() {
        // The security property: pg_temp must be present and last, so temporary
        // objects cannot shadow the unqualified references a migration relies on
        // the search_path to resolve.
        let stmt = MigrationRunner::migration_search_path_stmt("duroxide");
        assert_eq!(stmt, "SET LOCAL search_path TO duroxide, pg_temp");
    }
}
