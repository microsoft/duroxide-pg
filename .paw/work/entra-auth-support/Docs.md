# Entra ID Authentication — Technical Reference

Internal reference for maintainers of the duroxide-pg PostgreSQL provider's
Microsoft Entra ID (formerly Azure AD) authentication path. Captures the
decisions a future maintainer would otherwise have to rediscover.

## Public API surface

Just two entry points and one configuration type, all exported from the crate
root:

| Item | Defined at | Purpose |
|---|---|---|
| `PostgresProvider::new_with_entra` | `src/provider.rs` | Entra-authenticated provider, public schema. |
| `PostgresProvider::new_with_schema_and_entra` | `src/provider.rs` | Entra-authenticated provider, custom schema. |
| `EntraAuthOptions` | `src/entra.rs` | Knobs: audience, max_connections, acquire_timeout, refresh_interval. |

Everything else (`TokenSource`, `EntraToken`, `AzureIdentityTokenSource`,
`ChainedCredential`) is `pub(crate)` to insulate callers from upstream
`azure_core` / `azure_identity` churn (mitigates spec Risk #2).

## Dependency choices and rationale

```toml
azure_core = { version = "0.35", default-features = false, features = ["reqwest", "reqwest_deflate", "reqwest_gzip", "tokio"] }
azure_identity = { version = "0.35", default-features = false, features = ["tokio"] }
```

**Why `default-features = false`?** The `azure_core` 0.35 default feature set
includes `reqwest_rustls`, which transitively pulls `ring`. The repo's FIPS
posture requires no `ring` in the dep tree (the same reason sqlx is configured
with `runtime-tokio-native-tls`). By disabling defaults and enabling only
plain `reqwest` (which uses its own `default-tls` → native-tls on
Windows / Linux), we keep the `ring`-free invariant.

**Why pin to 0.35 specifically?** The Azure SDK for Rust 0.35 is the first
version that ships `ManagedIdentityCredential::new(None)?` and
`DeveloperToolsCredential::new(None)?` as standalone, individually
constructible types. `DefaultAzureCredential` does **not** exist in 0.35
(scheduled for a later release). The crate's small `ChainedCredential` in
`src/entra.rs` substitutes for it.

**No new Cargo feature gate.** Per the spec decision, Entra support is
unconditional. A `azure-entra` feature gate is listed as a Phase Candidate in
`ImplementationPlan.md` if downstream consumers later object to the dep
footprint.

## Connection setup and TLS

`build_entra_connect_options(host, port, database, user)` in `src/provider.rs`
constructs every Entra connection's `PgConnectOptions`:

```rust
PgConnectOptions::new()
    .host(host)
    .port(port)
    .database(database)
    .username(user)
    .ssl_mode(PgSslMode::VerifyFull)
```

`PgSslMode::VerifyFull` is non-negotiable. There is no public path that
constructs Entra connect options with a weaker SSL mode. The unit test
`build_entra_connect_options_uses_verify_full` guards this contract; a grep
audit (`grep PgSslMode src/`) confirms no `Disable`/`Allow`/`Prefer`/`Require`/`VerifyCa`
appears in the Entra path.

Token-as-password handoff: `connect_with(base_options.clone().password(&token.secret))`.
sqlx accepts arbitrary password strings; Azure's wire protocol accepts the
Entra access token in the password slot.

## Token refresh task

`spawn_token_refresh_task` is the single background task per Entra-authenticated
provider. It owns the `Arc<dyn TokenSource>`, a clone of `base_options` (the
no-password template), the `Arc<PgPool>`, the audience string, and the
caller-configured `refresh_interval_ceiling`.

### Scheduling: expiry-driven with safety margin

```
next_sleep = min(refresh_interval_ceiling,
                 max(MIN_REFRESH=30s, expires_at - now - SAFETY_MARGIN=5min))
```

| Constant | Value | Rationale |
|---|---|---|
| `ENTRA_REFRESH_MIN_INTERVAL` | 30 s | Floor — prevents busy-loop if a token is already inside the safety margin. Larger than realistic operator misconfiguration. |
| `ENTRA_REFRESH_SAFETY_MARGIN` | 5 min | Cushion for clock skew + connection-acquisition latency + a grace window for a slow IDP response. Empirically tracks the budget Azure Postgres allows after token expiry. |
| `ENTRA_REFRESH_MAX_RETRY` | 30 s | After a failed refresh, retry no later than this. |
| Default `refresh_interval` | 20 min | Conservative ceiling: shorter than Entra's typical 60-minute access-token lifetime so we always rotate well before expiry, even if `expires_at` lookup is for some reason wrong. Override via `EntraAuthOptions::refresh_interval`. |

The pure scheduling logic is `compute_next_refresh_sleep`, covered by three
unit tests in `provider::tests`.

### Rotation path: `Pool::set_connect_options`

sqlx 0.8 has **no** `before_connect` hook (this was the pivotal finding from
`CodeResearch.md`). The supported rotation path is
[`sqlx::Pool::set_connect_options`](https://docs.rs/sqlx/0.8/sqlx/pool/struct.Pool.html#method.set_connect_options),
which atomically swaps the template used for **future** connection acquisitions.
Existing in-flight connections are unaffected; they are eventually retired by
sqlx's max-lifetime reaper or evicted on the next acquisition cycle.

This is intentional: forcing eager re-authentication of every pooled connection
on rotation would cause a thundering herd against the IDP. Letting connections
age out organically is correct for a token whose lifetime is much longer than
typical request handling.

### Failure handling

On a refresh failure we log at WARN with `target = "duroxide::providers::postgres"`
and back off:

```rust
let retry_in = ENTRA_REFRESH_MAX_RETRY.min(compute_next_refresh_sleep(...));
sleep(retry_in.max(ENTRA_REFRESH_MIN_INTERVAL)).await;
```

With `MIN==MAX==30s` this currently means a flat 30 s retry. The min/max
expression is intentional belt-and-suspenders so future tuning doesn't
accidentally violate either bound.

Persistent token-fetch failure loops every 30 s with WARN logs. If it persists,
operator intervention is required (revoked principal, expired secret, etc.).

### Lifecycle

The task's `tokio::task::AbortHandle` is wrapped in `AbortOnDropHandle` and
stored on the provider as `_refresh_task: Option<AbortOnDropHandle>`. When the
provider is dropped, the handle is dropped, and the task is aborted. There is
no other shutdown path — providers are cheap and shutting down a runtime drops
its provider. This is the same lifecycle pattern as the rest of the crate's
background tasks.

For the password constructors (`new`, `new_with_schema`), `_refresh_task` is
`None`, so dropping a password-path provider is a no-op for the refresh
machinery. FR-006 ("byte-identical" behavior) is upheld in observable terms.

## Error mapping

### Construction-time

`EntraAuthOptions::default_token_source()` and the initial `fetch_token` call
both wrap their errors with `.context("Entra credential resolution failed")`.
`anyhow`'s `Display` implementation surfaces the chain, e.g.:

```
Entra credential resolution failed: All chained Entra credentials failed to acquire a token:
  - Managed identity is not available
  - Azure CLI not found on PATH
```

This is the public contract for FR-007 (callers can search on the phrase
"Entra credential" to detect a credential-resolution failure).

### Runtime

`sqlx_to_provider_error` in `src/provider.rs` is extended with one branch:

```rust
} else if code == Some("28000") || code == Some("28P01") {
    // 28000 = invalid_authorization_specification
    // 28P01 = invalid_password
    ProviderError::retryable(operation, format!(
        "Authentication error (likely token rotation): {e}"
    ))
}
```

The classification is unconditional (applies to both Entra and password paths).
For the password path this means a misconfigured static password causes one
extra retry before surfacing as a permanent error, which is acceptable. The
combination of expiry-driven refresh (primary) + classifier extension
(backstop) satisfies FR-004 and FR-008 without false-positive misclassification.

## Test seam

`pub(crate) trait TokenSource` is the single seam for unit testing. Production
uses `AzureIdentityTokenSource` (wrapping `Arc<dyn TokenCredential>`); tests
use `RecordingFakeTokenSource` (in `crate::entra::test_support`) which records
scope arguments and returns scripted tokens or scripted failures.

Cross-file test reuse: `RecordingFakeTokenSource` lives in
`#[cfg(test)] pub(crate) mod test_support` so `src/provider.rs::tests` can
import it via `use crate::entra::test_support::{token, RecordingFakeTokenSource}`.

## Audience strings per Azure cloud

| Cloud | Audience |
|---|---|
| Public (default) | `https://ossrdbms-aad.database.windows.net/.default` |
| US Government | `https://ossrdbms-aad.database.usgovcloudapi.net/.default` |
| China (Mooncake) | `https://ossrdbms-aad.database.chinacloudapi.cn/.default` |

Override via `EntraAuthOptions::audience(...)`.

## Troubleshooting playbook

| Symptom | Likely cause | Mitigation |
|---|---|---|
| `Entra credential resolution failed: All chained Entra credentials failed...` at startup | No managed identity, no `az login`. | Run `az login`, configure managed identity, or set `AZURE_*` env vars for a workload identity. |
| `password authentication failed for user "..."` at startup, before any retry | Postgres role not granted to Entra principal, or Entra admin not configured on the Flexible Server. | `az postgres flexible-server ad-admin set ...` then `SELECT pgaadauth_create_principal('...', false, false);` |
| `permission denied for ...` at startup | Principal authenticated but lacks DB privileges (SQLSTATE `42501`). NOT classified as retryable. | `GRANT ... ON DATABASE ...` etc. |
| Periodic WARN: `Entra token refresh failed; will retry` | Transient IDP unavailability, or principal revoked. | Check Azure activity log; if persistent, principal needs re-credentialing. |
| Pointing `new_with_entra` at non-Azure Postgres | Vanilla Postgres doesn't accept Entra tokens as passwords. | Use `new`/`new_with_schema` for non-Azure deployments. |

## Manual verification checklist

Captured in `ImplementationPlan.md` Phase 2 §Manual Verification, copied here
for the as-built record:

- End-to-end against Azure Postgres Flexible Server with managed identity.
  Run a small orchestration round-trip; observe migrations applied.
- Token rotation observable via `RUST_LOG=duroxide::providers::postgres=debug`.
- Drop the provider; refresh task terminates.
- `az login` flow against the same instance from a developer workstation.
- Edge case: principal lacks DB role → `42501` permanent error, NOT routed
  through 28xxx retryable mapping.
- Edge case: Entra options against non-Azure Postgres → fails with clear auth
  error, doesn't loop indefinitely.

## References

- Spec: `.paw/work/entra-auth-support/Spec.md`
- Code research: `.paw/work/entra-auth-support/CodeResearch.md`
- Implementation plan: `.paw/work/entra-auth-support/ImplementationPlan.md`
- sqlx 0.8 source for `Pool::set_connect_options`:
  <https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-core/src/pool/mod.rs>
- Microsoft Learn — Entra ID auth for Azure Database for PostgreSQL Flexible Server:
  <https://learn.microsoft.com/azure/postgresql/flexible-server/concepts-azure-ad-authentication>
- Azure SDK for Rust 0.35 release notes:
  <https://github.com/Azure/azure-sdk-for-rust>
