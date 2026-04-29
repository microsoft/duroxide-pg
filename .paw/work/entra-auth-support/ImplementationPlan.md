# Azure Entra Authentication for PostgresProvider — Implementation Plan

> **Revision history**:
> - Initial draft + plan-review PASS.
> - **Multi-model planning-docs review revisions** (synthesis at `reviews/planning/REVIEW-SYNTHESIS.md`):
>   - **C1 (consensus must-fix)**: Refresh task is now expiry-driven (`expires_at - SAFETY_MARGIN`), with `refresh_interval` as a ceiling. `sqlx_to_provider_error` extended to map SQLSTATE 28000/28P01 to retryable as a backstop for FR-008. (Resolution (c) — both.)
>   - **C2 (consensus should-fix)**: All new unit tests live in in-crate `#[cfg(test)] mod tests` (not `tests/` integration crate) so they can name `pub(crate) TokenSource`.
>   - **C3 (consensus should-fix)**: Refresh-swap test no longer uses `pool.connect_options().password()` (no public getter exists in sqlx 0.8). Replaced with `RecordingFakeTokenSource` + pure-function test on `build_connect_options`.
>   - **C4 (consensus consider)**: CHANGELOG/version-bump marked as optional release-process, not blocking SC.
>   - **P1 (partial should-fix)**: `with_credential(Arc<dyn TokenCredential>)` and the `azure_core::TokenCredential` re-export are **dropped**. Custom credential injection is out of scope for v1; tests inject via the in-crate `pub(crate) TokenSource` seam.
>   - **S1 (single should-fix)**: Added Phase 2 manual verification bullets for the spec edge cases "principal lacks DB role" and "Entra against non-Azure Postgres".
>   - **S2 (single consider)**: `min_connections` removed from public `EntraAuthOptions`; pool internally hard-codes `.min_connections(1)` to match the existing password path.

## Overview

Add Microsoft Entra ID authentication support to `PostgresProvider` so it can connect to Azure Database for PostgreSQL Flexible Server without static passwords. The design preserves all existing constructors and behavior; new functionality is purely additive.

The integration uses `azure_identity` to obtain Entra access tokens from the ambient Azure identity (managed identity, workload identity, environment service principal, or developer CLI sign-in) and supplies tokens to sqlx via `Pool::set_connect_options(...)` — refreshed periodically by a background task — because sqlx 0.8 does not expose a `before_connect` hook. TLS is pinned to `PgSslMode::VerifyFull`. A small internal `TokenSource` trait lets unit tests inject fake credentials so the entire feature can be tested without an Azure environment.

## Current State Analysis

- `PostgresProvider::new` and `new_with_schema` parse a single `database_url` and call `PgPoolOptions::connect(&str)` (`src/provider.rs:46-76`). All public methods on the type (`pool`, `schema_name`, `initialize_schema`, `cleanup_schema`) are agnostic of how the pool was constructed — new constructors only need to produce the same `Arc<PgPool>` plus a schema name and run `MigrationRunner::migrate()`.
- sqlx 0.8 has **no `before_connect` callback** (verified against `v0.8.6` source). The supported per-connection credential rotation pattern is `Pool::set_connect_options(...)` paired with a background refresh task.
- TLS verify-full is reachable today (`PgConnectOptions::ssl_mode(PgSslMode::VerifyFull)`) under the existing `runtime-tokio-native-tls` feature; no `ring` dependency is reintroduced.
- Errors funnel through `sqlx_to_provider_error` (`src/provider.rs:111-142`); `SqlxError::Io` already maps to retryable, which is the natural channel for runtime token-fetch failures.
- Tests in `tests/common/mod.rs` and `tests/postgres_provider_test.rs` are integration-only and require `DATABASE_URL`. There is no mocking infrastructure today, so the new feature must introduce its own seam (a `TokenSource` trait) for unit testing.
- README (`README.md`) and crate rustdoc (`src/lib.rs`) follow a strict `## Usage` / `## Custom Schema` runnable-example pattern; an Entra section slots in cleanly.

