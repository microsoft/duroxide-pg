//! # Duroxide PostgreSQL Provider
//!
//! A PostgreSQL-based provider implementation for [Duroxide](https://crates.io/crates/duroxide),
//! a durable task orchestration framework for Rust.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use duroxide_pg::PostgresProvider;
//! use duroxide::runtime::Runtime;
//! use std::sync::Arc;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Create a provider with the database URL
//! let provider = PostgresProvider::new("postgres://user:password@localhost:5432/mydb").await?;
//!
//! // Use with the Duroxide runtime
//! // let runtime = Runtime::start_with_store(Arc::new(provider), activity_registry, orchestration_registry).await;
//! # Ok(())
//! # }
//! ```
//!
//! ## Custom Schema
//!
//! To isolate data in a specific PostgreSQL schema (useful for multi-tenant deployments):
//!
//! ```rust,no_run
//! use duroxide_pg::PostgresProvider;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let provider = PostgresProvider::new_with_schema(
//!     "postgres://user:password@localhost:5432/mydb",
//!     Some("my_schema"),
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Microsoft Entra ID Authentication
//!
//! Connect to Azure Database for PostgreSQL Flexible Server using an Entra
//! access token. A background task refreshes the token before expiry. See
//! [`EntraAuthOptions`] for tunables.
//!
//! ```rust,no_run
//! use duroxide_pg::{EntraAuthOptions, PostgresProvider};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let provider = PostgresProvider::new_with_entra(
//!     "myserver.postgres.database.azure.com",
//!     5432,
//!     "mydb",
//!     "my-entra-principal@contoso.onmicrosoft.com",
//!     EntraAuthOptions::new(),
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! All Entra connections use `PgSslMode::VerifyFull`. The default credential
//! chain is `[WorkloadIdentityCredential (added only when AKS federated env
//! vars are present), ManagedIdentityCredential, DeveloperToolsCredential]`,
//! which works for AKS Workload Identity, other Azure-hosted managed
//! identities, and developer workstations logged in via `az login`.
//!
//! ## Configuration
//!
//! | Environment Variable | Description | Default |
//! |---------------------|-------------|---------|
//! | `DUROXIDE_PG_POOL_MAX` | Maximum connection pool size | `10` |
//!
//! ## Features
//!
//! - Automatic schema migration on startup
//! - Connection pooling via sqlx
//! - Custom schema support for multi-tenant isolation
//! - Full implementation of the Duroxide `Provider` and `ProviderAdmin` traits

pub mod entra;
pub mod migrations;
pub mod provider;

pub use entra::EntraAuthOptions;
pub use provider::PostgresProvider;
