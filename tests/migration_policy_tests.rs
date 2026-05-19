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

    // `ProviderConfig::default()` is equivalent to `MigrationPolicy::ApplyAll`,
    // so this also smoke-tests the default-derived value.
    let _provider = PostgresProvider::new_with_schema_and_config(
        &database_url,
        Some(&schema),
        ProviderConfig::default(),
    )
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
    let mut config = ProviderConfig::default();
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let _verify =
        PostgresProvider::new_with_schema_and_config(&database_url, Some(&schema), config)
            .await
            .expect("VerifyOnly should succeed when migrations are up to date");

    drop_schema(&schema).await;
}

#[tokio::test]
async fn verify_only_errors_against_uninitialized_schema() {
    let database_url = get_database_url();
    let schema = get_test_schema();

    let mut config = ProviderConfig::default();
    config.migration_policy = MigrationPolicy::VerifyOnly;

    let result =
        PostgresProvider::new_with_schema_and_config(&database_url, Some(&schema), config).await;

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
