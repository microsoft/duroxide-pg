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

### Microsoft Entra ID Authentication (Azure Database for PostgreSQL)

Connect to **Azure Database for PostgreSQL Flexible Server** using a Microsoft
Entra ID (Azure AD) access token instead of a static password. The provider
acquires the initial token at construction time and a background task
refreshes it before expiry, swapping the new token into the pool via
`sqlx::Pool::set_connect_options`.

```rust,no_run
use duroxide_pg::{EntraAuthOptions, PostgresProvider};

# async fn example() -> anyhow::Result<()> {
let provider = PostgresProvider::new_with_entra(
    "myserver.postgres.database.azure.com",
    5432,
    "mydb",
    "my-entra-principal@contoso.onmicrosoft.com",
    EntraAuthOptions::new(),
)
.await?;
# Ok(()) }
```

For multi-tenant deployments use the schema variant:

```rust,no_run
use duroxide_pg::{EntraAuthOptions, PostgresProvider};

# async fn example() -> anyhow::Result<()> {
let provider = PostgresProvider::new_with_schema_and_entra(
    "myserver.postgres.database.azure.com",
    5432,
    "mydb",
    "my-entra-principal@contoso.onmicrosoft.com",
    Some("tenant_a"),
    EntraAuthOptions::new(),
)
.await?;
# Ok(()) }
```

#### Identity sources

By default the provider chains `[WorkloadIdentityCredential (only when AKS
federated env vars are present), ManagedIdentityCredential,
DeveloperToolsCredential]` — so the same code works for:

- **AKS**: Workload Identity federation (preferred when
  `AZURE_FEDERATED_TOKEN_FILE` and friends are set).
- **Production**: User-assigned or system-assigned managed identity,
  App Service / Container Apps managed identity.
- **Local dev**: `az login` (Azure CLI) or `azd auth login`.

#### Required Azure setup

1. Configure an Entra admin on the Flexible Server (`az postgres flexible-server ad-admin set`).
2. Create a Postgres role for the principal:
   ```sql
   SELECT pgaadauth_create_principal('my-app-managed-identity', false, false);
   ```
3. Grant the role the privileges your application needs (`GRANT ... ON DATABASE
   ...`, `GRANT USAGE ON SCHEMA ...`, etc.).

#### Sovereign clouds

The default audience is the public-cloud value
`https://ossrdbms-aad.database.windows.net/.default`. Override for sovereign
clouds:

```rust,no_run
use std::time::Duration;
use duroxide_pg::EntraAuthOptions;

let options = EntraAuthOptions::new()
    .audience("https://ossrdbms-aad.database.usgovcloudapi.net/.default")
    .refresh_interval(Duration::from_secs(15 * 60));
```

#### Notes

- All Entra connections are pinned to `PgSslMode::VerifyFull`. There is no
  fallback to weaker TLS modes.
- Brief auth-failure windows during token rotation surface as **retryable**
  `ProviderError`s (SQLSTATE `28000` / `28P01`) so the runtime retries
  transparently.
- See the [Entra ID technical reference](https://github.com/microsoft/duroxide-pg/blob/main/.paw/work/entra-auth-support/Docs.md)
  for the design rationale (refresh scheduling, troubleshooting, dependency
  choices).

#### Testing

Two test layers cover the Entra integration:

1. **Local pipeline tests** (`cargo test --lib entra_pipeline`) — exercise the
   full token → connect-options → pool → migrations flow against a local
   PostgreSQL by injecting a fake `TokenSource` (no Azure dependency). They
   automatically skip if `DATABASE_URL` is not set.

2. **Live Entra smoke test** (`tests/entra_live_test.rs`, `#[ignore]`) — opt
   in by setting `DUROXIDE_PG_ENTRA_LIVE_TEST=1` plus
   `DUROXIDE_PG_ENTRA_TEST_HOST`, `DUROXIDE_PG_ENTRA_TEST_DB`, and
   `DUROXIDE_PG_ENTRA_TEST_USER`. Run with
   `cargo test --test entra_live_test -- --ignored`. Credentials are picked
   up from the ambient `azure_identity` chain.

   **First-time setup.** A pair of helper scripts provisions an
   ephemeral Azure Database for PostgreSQL Flexible Server (Burstable
   B1ms tier, ~$13/month if left running — remember to tear it down):

   ```bash
   az login
   ./scripts/provision_entra_test_pg.sh   # creates RG + server + firewall + Entra admin
   # (script prints the env-var block to copy into your shell)
   cargo test --test entra_live_test -- --ignored --nocapture
   ./scripts/teardown_entra_test_pg.sh    # deletes the resource group
   ```

   The scripts are idempotent and use the currently `az login`'d user as
   the Entra admin / test principal. Override naming with
   `DUROXIDE_PG_ENTRA_TEST_PREFIX`, `DUROXIDE_PG_ENTRA_TEST_LOCATION`,
   `DUROXIDE_PG_ENTRA_TEST_RG`, or `DUROXIDE_PG_ENTRA_TEST_SERVER` env
   vars (see the script headers for details).

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
- Microsoft Entra ID authentication for Azure Database for PostgreSQL (managed identity, Workload Identity, az CLI)

## Latest Release (0.1.31)

- Added Microsoft Entra ID authentication for Azure Database for PostgreSQL Flexible Server. New constructors `PostgresProvider::new_with_entra` and `PostgresProvider::new_with_schema_and_entra` accept an `EntraAuthOptions` and authenticate via Entra access tokens; a background task refreshes the token before expiry and swaps it into the connection pool.
- Default credential chain is `[WorkloadIdentityCredential (when AKS federated env vars are set), ManagedIdentityCredential, DeveloperToolsCredential]`, with `PgSslMode::VerifyFull` pinned for all Entra connections.
- See [CHANGELOG.md](CHANGELOG.md) for full version history.

## Previous Release (0.1.30)

- Switched SQLx TLS backend from `runtime-tokio-rustls` to `runtime-tokio-native-tls` (drops transitive `ring` crate dependency for FIPS compliance)
- Bumped duroxide dependency to `0.1.28` (also drops `ring`)
- Dropped local `path = "../../duroxide"` overrides; now resolves from crates.io only

## License

MIT License - see [LICENSE](LICENSE) for details.
