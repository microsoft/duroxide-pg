# Duroxide PostgreSQL Provider

A PostgreSQL-based provider implementation for [Duroxide](https://github.com/microsoft/duroxide), a durable task orchestration framework for Rust.

> **Looking for durable functions directly in SQL?** That's [pg_durable](https://github.com/microsoft/pg_durable) — a PostgreSQL extension built on top of duroxide + duroxide-pg. duroxide-pg itself is the lower-level Rust building block: use it when you want to author your own workflows in Rust against a Postgres-backed store.

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

### Optional long-polling

Existing constructors use short polling. To reduce idle queries and wake workers
when work arrives, explicitly enable long-polling:

```rust,no_run
use duroxide_pg::{PostgresProvider, ProviderConfig};

# async fn example(database_url: &str) -> anyhow::Result<()> {
let mut config = ProviderConfig::url(database_url);
config.schema_name = Some("my_app".to_string());
config.long_poll.enabled = true;
let provider = PostgresProvider::new_with_config(config).await?;
# Ok(())
# }
```

The same setting works with `ProviderConfig::entra(...)`. Long-polling reserves
one connection **within** the provider's pool for PostgreSQL `LISTEN`, so the
pool must allow at least two connections. Leave it disabled for client-only
providers, especially one-connection backend clients. It requires a
session-preserving database connection; transaction-pooling proxies are not a
substitute for a dedicated LISTEN session.

`LongPollConfig` also provides `notifier_poll_interval` (default 60 seconds)
and `timer_grace_period` (default 100 milliseconds). The provider schedules
future work in both queues, rescans after a timer fires, and periodically wakes
fetches to recover missed notifications. Reconnection resubscribes and rechecks
work. Notifications are hints; queue predicates and locks remain authoritative.
Lost notifications and eligibility changes such as expired leases are also
recovered by subsequent fetches after the runtime's long-poll timeout.

An enabled fetch waits for work until its `poll_timeout`; a zero timeout still
performs a single fetch. The timeout bounds notification waiting, not an
in-flight database operation. Dropping the provider or closing its pool stops
the notification/timer task; connection cleanup has the network-failure
limitation described below.
The first notification wakes a fetch immediately. Further notifications that
find no eligible work, and timer catch-up scans, are coalesced to a 100 ms
cadence to bound query load. This can add up to 100 ms after an unsuccessful
wake; the overall polling deadline is not extended.

Migration `0027` installs **disabled** queue notification triggers. Constructing
a provider with both `MigrationPolicy::ApplyAll` and `long_poll.enabled = true`
activates them for that schema. `VerifyOnly` workers require that activation
first and perform no DDL themselves. See [Migration Policy](docs/migration-policy.md).

Once activated, inserts, visibility changes, and released locks emit
notifications, including writes from short-polling producers. Turning off a
provider's listener does not deactivate the schema's triggers. Transactions
writing those queues cannot use `PREPARE TRANSACTION`, because PostgreSQL does
not support preparing transactions that emit notifications. Schemas that have
not opted in retain their existing transaction behavior.

PostgreSQL serializes transactions that emit notifications through a
server-wide lock held until commit completes. Slow WAL flushes or synchronous
replication waits can therefore delay other notification writers, even in
other schemas or databases on that server. Benchmark concurrent writers with
representative commit latency before activating notifications; idle-query
savings alone do not establish loaded throughput.

Detected connection failures reconnect automatically, but silent network
partitions are not actively probed. Until the transport reports a failure,
work relies on the polling and refresh fallbacks instead of prompt
notifications. SQLx listener cleanup can also wait indefinitely on a wedged
socket, so provider drop does not guarantee bounded connection cleanup and
`pool.close()` has no hard shutdown deadline in that situation.

Recreate providers if their queue tables are dropped and recreated: notification
channels identify the original tables. Runtime session-capacity decisions are
also made once per fetch; a slot that started polling without session capacity
may need another fetch after its poll timeout before it can claim session work.

This feature does **not** convert a `duroxide-pg-opt` database into a
`duroxide-pg` database. Their migration histories remain incompatible.

## Features

- Automatic schema migration on startup
- Optional notification-driven long-polling for orchestration and activity work
- Connection pooling via sqlx
- Custom schema support for multi-tenant isolation
- Full implementation of the Duroxide `Provider` and `ProviderAdmin` traits
- Poison message detection with attempt count tracking
- Lock renewal for long-running orchestrations and activities
- KV store — durable per-instance key-value state for orchestration coordination
- Orchestration stats introspection via `Client::get_orchestration_stats()`
- Microsoft Entra ID authentication for Azure Database for PostgreSQL (managed identity, Workload Identity, az CLI)

## Latest Release (0.1.38)

- Fixes an orchestration acknowledge that could commit after another worker had taken the instance over. The consumed messages stayed queued and were processed again, so one queue message or external event could run an activity twice (migration `0026`).
- Fixes deadlocks between orchestration lock renewal and fetch, acknowledge or abandon, and stops renewal from reviving an expired lease or shortening a live one (migration `0026`).
- Retries transient renewal errors, and finds leases by lock token through a new unique index instead of a table scan.
- Adds a deadlock test suite that runs in every `cargo test`, and a TLA+ model of the lease procedures under worker clock skew (`docs/tla/orchestration-leases`).
- See [CHANGELOG.md](CHANGELOG.md) for full version history.

> **Releases are published by Microsoft's internal OSS infrastructure.** See
> [RELEASE_POLICY.md](RELEASE_POLICY.md) for details.

## Previous Release (0.1.37)

- Fails startup on catalog errors, inconsistent migration history, or missing, unexpected, or non-table Duroxide relations without deleting migration records or re-applying old SQL.
- Reports malformed KV snapshots through `history_error`, allowing the runtime to poison the instance after `max_attempts` instead of replaying an empty snapshot or retrying forever.
- Keeps malformed durable history out of orphan-message cleanup so its queue messages are not acknowledged before runtime poison/backoff handling.

## Support

Use GitHub Issues for bug reports and feature requests. Do not report security vulnerabilities through public GitHub issues; follow the instructions in [SECURITY.md](SECURITY.md) instead.

## Code of Conduct

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/). For more information, see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with questions or comments.

## Security

Microsoft takes the security of our software products and services seriously. Please do not report security vulnerabilities through public GitHub issues. See [SECURITY.md](SECURITY.md) for security reporting instructions.

## Privacy and Telemetry

duroxide-pg does not send telemetry to Microsoft. Applications may configure their own database, logging, or metrics collection; those signals are controlled by the application owner.

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft trademarks or logos is subject to and must follow [Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general). Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship. Any use of third-party trademarks or logos is subject to those third-party policies.

## License

MIT License - see [LICENSE](LICENSE) for details.
