//! Behavioral verification that pinning `pg_temp` last in the migration
//! `search_path` (see `MigrationRunner::migration_search_path_stmt`) prevents a
//! same-named temporary object from shadowing the unqualified references a
//! migration relies on the `search_path` to resolve.
//!
//! The unit test `migration_search_path_pins_pg_temp_last` locks the *string*
//! the runner emits; this test proves that string has the intended PostgreSQL
//! name-resolution semantics. It is `#[ignore]`-d because it needs a live
//! database (`DATABASE_URL`).

use sqlx::{Connection, PgConnection, Row};

fn get_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

fn unique_schema() -> String {
    let guid = uuid::Uuid::new_v4().to_string();
    format!("sp_test_{}", &guid[guid.len() - 8..])
}

/// In a single connection we create a real `widget` table in `<schema>` and a
/// temporary `widget` that would shadow it. We then show that:
///   1. with `SET LOCAL search_path TO <schema>, pg_temp` (the runner's
///      statement) an unqualified `widget` resolves to the SCHEMA table, and
///   2. the legacy `SET LOCAL search_path TO <schema>` (pg_temp implicit first)
///      resolves the same reference to the TEMP table — the behavior the fix
///      hardens against.
#[tokio::test]
#[ignore = "requires a live PostgreSQL (DATABASE_URL)"]
async fn pg_temp_pinned_last_does_not_shadow_unqualified_reference() {
    let url = get_database_url();
    let schema = unique_schema();
    let mut conn = PgConnection::connect(&url).await.expect("connect");

    // Real object in the target schema, tagged so we can tell it apart.
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut conn)
        .await
        .expect("create schema");
    sqlx::query(&format!("CREATE TABLE {schema}.widget (src text NOT NULL)"))
        .execute(&mut conn)
        .await
        .expect("create schema table");
    sqlx::query(&format!(
        "INSERT INTO {schema}.widget (src) VALUES ('schema')"
    ))
    .execute(&mut conn)
    .await
    .expect("seed schema table");

    // A temporary `widget` that shadows it unless pg_temp is demoted.
    sqlx::query("CREATE TEMP TABLE widget (src text NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create temp table");
    sqlx::query("INSERT INTO widget (src) VALUES ('temp')")
        .execute(&mut conn)
        .await
        .expect("seed temp table");

    // (1) The runner's statement: pg_temp pinned last -> schema wins.
    let safe_stmt = format!("SET LOCAL search_path TO {schema}, pg_temp");
    let mut tx = conn.begin().await.expect("begin safe tx");
    sqlx::query(&safe_stmt)
        .execute(&mut *tx)
        .await
        .expect("set safe search_path");
    let resolved: String = sqlx::query("SELECT src FROM widget LIMIT 1")
        .fetch_one(&mut *tx)
        .await
        .expect("read unqualified widget (safe)")
        .get("src");
    tx.rollback().await.expect("rollback safe tx");
    assert_eq!(
        resolved, "schema",
        "with pg_temp pinned last, an unqualified reference must resolve to the schema object"
    );

    // (2) Legacy statement: pg_temp implicit first -> temp shadows the schema.
    let legacy_stmt = format!("SET LOCAL search_path TO {schema}");
    let mut tx = conn.begin().await.expect("begin legacy tx");
    sqlx::query(&legacy_stmt)
        .execute(&mut *tx)
        .await
        .expect("set legacy search_path");
    let shadowed: String = sqlx::query("SELECT src FROM widget LIMIT 1")
        .fetch_one(&mut *tx)
        .await
        .expect("read unqualified widget (legacy)")
        .get("src");
    tx.rollback().await.expect("rollback legacy tx");
    assert_eq!(
        shadowed, "temp",
        "the legacy search_path leaves pg_temp first, so the temp object shadows the schema \
         object — this is the behavior the fix hardens against"
    );

    // Cleanup (temp table disappears with the session).
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut conn)
        .await
        .expect("drop schema");
    conn.close().await.ok();
}
