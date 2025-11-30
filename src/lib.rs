//! # Duroxide PostgreSQL Provider
//!
//! A PostgreSQL-based provider implementation for [Duroxide](https://crates.io/crates/duroxide),
//! a durable task orchestration framework for Rust.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use duroxide_pg::PostgresProvider;
//! use duroxide::Worker;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create a provider with the database URL
//!     let provider = PostgresProvider::new("postgres://user:password@localhost:5432/mydb").await?;
//!
//!     // Use with a Duroxide worker
//!     let worker = Worker::new(provider);
//!     // ... register orchestrations and activities, then run
//!
//!     Ok(())
//! }
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

pub mod migrations;
pub mod provider;

pub use provider::PostgresProvider;