## Desired End State

- `PostgresProvider` exposes two new constructors — `new_with_entra(host, port, database, user, options)` and `new_with_schema_and_entra(host, port, database, user, schema_name, options)` — that obtain Entra tokens via the ambient Azure identity and connect to Azure Postgres over TLS verify-full without passwords.
- A token-refresh background task keeps the pool's connect options current so connections opened hours after startup authenticate successfully (SC-003).
- A small internal `TokenSource` trait abstracts `azure_identity::TokenCredential` so unit tests can drive the feature without Azure (SC of FR-007, FR-008, FR-005 all testable with a fake source).
- `EntraAuthOptions` exposes audience/scope override (FR-009) and pool tuning (FR-010) with sensible Azure-public-cloud defaults.
- All existing tests pass unchanged (SC-004); existing constructors are byte-compatible (FR-006).
- README, crate rustdoc, and CHANGELOG document Entra usage, required Azure setup, and recommended identity sources (SC-007).
- Verification: `cargo build --all-targets` clean; `cargo nextest run` (or `cargo test`) passes including new unit tests; `cargo doc --no-deps` clean; clippy clean.

## What We're NOT Doing

- No live Azure-Postgres CI integration tests (Spec out-of-scope). An opt-in integration test gated on `DUROXIDE_PG_AZURE_*` env vars may be scaffolded but is not enabled by default.
- No support for caller-supplied static tokens or custom credential factories beyond what `EntraAuthOptions` exposes (Spec out-of-scope).
- No changes to the duroxide `Provider`/`ProviderAdmin` trait surface or to existing migrations (Spec out-of-scope).
- No new `ProviderError` variants — Entra credential failures at construction surface via the existing `anyhow::Result<Self>` channel; runtime token-fetch failures map onto the existing retryable IO classification.
- No Cargo feature gate for Entra support — per spec decision, dependencies are added unconditionally.
- No switch back to `runtime-tokio-rustls` / `ring`. Azure dependency features must keep the `native-tls` HTTP client.
- No support for non-Postgres Azure database services.
- No password fallback or auto-detection: the Entra constructors are explicit; the existing password constructors remain the only path for non-Entra connections.

## Phase Status

- [ ] **Phase 1: Dependencies, `EntraAuthOptions`, and `TokenSource` seam** — Add Azure SDK deps with the right TLS feature flags; introduce the configuration type and credential abstraction with unit tests.
- [ ] **Phase 2: Entra constructors and token-refresh task** — Wire `new_with_entra` / `new_with_schema_and_entra`, build `PgConnectOptions` with `VerifyFull`, run migrations, and spawn the refresh task that calls `Pool::set_connect_options` periodically.
- [ ] **Phase 3: Documentation** — `Docs.md`, README and rustdoc Entra sections, CHANGELOG entry, optional opt-in integration test scaffold.

## Phase Candidates

<!-- Items that could be promoted to full phases later if needed -->
- [ ] Cargo feature gate (`azure-entra`) for opt-in dependency footprint — only if downstream consumers complain.
- [ ] Caller-supplied static-token / custom-credential factory variant on `EntraAuthOptions`.
- [ ] Live opt-in CI integration test exercising a real Azure Postgres Flexible Server.

---

## Phase 1: Dependencies, `EntraAuthOptions`, and `TokenSource` seam

**Objective**: Land the foundational types (config struct, credential trait) and dependencies without changing any user-visible behavior. This phase contains no provider construction logic — that's Phase 2 — but ensures that adding Azure SDK dependencies builds cleanly and that the new types are unit-tested in isolation.

### Changes Required

