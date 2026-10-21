// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for [`MigrationPolicy`] / [`ProviderConfig`] / `new_with_config`.
//!
//! These tests verify that:
//! 1. `MigrationPolicy::ApplyAll` (the default) creates schema + tables, matching
//!    pre-feature behavior.
//! 2. `MigrationPolicy::VerifyOnly` succeeds against a schema where migrations
//!    have already been applied.
//! 3. `MigrationPolicy::VerifyOnly` returns an error against an uninitialized
//!    schema, without creating any objects.

use duroxide_pg::{MigrationPolicy, PostgresProvider, ProviderConfig};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set in environment or .env file")
}

fn get_test_schema() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("test_migpolicy_{suffix}")
}

async fn drop_schema(schema_name: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&get_database_url())
        .await
        .expect("connect for cleanup");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop schema");
}

async fn schema_exists(schema_name: &str) -> bool {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&get_database_url())
        .await
        .expect("connect for schema check");
    let row = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM information_schema.schemata WHERE schema_name = $1) AS e",
    )
    .bind(schema_name)
    .fetch_one(&pool)
    .await
    .expect("query schema existence");
    row.get::<bool, _>("e")
}

#[tokio::test]
async fn apply_all_creates_schema_and_tables() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // `ProviderConfig::url` defaults to `MigrationPolicy::ApplyAll`,
    // so this also smoke-tests the default policy via the new API.
    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    let _provider = PostgresProvider::new_with_config(config)
        .await
        .expect("ApplyAll should succeed against a fresh schema");

    assert!(
        schema_exists(&schema).await,
        "schema {schema} should exist after ApplyAll"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn verify_only_succeeds_against_initialized_schema() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // First, apply migrations.
    let _bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    // Then construct a VerifyOnly provider against the same schema.
    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let _verify = PostgresProvider::new_with_config(config)
        .await
        .expect("VerifyOnly should succeed when migrations are up to date");

    drop_schema(&schema).await;
}

#[tokio::test]
async fn verify_only_errors_against_uninitialized_schema() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;

    let err = match result {
        Ok(_) => panic!("VerifyOnly must fail when the target schema has no migrations applied"),
        Err(e) => e.to_string(),
    };

    assert!(
        err.contains("not initialized") || err.contains("_duroxide_migrations"),
        "error should mention missing migration table; got: {err}"
    );

    // VerifyOnly must not create the schema.
    assert!(
        !schema_exists(&schema).await,
        "VerifyOnly should not create schema {schema}"
    );
}

