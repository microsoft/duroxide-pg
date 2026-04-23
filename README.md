# Duroxide PostgreSQL Provider

A PostgreSQL-based provider implementation for [Duroxide](https://github.com/microsoft/duroxide), a durable task orchestration framework for Rust.

> **Note:** See [CHANGELOG.md](CHANGELOG.md) for version history and breaking changes.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
duroxide-pg = "0.1"
```

## Usage

```rust
use duroxide_pg::PostgresProvider;
use duroxide::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a provider with the database URL
    let provider = PostgresProvider::new("postgres://user:password@localhost:5432/mydb").await?;

    // Use with a Duroxide worker
    let worker = Worker::new(provider);
    // ... register orchestrations and activities, then run
    
    Ok(())
}
```

### Custom Schema

To isolate data in a specific PostgreSQL schema:

```rust
let provider = PostgresProvider::new_with_schema(
    "postgres://user:password@localhost:5432/mydb",
    Some("my_schema"),
).await?;
```

## Configuration

| Environment Variable | Description | Default |
|---------------------|-------------|---------|
| `DUROXIDE_PG_POOL_MAX` | Maximum connection pool size | `10` |

## Features

- Automatic schema migration on startup
- Connection pooling via sqlx
- Custom schema support for multi-tenant isolation
- Full implementation of the Duroxide `Provider` and `ProviderAdmin` traits
- Poison message detection with attempt count tracking
- Lock renewal for long-running orchestrations and activities
- KV store — durable per-instance key-value state for orchestration coordination
- Orchestration stats introspection via `Client::get_orchestration_stats()`

## Latest Release (0.1.30)

- Switched SQLx TLS backend from `runtime-tokio-rustls` to `runtime-tokio-native-tls` (drops transitive `ring` crate dependency for FIPS compliance)
- Bumped duroxide dependency to `0.1.28` (also drops `ring`)
- Dropped local `path = "../../duroxide"` overrides; now resolves from crates.io only
- See [CHANGELOG.md](CHANGELOG.md) for full version history

## License

MIT License - see [LICENSE](LICENSE) for details.