- **`Cargo.toml`**:
  - Add `azure_identity` and `azure_core` as direct dependencies pinned to a single matching minor version. Choose a minor whose features include the default credential chain (managed identity + workload identity + Azure CLI). Verify chosen features keep the HTTP client on `native-tls` (do **not** enable `rustls`/`ring`); if `azure_core` defaults pull `rustls`, set `default-features = false` and select the `reqwest` + `enable_native_tls` (or equivalent) feature combination.
  - Confirm `pg-stress/Cargo.toml` does not depend on `duroxide-pg`'s public surface in a way that requires changes (read it first; expected to be unaffected).
  - Run `cargo tree -d` after adding deps to confirm no `ring` is introduced.

- **`src/entra.rs`** (new module):
  - **No public re-export of `azure_core::TokenCredential`** *(per synthesis P1, resolution (i))*. Azure SDK types stay internal to the module. This minimizes public API surface and avoids exposing pre-1.0 third-party types to callers (mitigates spec Risk #2).
  - Define `EntraAuthOptions` struct with builder-style methods. Fields: token audience/scope (default = Azure public cloud OSS RDBMS scope `https://ossrdbms-aad.database.windows.net/.default`), max pool size (default = current 10, or `DUROXIDE_PG_POOL_MAX` env if unset, matching the existing path for parity), acquire timeout (default 30 s), refresh interval ceiling (default = 1/3 of expected token lifetime; treated as a *ceiling*, not the sole driver — see refresh-task design in Phase 2 and synthesis C1).
    - **Removed** *(per synthesis S2)*: `min_connections` is **not** exposed on `EntraAuthOptions`. The pool internally hard-codes `.min_connections(1)` to match the existing password path (`src/provider.rs:51`) exactly. This keeps FR-010's stated tunables ("max size, acquire timeout") as the public surface.
    - **Removed** *(per synthesis P1, resolution (i))*: no `optional explicit Arc<dyn TokenCredential>` field, no `with_credential` mutator. Custom credential injection is out of scope for v1.
  - Define `EntraToken { secret: String, expires_at: SystemTime }` struct. **`expires_at` is consumed by the refresh task in Phase 2** (per synthesis C1) — it is not vestigial.
  - Define an internal `TokenSource` trait — small async-trait with `async fn fetch_token(&self, scopes: &[&str]) -> anyhow::Result<EntraToken>`. Provide a default implementation `AzureIdentityTokenSource` that wraps the constructed credential and translates `azure_core::AccessToken` into `EntraToken` (mapping the upstream expiry timestamp into `EntraToken.expires_at`). The trait stays `pub(crate)` (escape hatch for future churn in `azure_identity`); tests live in-crate (see Phase 1 test-placement rationale).
  - Provide a constructor `EntraAuthOptions::new()` (Azure public cloud defaults) plus chainable mutators: `audience(impl Into<String>)`, `max_connections(u32)`, `acquire_timeout(Duration)`, `refresh_interval(Duration)`.
  - Internal helper `EntraAuthOptions::token_source(self) -> anyhow::Result<Arc<dyn TokenSource>>` that constructs the default chained credential (`azure_identity::DefaultAzureCredential` or `DeveloperToolsCredential`+production chain; exact symbol confirmed at implementation against the pinned version's docs.rs). Surfacing failure to construct the default credential happens here as an `anyhow::Error` that names "Entra credential resolution".

- **`src/lib.rs`**:
  - Add `pub mod entra;` and re-export `EntraAuthOptions` (keep `TokenSource` private).

- **Tests — in-crate `#[cfg(test)] mod tests` inside `src/entra.rs`** (unit-style; runs without `DATABASE_URL`).
  > **Test placement rationale**: `TokenSource` is `pub(crate)` (escape hatch for `azure_identity` churn). Rust integration tests under `tests/` compile as a separate crate and cannot name `pub(crate)` items, so all unit tests that need to implement or inject `FakeTokenSource` live in `#[cfg(test)] mod tests` blocks inside `src/entra.rs` (and `src/provider.rs` for constructor-level tests in Phase 2). Existing integration tests under `tests/` are unchanged.
  - Build `EntraAuthOptions` with defaults; assert audience equals the Azure-public-cloud scope; assert pool defaults match the existing password-path numbers (10 / 1 / 30 s) so SC-004 spirit holds.
  - Override audience and assert it round-trips.
  - Provide a `FakeTokenSource` (defined in the same `#[cfg(test)] mod tests`) that returns a constant token; verify it implements the `TokenSource` trait via a small smoke test.
  - (Conditional on resolution of P1 — `with_credential` public API decision: if kept, add a test that `EntraAuthOptions::with_credential(fake)` short-circuits default-chain construction. If dropped, this test is removed.)

### Success Criteria

#### Automated Verification:
- [ ] Workspace builds: `cargo build --workspace --all-targets`
- [ ] Tests pass: `cargo nextest run` (or `cargo test`) — including the new `entra_options_test`
- [ ] No `ring` in dep tree: `cargo tree -d` returns no diff vs. baseline (or only the expected Azure crates) and no `ring` package appears in `cargo tree -p duroxide-pg`
- [ ] Doc build clean: `cargo doc --no-deps`
- [ ] Lint clean: `cargo clippy --workspace --all-targets -- -D warnings`

#### Manual Verification:
- [ ] Adding deps does not flip TLS backend (visual diff on `cargo tree`)
- [ ] `EntraAuthOptions` API reads sensibly from a caller's perspective (skim rustdoc once written in Phase 3, but the type names should already be intuitive)

---

## Phase 2: Entra constructors and token-refresh task

**Objective**: Land the user-visible API — `new_with_entra` and `new_with_schema_and_entra` — including the refresh task. After this phase, an Azure-deployed service can authenticate against Azure Database for PostgreSQL Flexible Server end-to-end (SC-001).

### Changes Required

- **`src/provider.rs`**:
  - Import `sqlx::postgres::{PgConnectOptions, PgSslMode}` alongside the existing imports.
  - Add public methods:
    - `pub async fn new_with_entra(host: &str, port: u16, database: &str, user: &str, options: EntraAuthOptions) -> Result<Self>`
    - `pub async fn new_with_schema_and_entra(host: &str, port: u16, database: &str, user: &str, schema_name: Option<&str>, options: EntraAuthOptions) -> Result<Self>`
  - `new_with_entra` delegates to `new_with_schema_and_entra` with `None` (mirrors `new` → `new_with_schema`).
  - Implementation flow inside `new_with_schema_and_entra`:
    1. Resolve a `TokenSource` from `options` (calls into Phase 1 helper). On failure: return an `anyhow::Error` with message "Entra credential resolution failed: <cause>" (FR-007). This is the construction-time error contract.
    2. Acquire an initial token by calling `token_source.fetch_token(&[audience])`. Same error path on failure.
    3. Build `PgConnectOptions::new().host(host).port(port).database(database).username(user).password(&token.secret).ssl_mode(PgSslMode::VerifyFull)`.
    4. Build `PgPoolOptions` with the configured max/min connections and acquire timeout, then `connect_with(connect_options.clone())`.
    5. Wrap pool in `Arc`, set `schema_name`, run `MigrationRunner::migrate()`.
    6. Spawn the refresh task (see below) **after** migrations succeed so that startup failures don't leave a stray task.
  - Add a private helper `spawn_token_refresh_task(pool: Arc<PgPool>, token_source: Arc<dyn TokenSource>, base_options: PgConnectOptions, audience: String, refresh_interval_ceiling: Duration) -> JoinHandle<()>` implementing **expiry-driven refresh** *(per synthesis C1, resolution (c))*:
    - Maintains the most recently fetched `EntraToken` in scope.
    - Computes next sleep as `next_sleep = min(refresh_interval_ceiling, max(MIN_REFRESH, expires_at - now - SAFETY_MARGIN))` where `SAFETY_MARGIN` is e.g. 5 min and `MIN_REFRESH` is e.g. 30 s (concrete values picked at implementation and documented in `Docs.md`). This guarantees a refresh fires before the safety margin is breached, even if the operator misconfigures `refresh_interval_ceiling` larger than token lifetime.
    - On wake: calls `token_source.fetch_token(&[audience])`. On success: clones `base_options`, calls `.password(&token.secret)`, calls `pool.set_connect_options(new_options)`, replaces the cached token. On failure: logs `tracing::warn!(target = "duroxide::providers::postgres", error = %e, "Entra token refresh failed; will retry")` and schedules a short retry (e.g., min(30 s, half of remaining time before expiry)). The refresh task itself never aborts.
  - **FR-008 retryability** *(per synthesis C1, resolution (c) — belt-and-suspenders backstop)*: Extend `sqlx_to_provider_error` in `src/provider.rs` to map Postgres SQLSTATE `28000` (`invalid_authorization_specification`) and `28P01` (`invalid_password`) to `ProviderError::retryable` for the **operation that triggered the connect attempt**. This is a tightly scoped one-line addition to the existing `match db.code()` block; document inline that the rationale is Entra-token-rotation auth windows. Existing password-path callers are unaffected because their static passwords don't change at runtime — a 28xxx error there indicates a permanent misconfiguration AND a transient retry, but the worst case is one extra retry before the error surfaces, which is acceptable. (If the team prefers to gate this strictly to Entra connections, an alternative is to feature-flag via a `is_entra: bool` field on the provider struct and branch in the classifier; default to the simpler unconditional mapping unless the team objects.)
    - The combination of expiry-driven refresh (primary, makes stale-token windows rare) + classifier extension (backstop, ensures rare windows are retryable) satisfies FR-004 and FR-008 without the misclassification flagged in synthesis C1.
  - Store the `JoinHandle` on the provider struct (or in a side channel) so it is aborted in `Drop`. Add a `_refresh_task: Option<AbortOnDropHandle>` field; existing constructors leave it `None`. Use `tokio::task::AbortHandle` wrapped in a small newtype that aborts on drop. (Avoids leaking the task when a user drops the provider.)
  - Add `#[instrument(skip(options), target = "duroxide::providers::postgres")]` on the new constructors and on the refresh task entry function.
  - Ensure the existing `new` and `new_with_schema` are byte-identical to today (FR-006).

- **`src/entra.rs`**:
  - No further additions — `EntraToken` struct and `TokenSource` trait are landed in Phase 1.

- **Tests — extend in-crate `#[cfg(test)] mod tests` in `src/entra.rs` and add `#[cfg(test)] mod tests` in `src/provider.rs`** (unit-style, no `DATABASE_URL` required; rationale per Phase 1 test-placement note):
  - **Missing credential** (FR-007): Construct `EntraAuthOptions` with a `FakeTokenSource` that always fails. Assert `new_with_entra(...)` returns an error whose `Display`/`to_string()` contains the phrase "Entra credential" (or equivalent stable identifier). The test does NOT need a real Postgres because the failure happens before `connect_with`.
  - **TLS contract** (FR-005, SC-006): Add an internal helper `pub(crate) fn build_connect_options(host, port, database, user, token_secret) -> PgConnectOptions` and assert via `PgConnectOptions::get_ssl_mode()` that the resulting options have `ssl_mode = VerifyFull`. Crucially: assert there is no code path in `src/entra.rs` or new code in `src/provider.rs` that constructs `PgSslMode::Disable/Allow/Prefer/Require/VerifyCa` (verified by inspection / `grep PgSslMode src/`).
  - **Refresh swap** (SC-003) — *behavior-driven, no `password()` getter required*: `PgConnectOptions` in sqlx 0.8.6 exposes only setters for `password` (no public getter), so the test asserts the refresh task's *behavior*, not the swapped password directly. Build a `RecordingFakeTokenSource` whose `fetch_token` increments a `Arc<AtomicUsize>` counter and returns distinct tokens. Spawn the refresh task with a tiny `refresh_interval` (e.g., 50 ms) against a `connect_lazy_with`-built pool. After ~3 ticks, assert the counter is ≥ 3 (refresh task fires periodically) and that each successive call observed a different token. Combine with a separate pure-function test on `build_connect_options(...).password(secret)` that constructs two options structs from two distinct secrets and asserts they differ via `Debug`-formatting (sqlx's `Debug` masks but at least changes when the password changes) or by round-tripping into a connect URL where the password segment is observable.
  - **Construction-time pool tunables**: Assert that `EntraAuthOptions::max_connections(5)` results in a pool whose `PgPoolOptions` carries `max_connections == 5` (use `connect_lazy_with` to inspect via `Pool::options()`).
  - **Audience override**: With a `FakeTokenSource` that records the scope it was called with, assert that overriding the audience changes the scope passed to `fetch_token`.
  - **Edge-case manual verification bullets** added to the Manual Verification list below for the two spec edge cases (principal lacks DB role; Entra against non-Azure Postgres) — see synthesis finding S1.

- **Existing tests**: No modifications. They must pass byte-for-byte (SC-004). A `cargo nextest run --test postgres_provider_test` smoke run is the gate.

### Success Criteria

#### Automated Verification:
- [ ] `cargo build --workspace --all-targets` clean
- [ ] All unit tests pass: `cargo nextest run` (or `cargo test`)
- [ ] Existing provider validation suite passes unchanged: `cargo nextest run --test postgres_provider_test` (requires `DATABASE_URL` as today)
- [ ] Lint clean: `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] No new public `ProviderError` variants introduced (grep on `enum ProviderError` in the duroxide dep — should be untouched)
- [ ] Inspect: every Entra-path call site uses `PgSslMode::VerifyFull`; no other variant appears in `src/entra.rs` or in the new code in `src/provider.rs` (`grep PgSslMode src/`)

#### Manual Verification:
- [ ] Behavioral check (manual, requires Azure Postgres + managed identity): the example from the upcoming Phase 3 README block runs end-to-end against a real Azure Postgres Flexible Server. Run a small orchestration round-trip; observe migrations applied. Token-rotation behavior is observable via `RUST_LOG=duroxide::providers::postgres=debug` showing periodic refresh log lines.
- [ ] Behavioral check (manual): drop the provider; refresh task terminates (no stranded tokio task in tracing or `tokio-console` if attached).
- [ ] Behavioral check (manual): `az login` flow works from a developer workstation against the same instance (SC-002).
- [ ] **Edge case — principal lacks DB role** *(per synthesis S1, spec edge case `Spec.md:59`)*: connect with a valid Entra token whose principal has no `GRANT` for the target database role. Expected: connection returns a `permanent` error with SQLSTATE `42501` (`insufficient_privilege`) or similar — NOT routed through the Entra retryable mapping (28xxx). Verify Postgres error code in logs distinguishes this case from token-rotation errors.
- [ ] **Edge case — Entra against non-Azure Postgres** *(per synthesis S1, spec edge case `Spec.md:62`)*: point `new_with_entra` at a vanilla Postgres instance (e.g., local Docker) that does not understand Entra tokens as passwords. Expected: connection fails with a clear authentication error; the failure does not loop indefinitely (eventually surfaces as either a permanent classification or a retry-bounded transient classification per duroxide's standard retry envelope). Confirm `Docs.md` troubleshooting section covers the symptom.

---

## Phase 3: Documentation

**Objective**: Capture the as-built record in `Docs.md`; update user-facing docs (README, rustdoc, CHANGELOG) so SC-007 is met. Optionally land an opt-in integration test scaffold gated on env vars.

> **Note**: Implementer should load the `paw-docs-guidance` utility skill at the start of this phase for the `Docs.md` template plus README/rustdoc/CHANGELOG conventions used elsewhere in the repo.

### Changes Required

- **`.paw/work/entra-auth-support/Docs.md`** (new): Technical reference following `paw-docs-guidance` template. Captures: chosen `azure_identity` / `azure_core` versions and rationale; the `Pool::set_connect_options` rotation pattern (with link to sqlx 0.8 source for future maintainers); audience strings per Azure cloud; how `TokenSource` is the test seam; refresh-interval default and reasoning; how errors map across construction vs. runtime; a short troubleshooting playbook (no credential available; principal lacks DB role; TLS verification failure).

- **`README.md`**:
  - Add a new top-level **Authentication** section (or a `## Entra ID Authentication` subsection between **Custom Schema** and **Configuration**) with:
    - One runnable Rust example showing `new_with_entra` (managed identity in production) wrapped in `# async fn example() -> anyhow::Result<()> { ... # Ok(()) # }`.
    - A second example or note showing `new_with_schema_and_entra` for the multi-tenant schema case.
    - Required Azure setup (CREATE ROLE / sample `az` commands, granting Entra admin), recommended identity sources for production vs. local dev.
    - Sovereign-cloud override note pointing at `EntraAuthOptions::audience`.
  - Extend the **Configuration** section's table only if new env vars are introduced (Phase 1's design avoids them; verify and note in Docs.md).
  - Update the **Latest Release** block (or wherever changelog is summarized).