#[tokio::test]
async fn verify_only_rejects_unknown_migrations() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Bring the schema up to date via ApplyAll.
    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    // Insert an "unknown" migration version directly into the tracking table.
    sqlx::query(&format!(
        "INSERT INTO {schema}._duroxide_migrations (version, name) VALUES ($1, $2)"
    ))
    .bind(9_999_i64)
    .bind("9999_future.sql")
    .execute(bootstrap.pool())
    .await
    .expect("insert unknown migration row");
    drop(bootstrap);

    // VerifyOnly must refuse to claim a schema that is ahead of the code.
    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;
    let msg = match result {
        Ok(_) => panic!("VerifyOnly must reject unknown applied migrations"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not recognized") && msg.contains("9999"),
        "expected ahead-of-code error mentioning the unknown version; got: {msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_unknown_migrations_causes_no_mutations() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Fully initialize the schema.
    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    // Snapshot the applied migrations before the perturbation.
    let before: Vec<(i64, String)> = sqlx::query_as(&format!(
        "SELECT version, name FROM {schema}._duroxide_migrations ORDER BY version"
    ))
    .fetch_all(bootstrap.pool())
    .await
    .expect("read migrations");
    let last_version = before.last().expect("at least one bundled migration").0;

    // Create a "pending migration" condition by removing the last real
    // migration record, drop a core table to disable the re-apply-if-missing
    // path, then insert an unknown future migration.
    sqlx::query(&format!(
        "DELETE FROM {schema}._duroxide_migrations WHERE version = $1"
    ))
    .bind(last_version)
    .execute(bootstrap.pool())
    .await
    .expect("delete last migration");

    sqlx::query(&format!("DROP TABLE {schema}.instances"))
        .execute(bootstrap.pool())
        .await
        .expect("drop instances table");

    sqlx::query(&format!(
        "INSERT INTO {schema}._duroxide_migrations (version, name) VALUES ($1, $2)"
    ))
    .bind(9_999_i64)
    .bind("9999_future.sql")
    .execute(bootstrap.pool())
    .await
    .expect("insert unknown migration row");

    drop(bootstrap);

    // ApplyAll must short-circuit on the unknown migration and run no DDL.
    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must reject unknown applied migrations"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not recognized") && msg.contains("9999"),
        "expected ahead-of-code error; got: {msg}"
    );

    // Prove no DDL fired: the deleted real migration is still absent and the
    // dropped core table is still missing.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for verification");

    let after_versions: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT version FROM {schema}._duroxide_migrations"
    ))
    .fetch_all(&pool)
    .await
    .expect("read migrations after rejection");
    assert!(
        !after_versions.contains(&last_version),
        "migrate_inner should not have re-applied migration {last_version}; \
         current set: {after_versions:?}"
    );

    let instances_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = 'instances')",
    )
    .bind(&schema)
    .fetch_one(&pool)
    .await
    .expect("check instances table after rejection");
    assert!(
        !instances_exists,
        "migrate_inner should not have recreated the instances table"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn schema_name_validation_rejects_unsafe_identifiers() {
    let database_url = get_database_url();

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some("bad-name".to_string());

    match PostgresProvider::new_with_config(config).await {
        Ok(_) => panic!("Expected schema name validation to fail"),
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("Invalid schema_name"),
                "expected validation error, got: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn verify_only_errors_when_schema_missing() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Belt-and-braces: make sure the schema is absent.
    drop_schema(&schema).await;

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;
    let msg = match result {
        Ok(_) => panic!("VerifyOnly should fail when the target schema does not exist"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not initialized") || msg.contains("_duroxide_migrations"),
        "expected missing-schema/missing-tracking-table error, got: {msg}"
    );

    // VerifyOnly must not create the schema as a side effect.
    assert!(
        !schema_exists(&schema).await,
        "VerifyOnly must not create schema {schema}"
    );
}

#[tokio::test]
async fn verify_only_errors_when_tracking_table_missing() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Create a bare schema with no tables (no migrations applied).
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for setup");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create bare schema");

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;
    let msg = match result {
        Ok(_) => panic!("VerifyOnly should fail when the tracking table is missing"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not initialized") && msg.contains("_duroxide_migrations"),
        "expected tracking-table-missing error, got: {msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn verify_only_errors_when_migrations_behind() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Fully initialize, then synthesize a "behind" state by deleting the
    // most recent migration record without dropping the tracking table.
    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    let last_version: i64 = sqlx::query_scalar(&format!(
        "SELECT MAX(version) FROM {schema}._duroxide_migrations"
    ))
    .fetch_one(bootstrap.pool())
    .await
    .expect("read max version");

    sqlx::query(&format!(
        "DELETE FROM {schema}._duroxide_migrations WHERE version = $1"
    ))
    .bind(last_version)
    .execute(bootstrap.pool())
    .await
    .expect("delete most-recent migration row");

    drop(bootstrap);

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;
    let msg = match result {
        Ok(_) => panic!("VerifyOnly should fail when migrations are behind"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not up to date") && msg.contains(&last_version.to_string()),
        "expected behind-schema error mentioning version {last_version}; got: {msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn verify_only_errors_when_core_tables_are_missing() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Fully initialize, then leave migration records intact while dropping a
    // core table. VerifyOnly must not trust the tracking table alone.
    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    sqlx::query(&format!("DROP TABLE {schema}.instances"))
        .execute(bootstrap.pool())
        .await
        .expect("drop instances table");

    drop(bootstrap);

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result = PostgresProvider::new_with_config(config).await;
    let msg = match result {
        Ok(_) => panic!("VerifyOnly should fail when core tables are missing"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("core tables are missing") || msg.contains("corrupted"),
        "expected missing-core-tables error, got: {msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_refuses_to_reapply_when_core_tables_are_missing() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    let before_versions: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT version FROM {schema}._duroxide_migrations ORDER BY version"
    ))
    .fetch_all(bootstrap.pool())
    .await
    .expect("read migration ledger before corruption");
    let signature = format!("{schema}.fetch_orchestration_item(bigint,bigint,bigint,bigint)");
    let before_function: String =
        sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1)::oid)")
            .bind(&signature)
            .fetch_one(bootstrap.pool())
            .await
            .expect("read fetch function before corruption");

    sqlx::query(&format!("DROP TABLE {schema}.instances CASCADE"))
        .execute(bootstrap.pool())
        .await
        .expect("drop instances table");
    drop(bootstrap);

    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must not re-apply recorded migrations"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("instances")
            && msg.contains("missing")
            && msg.contains("refusing to apply migrations"),
        "expected a fail-closed corruption error, got: {msg}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for post-failure verification");
    let after_versions: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT version FROM {schema}._duroxide_migrations ORDER BY version"
    ))
    .fetch_all(&pool)
    .await
    .expect("read migration ledger after rejection");
    let after_function: String =
        sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1)::oid)")
            .bind(&signature)
            .fetch_one(&pool)
            .await
            .expect("read fetch function after rejection");

    assert_eq!(
        after_versions, before_versions,
        "ApplyAll must not delete or rewrite migration ledger rows"
    );
    assert_eq!(
        after_function, before_function,
        "ApplyAll must not roll stored procedures back to older definitions"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_rejects_a_hole_in_migration_history_before_ddl() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");

    sqlx::query(&format!(
        "DELETE FROM {schema}._duroxide_migrations WHERE version = 19"
    ))
    .execute(bootstrap.pool())
    .await
    .expect("remove migration 19 from the ledger");
    drop(bootstrap);

    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must reject a non-prefix migration history"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("migration history is inconsistent")
            && msg.contains("19")
            && msg.contains("refusing to apply migrations"),
        "expected a no-DDL migration-history error, got: {msg}"
    );
    assert!(
        !msg.contains("Failed to execute statement"),
        "the runner attempted migration DDL before rejecting the ledger: {msg}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for post-failure verification");
    let version_19_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {schema}._duroxide_migrations WHERE version = 19"
    ))
    .fetch_one(&pool)
    .await
    .expect("check migration 19 after rejection");
    assert_eq!(
        version_19_count, 0,
        "the runner must not repair migration history implicitly"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_rejects_missing_tracking_table_without_creating_a_new_ledger() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    sqlx::query(&format!("DROP TABLE {schema}._duroxide_migrations"))
        .execute(bootstrap.pool())
        .await
        .expect("drop migration tracking table");
    drop(bootstrap);

    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must reject live tables without a migration ledger"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("instances")
            && msg.contains("tracking table is missing")
            && msg.contains("refusing"),
        "expected missing-ledger corruption error, got: {msg}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for post-failure verification");
    let tracking_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = '_duroxide_migrations')",
    )
    .bind(&schema)
    .fetch_one(&pool)
    .await
    .expect("check tracking table after rejection");
    assert!(
        !tracking_exists,
        "the runner must not create a replacement ledger beside live tables"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_rejects_an_empty_ledger_beside_live_tables() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    let signature = format!("{schema}.fetch_orchestration_item(bigint,bigint,bigint,bigint)");
    let before_function: String =
        sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1)::oid)")
            .bind(&signature)
            .fetch_one(bootstrap.pool())
            .await
            .expect("read fetch function before corruption");

    sqlx::query(&format!("TRUNCATE TABLE {schema}._duroxide_migrations"))
        .execute(bootstrap.pool())
        .await
        .expect("empty migration ledger");
    drop(bootstrap);

    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must reject an empty ledger beside live tables"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("not represented by its migration ledger") && msg.contains("refusing"),
        "expected empty-ledger corruption error, got: {msg}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for post-failure verification");
    let ledger_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {schema}._duroxide_migrations"
    ))
    .fetch_one(&pool)
    .await
    .expect("count ledger rows after rejection");
    let after_function: String =
        sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1)::oid)")
            .bind(&signature)
            .fetch_one(&pool)
            .await
            .expect("read fetch function after rejection");

    assert_eq!(ledger_count, 0, "the empty ledger must remain untouched");
    assert_eq!(
        after_function, before_function,
        "stored procedures must not be rolled back"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_rejects_residual_tables_when_ledger_and_instances_are_missing() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    sqlx::query(&format!(
        "DROP TABLE {schema}._duroxide_migrations, {schema}.instances CASCADE"
    ))
    .execute(bootstrap.pool())
    .await
    .expect("drop ledger and instances while leaving other provider tables");
    drop(bootstrap);

    let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let msg = match result {
        Ok(_) => panic!("ApplyAll must reject residual provider tables"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("executions")
            && msg.contains("tracking table is missing")
            && msg.contains("refusing"),
        "expected residual-table corruption error, got: {msg}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect for post-failure verification");
    let tracking_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_schema = $1 AND table_name = '_duroxide_migrations')",
    )
    .bind(&schema)
    .fetch_one(&pool)
    .await
    .expect("check tracking table after rejection");
    assert!(!tracking_exists);

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_rejects_each_missing_required_table() {
    let database_url = get_database_url();
    let required_tables = [
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

    for table in required_tables {
        let schema = get_test_schema();
        let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
            .await
            .expect("bootstrap apply");
        let before_versions: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT version FROM {schema}._duroxide_migrations ORDER BY version"
        ))
        .fetch_all(bootstrap.pool())
        .await
        .expect("read migration ledger before corruption");
        sqlx::query(&format!("DROP TABLE {schema}.{table} CASCADE"))
            .execute(bootstrap.pool())
            .await
            .unwrap_or_else(|e| panic!("drop {table}: {e}"));
        drop(bootstrap);

        let result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
        let msg = match result {
            Ok(_) => panic!("ApplyAll must reject missing table {table}"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(table) && msg.contains("missing") && msg.contains("refusing"),
            "{table}: expected required-table corruption error, got: {msg}"
        );

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect for post-failure verification");
        let after_versions: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT version FROM {schema}._duroxide_migrations ORDER BY version"
        ))
        .fetch_all(&pool)
        .await
        .expect("read migration ledger after rejection");
        assert_eq!(
            after_versions, before_versions,
            "{table}: migration ledger changed after rejection"
        );

        drop_schema(&schema).await;
    }
}

