# Code Research: Azure Entra Authentication for PostgresProvider

**Work ID**: entra-auth-support
**Spec**: `.paw/work/entra-auth-support/Spec.md`
**Repo**: `microsoft/duroxide-pg`

This document captures concrete code-path, dependency, and API research needed
to plan an Entra-authenticated construction path for `PostgresProvider`. It
makes no design decisions — only inventories what the planner will need.

---

## 1. Current `PostgresProvider` Construction Path

### Public API surface (must remain unchanged per FR-006)

`src/lib.rs:55` re-exports the only public type:

```rust
pub use provider::PostgresProvider;
```

`src/provider.rs:41-44` — struct definition:

```rust
pub struct PostgresProvider {
    pool: Arc<PgPool>,
    schema_name: String,
}
```

`src/provider.rs:46-76` — the only constructors:

```rust
impl PostgresProvider {
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::new_with_schema(database_url, None).await
    }

    pub async fn new_with_schema(
        database_url: &str,
        schema_name: Option<&str>,
    ) -> Result<Self> {
        let max_connections = std::env::var("DUROXIDE_PG_POOL_MAX")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(10);

        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(database_url)
            .await?;

        let schema_name = schema_name.unwrap_or("public").to_string();

        let provider = Self {
            pool: Arc::new(pool),
            schema_name: schema_name.clone(),
        };

        // Run migrations to initialize schema
        let migration_runner = MigrationRunner::new(provider.pool.clone(), schema_name.clone());
        migration_runner.migrate().await?;

        Ok(provider)
    }
    // ...
}
```

Other public methods on the type (must also remain): `initialize_schema`,
`pool() -> &PgPool`, `schema_name() -> &str`, `cleanup_schema`. None of them
care how the pool was constructed.

### Hook points for new constructors

A new Entra-flavored constructor only needs to produce the same
`Arc<PgPool>` and `schema_name: String`, then drive the **identical**
migration step (`MigrationRunner::new(pool, schema).migrate().await`). All
behavior after construction is unchanged.

Hard-coded behaviors that any new path must preserve or explicitly replace:

| Concern | Current value | Where |
|---|---|---|
| Max pool size default | `10` | `provider.rs:55` |
| Max pool size override | env `DUROXIDE_PG_POOL_MAX` | `provider.rs:52-55` |
| Min pool connections | `1` | `provider.rs:59` |
| Acquire timeout | `30s` | `provider.rs:60` |
| Connection input | parsed from URL via `PgPoolOptions::connect(&str)` | `provider.rs:61` |
| Migrations on first construction | always run | `provider.rs:72-73` |

FR-010 explicitly asks the new path to expose these pool tunables.

### Public type imports (top of provider.rs:1-15)

```rust
use sqlx::{postgres::PgPoolOptions, Error as SqlxError, PgPool};
```

Anything new (e.g., `PgConnectOptions`, `PgSslMode`) must be added here.

### Crate-level rustdoc (`src/lib.rs:1-50`)

Currently shows two examples — `new` and `new_with_schema`. Per FR-011 / SC-007
this header doc must gain an Entra example block.

---

## 2. sqlx 0.8 Integration Points for Per-Connection Token Injection

### **There is no `before_connect` hook in sqlx 0.8.**

Verified against the v0.8.6 source
(`https://raw.githubusercontent.com/launchbadge/sqlx/v0.8.6/sqlx-core/src/pool/options.rs`).
The only callback hooks on `PoolOptions` are:

- `after_connect(callback)` — runs SQL after a new physical connection is up
- `before_acquire(callback)` — runs on idle connection before handing it out
- `after_release(callback)` — runs when a connection is returned to the pool

Web search results that claim a `before_connect` callback (returning
`BoxFuture` and mutating `PgConnectOptions`) **are wrong / hallucinated for
sqlx 0.8** — the planner should not rely on them. Code-search of
`launchbadge/sqlx` for `before_connect` returns zero hits.