- **`src/lib.rs` rustdoc**:
  - Add a third `## Entra Authentication` subsection mirroring the README example, again as `rust,no_run` inside `# async fn example()` so doctests typecheck under `cargo test --doc` / `cargo doc`.

- **`src/provider.rs` rustdoc on `PostgresProvider`**:
  - Add a third example in the type-level rustdoc showing `new_with_entra`. Same `rust,no_run` pattern.

- **`CHANGELOG.md`** *(release-process item — not a blocking phase success criterion; flagged as scope-drift in synthesis C4)*:
  - New entry under the next version: "Added Entra ID authentication support via `new_with_entra` / `new_with_schema_and_entra` and `EntraAuthOptions`." Per the repo convention noted in stored memory, do not push or publish without explicit user confirmation; this PR only records the changelog.
  - Bump crate version per existing convention (e.g., 0.1.30 → 0.1.31). Confirm with the maintainer before tagging. *(Treated as optional release-management; FR-011/SC-007 only require README and rustdoc.)*

- **(Optional) `tests/entra_integration_test.rs`** — scaffolded but `#[ignore]`'d by default and gated on env vars `DUROXIDE_PG_AZURE_HOST`, `DUROXIDE_PG_AZURE_DB`, `DUROXIDE_PG_AZURE_USER`. Documents how a maintainer can run live verification against an Azure Postgres instance.

