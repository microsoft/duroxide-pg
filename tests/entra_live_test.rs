//! Live Entra ID smoke test against a real Azure Database for PostgreSQL.
//!
//! This test is `#[ignore]` by default and is opt-in via the
//! `DUROXIDE_PG_ENTRA_LIVE_TEST=1` environment variable. It is intended to be
//! run manually (or in a dedicated CI lane with credentials configured), not
//! during normal `cargo test` runs.
//!
//! # Required environment variables when enabled
//!
//! - `DUROXIDE_PG_ENTRA_LIVE_TEST=1` — explicit opt-in
//! - `DUROXIDE_PG_ENTRA_TEST_HOST` — Azure PG hostname (e.g.
//!   `myserver.postgres.database.azure.com`)
//! - `DUROXIDE_PG_ENTRA_TEST_DB`   — database name
//! - `DUROXIDE_PG_ENTRA_TEST_USER` — Entra principal name (Azure AD user or
//!   group display name configured as a PG role)
//!
//! Optional:
//! - `DUROXIDE_PG_ENTRA_TEST_PORT` — defaults to `5432`
//! - `DUROXIDE_PG_ENTRA_TEST_SCHEMA` — defaults to a generated unique schema
//!
//! Credentials are picked up from the ambient environment (Azure CLI,
//! managed identity, environment variables, etc.) via the default
//! [`azure_identity`] credential chain — same path production callers use.
//!
//! # Run
//!
//! ```pwsh
//! $env:DUROXIDE_PG_ENTRA_LIVE_TEST = "1"
//! $env:DUROXIDE_PG_ENTRA_TEST_HOST = "myserver.postgres.database.azure.com"
//! $env:DUROXIDE_PG_ENTRA_TEST_DB   = "postgres"
//! $env:DUROXIDE_PG_ENTRA_TEST_USER = "alice@contoso.com"
//! cargo test --test entra_live_test -- --ignored --nocapture
//! ```

use duroxide_pg::{EntraAuthOptions, PostgresProvider};
use sqlx::Row;

const ENABLE_VAR: &str = "DUROXIDE_PG_ENTRA_LIVE_TEST";
const HOST_VAR: &str = "DUROXIDE_PG_ENTRA_TEST_HOST";
const DB_VAR: &str = "DUROXIDE_PG_ENTRA_TEST_DB";
const USER_VAR: &str = "DUROXIDE_PG_ENTRA_TEST_USER";
const PORT_VAR: &str = "DUROXIDE_PG_ENTRA_TEST_PORT";
const SCHEMA_VAR: &str = "DUROXIDE_PG_ENTRA_TEST_SCHEMA";

fn unique_schema() -> String {
    let g = uuid::Uuid::new_v4().to_string();
    format!("entra_live_{}", &g[g.len() - 8..])
}

fn require_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{ENABLE_VAR}=1 was set, but {name} is missing"))
}

#[tokio::test]
#[ignore]
async fn entra_live_smoke_test() {
    dotenvy::dotenv().ok();

    if std::env::var(ENABLE_VAR).ok().as_deref() != Some("1") {
        eprintln!(
            "Skipping live Entra smoke test: set {ENABLE_VAR}=1 plus \
             {HOST_VAR}, {DB_VAR}, {USER_VAR} to enable."
        );
        return;
    }

    let host = require_env(HOST_VAR);
    let database = require_env(DB_VAR);
    let user = require_env(USER_VAR);
    let port: u16 = std::env::var(PORT_VAR)
        .ok()
        .map(|v| v.parse().expect("invalid DUROXIDE_PG_ENTRA_TEST_PORT"))
        .unwrap_or(5432);
    let schema = std::env::var(SCHEMA_VAR).unwrap_or_else(|_| unique_schema());

    eprintln!(
        "Live Entra smoke test: host={host} port={port} db={database} user={user} schema={schema}"
    );

    let provider = PostgresProvider::new_with_schema_and_entra(
        &host,
        port,
        &database,
        &user,
        Some(&schema),
        EntraAuthOptions::new(),
    )
    .await
    .expect("provider construction with default Entra credential chain must succeed");

    // Use a basic query to prove the pool is functional and the schema/migrations
    // were applied. We look for one of the well-known tables migrations create.
    let row = sqlx::query("SELECT to_regclass($1) IS NOT NULL AS exists")
        .bind(format!("{schema}.instances"))
        .fetch_one(provider.pool())
        .await
        .expect("query against live Azure PG must succeed");
    let exists: bool = row.get("exists");
    assert!(exists, "instances table must exist in test schema after migrations");

    // Best-effort cleanup. Failure here doesn't fail the test — the schema is
    // unique per run and easily dropped manually.
    let drop_sql = format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE");
    if let Err(e) = sqlx::query(&drop_sql).execute(provider.pool()).await {
        eprintln!("warning: failed to drop test schema {schema}: {e:#}");
    }
}
