//! Repro for https://github.com/microsoft/duroxide/issues/10
//!
//! When multiple workers start simultaneously against a fresh database,
//! all attempt concurrent schema migrations. duroxide-pg has no advisory
//! lock around migration execution, so the second worker crashes with:
//!
//!   duplicate key value violates unique constraint "_duroxide_migrations_pkey"

mod common;

use duroxide_pg::PostgresProvider;

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn next_schema_name(prefix: &str) -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    let suffix = &guid[guid.len() - 8..];
    format!("{prefix}_{suffix}")
}

/// Two concurrent `new_with_schema` calls on the same empty schema must both
/// succeed. This test reproduces the race from issue #10 — without an advisory
/// lock, one of the two will hit a PK violation on `_duroxide_migrations` or
/// a concurrent schema creation error.
#[tokio::test]
async fn concurrent_startup_migration_race() {
    let database_url = get_database_url();
    let schema = next_schema_name("race_mig");

    // Ensure clean slate — pre-create the schema so the race is specifically
    // on migration execution, not CREATE SCHEMA.
    common::cleanup_schema(&schema).await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to connect");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .expect("Failed to pre-create schema");
    drop(pool);

    let url1 = database_url.clone();
    let url2 = database_url.clone();
    let s1 = schema.clone();
    let s2 = schema.clone();

    // Spawn two concurrent provider initializations on the same fresh schema.
    let h1 = tokio::spawn(async move {
        PostgresProvider::new_with_schema(&url1, Some(&s1)).await
    });
    let h2 = tokio::spawn(async move {
        PostgresProvider::new_with_schema(&url2, Some(&s2)).await
    });

    let (r1, r2) = tokio::join!(h1, h2);

    let p1_result = r1.expect("task 1 panicked");
    let p2_result = r2.expect("task 2 panicked");

    // Both providers should succeed. Without the advisory lock fix, one of
    // these will fail with a duplicate key violation on _duroxide_migrations.
    let _p1 = p1_result.expect("Provider #1 should succeed");
    let _p2 = p2_result.expect("Provider #2 should succeed");

    // Cleanup
    common::cleanup_schema(&schema).await;
}

/// Same race but with more concurrency (6 workers, matching the issue report).
#[tokio::test]
async fn concurrent_startup_migration_race_6_workers() {
    let database_url = get_database_url();
    let schema = next_schema_name("race6_mig");

    // Ensure clean slate — pre-create the schema so the race is specifically
    // on migration execution, not CREATE SCHEMA.
    common::cleanup_schema(&schema).await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to connect");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .expect("Failed to pre-create schema");
    drop(pool);

    let mut handles = Vec::new();
    for _ in 0..6 {
        let url = database_url.clone();
        let s = schema.clone();
        handles.push(tokio::spawn(async move {
            PostgresProvider::new_with_schema(&url, Some(&s)).await
        }));
    }

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut failure_messages = Vec::new();

    for (i, handle) in handles.into_iter().enumerate() {
        let result = handle.await.expect(&format!("task {i} panicked"));
        match result {
            Ok(_) => success_count += 1,
            Err(e) => {
                failure_count += 1;
                failure_messages.push(format!("Worker {i}: {e:#}"));
            }
        }
    }

    if failure_count > 0 {
        panic!(
            "Migration race condition reproduced! {success_count} succeeded, \
             {failure_count} failed.\nFailures:\n{}",
            failure_messages.join("\n")
        );
    }

    // Cleanup
    common::cleanup_schema(&schema).await;
}