#[tokio::test]
async fn migration_policies_reject_a_view_substituted_for_a_required_table() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    sqlx::query(&format!("DROP TABLE {schema}.history"))
        .execute(bootstrap.pool())
        .await
        .expect("drop history table");
    sqlx::query(&format!(
        "CREATE VIEW {schema}.history AS SELECT 1::BIGINT AS event_id"
    ))
    .execute(bootstrap.pool())
    .await
    .expect("substitute history view");
    drop(bootstrap);

    let apply_result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let apply_msg = match apply_result {
        Ok(_) => panic!("ApplyAll must reject a view substituted for history"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        apply_msg.contains("history")
            && apply_msg.contains("\"v\"")
            && apply_msg.contains("non-table"),
        "expected relation-kind error from ApplyAll, got: {apply_msg}"
    );

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    let verify_result = PostgresProvider::new_with_config(config).await;
    let verify_msg = match verify_result {
        Ok(_) => panic!("VerifyOnly must reject a view substituted for history"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        verify_msg.contains("history")
            && verify_msg.contains("\"v\"")
            && verify_msg.contains("non-table"),
        "expected relation-kind error from VerifyOnly, got: {verify_msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn migration_policies_reject_a_view_substituted_for_the_ledger() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    sqlx::query(&format!("DROP TABLE {schema}._duroxide_migrations"))
        .execute(bootstrap.pool())
        .await
        .expect("drop migration ledger");
    sqlx::query(&format!(
        "CREATE VIEW {schema}._duroxide_migrations AS \
         SELECT 1::BIGINT AS version, 'substitute'::TEXT AS name, \
         CURRENT_TIMESTAMP AS applied_at"
    ))
    .execute(bootstrap.pool())
    .await
    .expect("substitute migration ledger view");
    drop(bootstrap);

    let apply_result = PostgresProvider::new_with_schema(&database_url, Some(&schema)).await;
    let apply_msg = match apply_result {
        Ok(_) => panic!("ApplyAll must reject a view substituted for the ledger"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        apply_msg.contains("tracking object")
            && apply_msg.contains("\"v\"")
            && apply_msg.contains("not a base table"),
        "expected tracking relation-kind error from ApplyAll, got: {apply_msg}"
    );

    let mut config = ProviderConfig::url(&database_url);
    config.schema_name = Some(schema.clone());
    config.migration_policy = MigrationPolicy::VerifyOnly;
    let verify_result = PostgresProvider::new_with_config(config).await;
    let verify_msg = match verify_result {
        Ok(_) => panic!("VerifyOnly must reject a view substituted for the ledger"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        verify_msg.contains("tracking object")
            && verify_msg.contains("\"v\"")
            && verify_msg.contains("not a base table"),
        "expected tracking relation-kind error from VerifyOnly, got: {verify_msg}"
    );

    drop_schema(&schema).await;
}

#[tokio::test]
async fn apply_all_still_applies_a_missing_suffix_migration() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let bootstrap = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("bootstrap apply");
    let latest_version: i64 = sqlx::query_scalar(&format!(
        "SELECT MAX(version) FROM {schema}._duroxide_migrations"
    ))
    .fetch_one(bootstrap.pool())
    .await
    .expect("read latest migration version");
    sqlx::query(&format!(
        "DELETE FROM {schema}._duroxide_migrations WHERE version = $1"
    ))
    .bind(latest_version)
    .execute(bootstrap.pool())
    .await
    .expect("remove latest migration row");
    drop(bootstrap);

    let provider = PostgresProvider::new_with_schema(&database_url, Some(&schema))
        .await
        .expect("ApplyAll should apply a missing suffix migration");
    let restored_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {schema}._duroxide_migrations WHERE version = $1"
    ))
    .bind(latest_version)
    .fetch_one(provider.pool())
    .await
    .expect("check restored suffix migration");
    assert_eq!(restored_count, 1);

    drop(provider);
    drop_schema(&schema).await;
}

#[tokio::test]
async fn concurrent_apply_all_is_serialized() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    // Ensure a clean slate.
    drop_schema(&schema).await;

    let make_config = |schema: String| {
        let mut cfg = ProviderConfig::url(&database_url);
        cfg.schema_name = Some(schema);
        cfg.migration_policy = MigrationPolicy::ApplyAll;
        cfg
    };

    let h1 = {
        let cfg = make_config(schema.clone());
        tokio::spawn(async move { PostgresProvider::new_with_config(cfg).await })
    };
    let h2 = {
        let cfg = make_config(schema.clone());
        tokio::spawn(async move { PostgresProvider::new_with_config(cfg).await })
    };

    let (r1, r2) = tokio::join!(h1, h2);
    let p1 = r1
        .expect("task 1 panicked")
        .expect("concurrent ApplyAll #1 should succeed under the advisory lock");
    let _p2 = r2
        .expect("task 2 panicked")
        .expect("concurrent ApplyAll #2 should succeed under the advisory lock");

    // Sanity check: the schema is fully migrated.
    let row_count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {schema}._duroxide_migrations"
    ))
    .fetch_one(p1.pool())
    .await
    .expect("count applied migrations");
    assert!(
        row_count.0 > 0,
        "expected at least one applied migration after concurrent ApplyAll"
    );

    drop(p1);
    drop_schema(&schema).await;
}