### What sqlx 0.8 *does* provide for dynamic credentials

`sqlx_core::pool::Pool` exposes (from v0.8.6 `sqlx-core/src/pool/pool.rs`):

```rust
pub fn connect_options(&self) -> Arc<<DB::Connection as Connection>::Options>
pub fn set_connect_options(
    &self,
    connect_options: <DB::Connection as Connection>::Options,
)
```

Internally the pool stores connect options behind an `RwLock<Arc<...>>`
(`sqlx-core/src/pool/inner.rs`):

```rust
pub(super) connect_options: RwLock<Arc<<DB::Connection as Connection>::Options>>,
```

When the pool needs to open a brand-new physical connection it reads from this
lock — i.e., **swapping the options at runtime affects every subsequent new
connection**. Existing connections are unaffected.

This is the documented sqlx 0.8 path for "rotate password" use cases such as
AWS RDS IAM tokens; the same pattern is the natural fit for Entra tokens. A
typical shape (illustrative — *not* a design decision):

1. Acquire an initial Entra token.
2. Build `PgConnectOptions::new().host(...).port(...).database(...).username(...).password(token).ssl_mode(VerifyFull)` and call `PgPoolOptions::connect_with(opts)` (or `connect_lazy_with`).
3. Spawn a background task that periodically (e.g., on a fraction of the token's `expires_on`) acquires a fresh token and calls `pool.set_connect_options(new_opts_with_fresh_password)`.

### `PgConnectOptions` builder (sqlx 0.8.6, `sqlx-postgres/src/options/mod.rs`)

Verified relevant methods:

```rust
pub struct PgConnectOptions { /* … */ }
impl PgConnectOptions {
    pub fn password(mut self, password: &str) -> Self
    pub fn ssl_mode(mut self, mode: PgSslMode) -> Self
    pub fn ssl_root_cert(mut self, cert: impl AsRef<Path>) -> Self
    pub fn ssl_root_cert_from_pem(mut self, pem_certificate: Vec<u8>) -> Self
}
```

`PgConnectOptions` also implements `FromStr`, so a caller-supplied URL can be
parsed and then mutated (`PgConnectOptions::from_str(url)?.password(token)`).

### `PgSslMode` (sqlx-postgres/src/options/ssl_mode.rs)

```rust
pub enum PgSslMode {
    Disable, Allow, Prefer /* default */, Require, VerifyCa, VerifyFull,
}
```

`VerifyFull` does CA + hostname verification — what FR-005 / SC-006 require.
No `ssl_root_cert` is needed when the server presents a cert chain rooted in
the system trust store, which Azure Database for PostgreSQL Flexible Server
does (DigiCert Global Root G2 etc.). The planner can decide whether to
require an explicit `ssl_root_cert` or rely on system roots.

### TLS backend feature flag

`Cargo.toml:34`:

```toml
sqlx = { version = "0.8", features = ["runtime-tokio-native-tls", "postgres", "chrono"], default-features = false }
```

`runtime-tokio-native-tls` uses the platform's native TLS stack
(`schannel` on Windows, `Secure Transport` on macOS, `openssl` on Linux). All
of these support hostname verification, which is what `PgSslMode::VerifyFull`
relies on. README (line 65-66) explicitly notes the switch from
`runtime-tokio-rustls` to `runtime-tokio-native-tls` was deliberate (FIPS,
no `ring`). Adding Entra support **must not** flip this back.

---

## 3. Azure Identity Crate Options for Rust

### Primary candidate: `azure_identity` (Microsoft official, `Azure/azure-sdk-for-rust`)

- **Crate**: `azure_identity` on crates.io
  (https://crates.io/crates/azure_identity).
- **Version index probe** (crates.io sparse index):
  latest published versions visible: `0.31.0`, `0.32.0`, `0.33.0`. The crate
  tracks the broader Azure SDK for Rust; current ecosystem version range is
  `0.22.0` (Mar 2025) up through `0.33.0+` (recent). It is still pre-1.0; the
  Azure SDK for Rust is in beta.
- **Companion crate**: `azure_core` at the matching minor version. Dependency
  example from the index: `azure_identity 0.31.0` requires
  `azure_core ^0.31.0`.
- **Source**: https://github.com/Azure/azure-sdk-for-rust/tree/main/sdk/identity/azure_identity
- **Docs**: https://docs.rs/azure_identity/

### Key API surface (per docs.rs/azure_identity/latest)

- `DeveloperToolsCredential::new(None)?` — chained credential designed for
  local dev. Tries Azure CLI, Azure Developer CLI, etc. (Older versions
  exposed this as `DefaultAzureCredential`; modern versions split developer
  vs. production chains.)
- The crate also exposes individual credentials (`AzureCliCredential`,
  `ManagedIdentityCredential`, `WorkloadIdentityCredential`,
  `AzureDeveloperCliCredential`, `ClientAssertionCredential`,
  `ClientSecretCredential`, etc.).
- All credentials implement `azure_core::credentials::TokenCredential`, an
  async trait whose contract is:

  ```rust
  async fn get_token(
      &self,
      scopes: &[&str],
      options: Option<TokenRequestOptions /* or similar */>,
  ) -> azure_core::Result<AccessToken>;
  ```

  `AccessToken` carries `token: Secret<String>` plus `expires_on: OffsetDateTime`.
  Tokens are cached internally per credential; repeat `get_token` calls within
  the validity window are cheap.

> **Note**: The exact name of the "default chained" credential changed between
> versions (`DefaultAzureCredential` in older releases, `DeveloperToolsCredential`
> + composite chains in newer). The planner must verify the chosen pinned
> version's exact symbol from docs.rs at the time of pinning.

### Tokio compatibility

The crate is async and works on tokio (`tokio = "1"` with `full` is already a
dependency, `Cargo.toml:33`). `azure_identity` uses `async-trait` and (from
the version-index dump) pulls in `async-lock = "^3.0"`. No conflict expected.

### MSRV

`azure_identity 0.31` and surrounding versions require recent stable Rust.
This project's `Cargo.toml` does not pin a `rust-version`, so MSRV is whatever
crates.io / `duroxide` 0.1.28 requires. Should be fine in practice; the
planner may want to confirm by running `cargo check` on the chosen version.

### Alternatives surveyed

- **Legacy `azure-identity` (hyphenated, 0.x archived)** — predecessor
  Azure SDK for Rust effort by Microsoft. Effectively superseded by
  `azure_identity`; not recommended.
- **Community crates** (e.g., `azure_auth`, `oauth2`) — too low-level for
  the stated requirements (managed identity + workload identity + CLI fallback
  out of the box).
- **Hand-rolling the IMDS / federated-token calls** — meets FR-001 but blows
  up scope; spec assumes a `DefaultAzureCredential`-equivalent (Spec line
  103).

### Recommendation surface (for the planner — *not* a design decision here)

The most plausible pin is `azure_identity = "0.X"` matched to the latest
`azure_core` release at planning time, both pinned to the same minor to
avoid type-mismatches. Both crates are pre-1.0 and have churned across
minor versions (Risk #2 in spec).

---

## 4. Token Audience / Scope Strings for Azure Postgres

Per Microsoft Learn ("Use Microsoft Entra ID for authentication with Azure
Database for PostgreSQL — Flexible Server", referenced in Spec line 144):

| Cloud | Resource URI | Scope (`/.default` form) |
|---|---|---|
| Azure Public | `https://ossrdbms-aad.database.windows.net` | `https://ossrdbms-aad.database.windows.net/.default` |
| Azure US Gov | `https://ossrdbms-aad.database.usgovcloudapi.net` | `https://ossrdbms-aad.database.usgovcloudapi.net/.default` |
| Azure China | `https://ossrdbms-aad.database.chinacloudapi.cn` | `https://ossrdbms-aad.database.chinacloudapi.cn/.default` |

Notes:
- `azure_identity` `TokenCredential::get_token` expects scope strings — i.e.
  the `/.default` suffix form, not the bare resource URI.
- The audience differs by sovereign cloud only in the host suffix; FR-009
  exists to expose this as an override.

### Token-as-password protocol detail

Azure Database for PostgreSQL Flexible Server accepts the bearer token
**verbatim as the Postgres password field** in the standard
`SCRAM-SHA-256 / password` startup flow. There is no protocol-level OAuth
bind/handshake. This is documented by Microsoft and is what Spec line 105
asserts. Practically: `PgConnectOptions::password(&token.secret())` is the
entire integration on the wire side. Username remains the Postgres role name
(typically the Entra principal display name or app-registration name).

---

## 5. Provider Error Classification Patterns

The single classifier is `src/provider.rs:111-142`:

```rust
fn sqlx_to_provider_error(operation: &str, e: SqlxError) -> ProviderError {
    match e {
        SqlxError::Database(ref db_err) => {
            // codes: 40P01 -> retryable (deadlock)
            //        40001 -> permanent (serialization failure)
            //        23505 -> permanent (duplicate)
            //        23503 -> permanent (FK violation)
            //        0A000 -> retryable (cached plan)
            //        else  -> permanent
        }
        SqlxError::PoolClosed | SqlxError::PoolTimedOut =>
            ProviderError::retryable(operation, …),
        SqlxError::Io(_) =>
            ProviderError::retryable(operation, …),
        _ => ProviderError::permanent(operation, …),
    }
}
```

Constructors used elsewhere (`grep ProviderError::(retryable|permanent)`):

- `ProviderError::retryable(operation, message)` — only used for
  deadlock, cached-plan, pool-closed/timed-out, and IO. (5 occurrences in
  `provider.rs`.)
- `ProviderError::permanent(operation, message)` — used liberally for
  every non-retryable Postgres condition and for serialization/deserialization
  errors elsewhere.

### Where Entra-related errors should fit (research only — *not* design)

There are two distinct failure modes implied by the spec:

| Failure mode | When | Spec ref | Existing analogue |
|---|---|---|---|
| Initial credential resolution fails (no managed identity, no env, no CLI) | At provider construction (`new_*`) | FR-007, edge case "No credential available" | Returned via `anyhow::Result<Self>` from `new_with_schema` — this site does not produce `ProviderError`. Today the construction path uses `?` against `sqlx::Error` and `anyhow`. |
| Token fetch fails during runtime pool growth (transient IMDS hiccup) | Inside whatever hook supplies the token to a new connection | FR-008, edge case "Token fetch fails transiently" | Maps to `SqlxError::Io(_)` / `PoolTimedOut` — already classified as retryable by `sqlx_to_provider_error`. The challenge: a token-fetch failure is *not* a `sqlx::Error`, so the planner needs to decide how to surface it (e.g., bubble through `sqlx::Error::Io` or a custom mapping path) so it lands as retryable. |
| Token-issuing principal lacks DB role (server returns auth failure) | At connect time | edge case "principal lacks DB role" | Postgres returns SQLSTATE 28000/28P01 → currently routed through the `else` branch → `ProviderError::permanent`. That matches the spec's expectation. |

The construction path currently returns `anyhow::Result<Self>`, **not**
`Result<Self, ProviderError>`. That means the FR-007 "clear, distinct error"
requirement is satisfied at construction time by an `anyhow` error with a
descriptive message — but there is no `ProviderError::EntraCredential` enum
variant today and adding one would touch the duroxide crate, which is out of
scope (Spec line 122-123).

---

## 6. Test Infrastructure

### Shared helpers — `tests/common/mod.rs`

- `get_database_url()` (line 9-12) — reads `DATABASE_URL` from `.env` /
  environment. Required for all integration tests.
- `next_schema_name()` (line 15-19) — generates `e2e_test_<8-hex>` schema
  per test for isolation.
- `create_postgres_store()` (line 86-95) — wraps
  `PostgresProvider::new_with_schema(&url, Some(&schema)).await` and returns
  the `Arc<dyn Provider>` plus the schema name.
- `cleanup_schema(name)` (line 99-111) — drops the test schema after the run.

### Provider validation harness — `tests/postgres_provider_test.rs`

Drives the standard `duroxide::provider_validation::*` suites against
`PostgresProvider`. Important for our purposes:

- Logging init: `init_test_logging()` (lines ~14-26).
- `create_postgres_provider(&self) -> Arc<PostgresProvider>` (line 94) and
  `create_provider(&self) -> Arc<dyn Provider>` (line 118) — these are the
  only two factory methods used by the validation suites.
- Tests rely on a real Postgres reachable via `DATABASE_URL`. **No mocking
  exists in the current harness.**

### Implications for unit-testing the Entra path without Azure

The spec (line 117) explicitly scopes in:
> Unit tests covering option construction, error surface for missing
> credentials, and the TLS-enforcement contract.

Concretely, achievable without Azure or a special Postgres instance:

1. **Option construction tests**: build the new options type and assert that
   the resulting `PgConnectOptions` has the expected
   host/port/database/username, and `ssl_mode == VerifyFull`. `PgConnectOptions`
   exposes inspector methods for these in sqlx 0.8 (and even if not, the
   builder is exercisable via an internal helper that returns it).
2. **Missing-credential test**: parameterize the constructor over a trait
   like `TokenSource` (a thin abstraction over `TokenCredential`). Inject a
   fake that returns "no credential available", assert the error returned
   from the constructor names Entra credential resolution. This avoids any
   live Azure call.
3. **TLS contract test**: same option-construction test asserts that no code
   path produces `PgSslMode::Prefer/Allow/Disable/Require/VerifyCa`. This is
   purely structural.
4. **Refresh-task behavior**: if the planner chooses the
   `set_connect_options` strategy, the refresh task can be tested by feeding
   a fake `TokenSource` that hands out distinct tokens per call and asserting
   that `Pool::connect_options()` reflects the latest one after the configured
   interval.

None of the above requires `DATABASE_URL`, Azure CLI, or a managed identity.

The planner will likely also want to mark a separate, opt-in integration test
(per Spec line 138) gated on env vars like
`DUROXIDE_PG_AZURE_HOST` / `DUROXIDE_PG_AZURE_DB` so it stays out of CI.

---

## 7. Existing Dependency Tree Implications

`Cargo.toml:30-44`:

```toml
duroxide = { version = "0.1.28", features = ["provider-test"] }
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.8", features = ["runtime-tokio-native-tls", "postgres", "chrono"], default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
dotenvy = "0.15"
uuid = { version = "1.0", features = ["v4"] }
anyhow = "1.0"
chrono = "0.4"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
semver = "1.0"
include_dir = "0.7"
```

### Likely additions for Entra

- `azure_identity = "0.X"` (whichever minor is pinned; see §3).
- `azure_core = "0.X"` matched to `azure_identity` (needed to import the
  `TokenCredential` trait and `AccessToken` type).

### Compatibility flags / risk inventory

| Concern | Status |
|---|---|
| Tokio version | Both `azure_identity` and `azure_core` use tokio. Project uses `tokio = "1"` with `full` — compatible. |
| TLS backend duplication | `azure_core`'s default HTTP client uses `reqwest`, which by default pulls in `native-tls` *or* `rustls`. `sqlx` is already on `runtime-tokio-native-tls` (no `ring`). The planner should pick `azure_core`/`azure_identity` features that keep the `native-tls` path (avoid `--features rustls` or anything that re-introduces `ring`). README line 65 calls out that `ring` was deliberately dropped for FIPS — adding it back via Azure crates would regress that. |
| `async-trait` | Already a direct dep; `azure_identity` also depends on `async-trait` `^0.1`. Compatible. |
| `chrono` | Already a direct dep at `0.4`. `azure_core` historically uses `time` (`OffsetDateTime`), not `chrono`. Both can coexist; no conflict. |
| MSRV | Project doesn't pin one. `azure_identity` 0.31+ likely requires recent stable Rust. Confirm with `cargo check` on the pinned version. |
| Pre-1.0 churn | Per Risk #2 in spec, both crates are pre-1.0. Planner should pin a specific minor. |
| Build time / binary size | Per Spec (cross-cutting, line 88), the impact is acknowledged and accepted unconditionally — no feature gate. |

### Workspace member `pg-stress`

`Cargo.toml:1-2` declares a workspace with `pg-stress` as the second member.
`pg-stress` is excluded from the `duroxide-pg` crate (line 16) but shares the
top-level lockfile. Adding `azure_identity` as a `duroxide-pg` direct dep
means `pg-stress` will pick it up transitively only if it depends on
`duroxide-pg`. Quick check (not yet read): `pg-stress/Cargo.toml` should be
inspected by the planner to confirm no surprise blow-up.

---

## 8. Documentation Conventions

### `README.md`

Top-level structure (verified, lines 1-72):

1. Title + Duroxide back-reference.
2. **Installation** (Cargo.toml snippet).
3. **Usage** — runnable Rust example, `tokio::main`, building a
   `PostgresProvider` from a URL and wiring it to a `Worker`.
4. **Custom Schema** subsection — second runnable snippet.
5. **Configuration** — single env-var table (`DUROXIDE_PG_POOL_MAX`).
6. **Features** — bulleted capability list.
7. **Latest Release** — short changelog summary.
8. **License**.

Per FR-011 / SC-007, an Entra section should slot between **Custom Schema**
and **Configuration** (or under a new top-level **Authentication** section
— the planner picks). It needs:

- A runnable usage example.
- Required Azure setup (Entra admin, role grants, sample `az`/`psql`
  commands).
- Recommended identity sources for production vs local dev.

The new `Configuration` table likely grows by Azure-related env vars (or by
a "see *Entra Auth Options*" cross-reference, depending on options-type
design).

### `docs/` folder

```
docs/concurrent-instance-fetch-race.md
docs/duroxide-blockers.md
```

Both are deep-dive notes, not user docs. The README is the primary user
doc surface. There is **no** `docs/configuration.md` or `docs/azure.md`
today — the planner can decide whether Entra warrants a dedicated file or
stays in README.

### Rustdoc conventions

`src/lib.rs:1-50` is the crate-level module doc and follows a strict pattern:

- `# Duroxide PostgreSQL Provider` heading.
- `## Usage` with a `rust,no_run` fenced block wrapped in
  `# async fn example() -> anyhow::Result<()> { ... # Ok(()) # }`.
- `## Custom Schema` with the same wrapper pattern.
- `## Configuration` table mirroring the README.
- `## Features` bullet list.

`src/provider.rs:24-40` repeats the runnable example on the type itself,
again as `rust,no_run`. New constructors should follow the same `# async fn
example()` doctest convention so the examples compile under `cargo test
--doc` (or, since they are `no_run`, at least typecheck).

Other rustdoc patterns in `provider.rs`:

- `#[instrument(skip(self), target = "duroxide::providers::postgres")]` on
  several public methods (e.g., line 78).
- `tracing::{debug, error, instrument, warn}` used with the same
  `target = "duroxide::providers::postgres"` everywhere.

A new Entra constructor / refresh task should probably emit `tracing` events
under the same target, but that's a planner call.

---

## Citations Summary

| Topic | Citation |
|---|---|
| `PostgresProvider` constructors | `src/provider.rs:46-76` |
| `PostgresProvider` struct | `src/provider.rs:41-44` |
| `PostgresProvider` accessors | `src/provider.rs:101-108` |
| Pool defaults & env var | `src/provider.rs:52-60` |
| sqlx imports | `src/provider.rs:10` |
| Error classifier | `src/provider.rs:111-142` |
| Crate-level rustdoc | `src/lib.rs:1-56` |
| README structure | `README.md:1-72` |
| Existing docs files | `docs/concurrent-instance-fetch-race.md`, `docs/duroxide-blockers.md` |
| sqlx Cargo features | `Cargo.toml:34` |
| Test harness shared helpers | `tests/common/mod.rs:9-111` |
| Provider validation harness | `tests/postgres_provider_test.rs:14-170` |
| sqlx 0.8 PoolOptions API | `https://raw.githubusercontent.com/launchbadge/sqlx/v0.8.6/sqlx-core/src/pool/options.rs` (no `before_connect`; only `after_connect` / `before_acquire` / `after_release`) |
| sqlx 0.8 `Pool::set_connect_options` | `https://raw.githubusercontent.com/launchbadge/sqlx/v0.8.6/sqlx-core/src/pool/pool.rs` |
| `PgConnectOptions::password / ssl_mode / ssl_root_cert` | `https://raw.githubusercontent.com/launchbadge/sqlx/v0.8.6/sqlx-postgres/src/options/mod.rs` |
| `PgSslMode::VerifyFull` | `https://raw.githubusercontent.com/launchbadge/sqlx/v0.8.6/sqlx-postgres/src/options/ssl_mode.rs` |
| `azure_identity` versions | `https://index.crates.io/az/ur/azure_identity` (latest `0.33.0`; chain `0.31.0` → `0.32.0` → `0.33.0`) |
| `azure_identity` API surface | `https://docs.rs/azure_identity/latest/azure_identity/` |
| Azure Postgres Entra audience | Microsoft Learn — "Use Microsoft Entra ID for authentication with Azure Database for PostgreSQL — Flexible Server" (referenced in Spec line 144) |

---

## Key Findings (Quick Read)

1. **No `before_connect` hook in sqlx 0.8** — the only viable per-connection
   credential injection strategy uses `Pool::set_connect_options(...)` plus
   a refresh task. The `PoolOptions` struct in sqlx 0.8.6 only exposes
   `after_connect`, `before_acquire`, `after_release`.
2. **`PgConnectOptions::ssl_mode(PgSslMode::VerifyFull)`** is the single
   call needed to satisfy FR-005; no `ssl_root_cert` is required for Azure
   public-cloud servers when using the system trust store. The current
   `runtime-tokio-native-tls` feature supports verify-full on all three
   target platforms.
3. **`azure_identity` is the right crate** — pre-1.0 (latest `0.33.0`),
   official Microsoft, async, tokio-compatible. Companion `azure_core` must
   be pinned at the matching minor. Risk: API churn between minors (per spec
   Risk #2).
4. **Token-as-password is the wire protocol** — Azure Postgres takes the
   Entra access token verbatim in the standard Postgres password slot. No
   special handshake.
5. **Audience scope strings** are well-defined per cloud
   (`https://ossrdbms-aad.database.windows.net/.default` for public Azure)
   and FR-009 already requires an override knob.
6. **Existing public API is small enough that the new constructor can be
   purely additive** — `new`, `new_with_schema`, the four accessors, and
   `cleanup_schema`. None need to change to satisfy FR-006.
7. **Error classification has a single chokepoint** (`sqlx_to_provider_error`)
   for runtime errors and `anyhow::Result` for construction errors. Both
   surfaces can carry FR-007 / FR-008 messages without changing
   `ProviderError`'s variants.
8. **Unit tests can fully exercise the Entra path without Azure** by hiding
   `TokenCredential` behind a small trait the constructor accepts. Live
   integration tests against Azure stay opt-in (Spec line 138).
9. **Native-TLS / FIPS posture must be preserved** — `azure_core` /
   `azure_identity` should be feature-selected to avoid pulling `ring` back
   in (per `README.md:65-66`).
10. **README + crate-level rustdoc both need an Entra section** with a
    `rust,no_run` runnable example and a pointer to required Azure setup
    (per FR-011 / SC-007). Existing convention is clear.