### Success Criteria

#### Automated Verification:
- [ ] `cargo doc --no-deps` builds clean (catches broken rustdoc links / examples)
- [ ] `cargo test --doc` passes
- [ ] Lint clean: `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] All Phase 1+2 success criteria still hold

#### Manual Verification:
- [ ] README Entra section reads naturally and matches the existing tone/format (same `## Usage` / `## Custom Schema` style)
- [ ] Docs.md captures decisions a future maintainer would otherwise have to rediscover (especially the sqlx-0.8 `set_connect_options` choice and Azure SDK version pinning rationale)
- [ ] CHANGELOG entry is accurate; version bump matches release convention

---

## References

- Issue: none
- Spec: `.paw/work/entra-auth-support/Spec.md`
- Research: `.paw/work/entra-auth-support/CodeResearch.md`
- Existing code anchors: `src/provider.rs:46-76` (constructors), `src/provider.rs:111-142` (error classifier), `src/lib.rs:1-56` (rustdoc), `Cargo.toml:30-44` (deps), `README.md:1-72` (doc structure).
- External: sqlx 0.8.6 source (`launchbadge/sqlx@v0.8.6`), `azure_identity` on crates.io, Microsoft Learn — Entra ID auth for Azure Database for PostgreSQL Flexible Server.
