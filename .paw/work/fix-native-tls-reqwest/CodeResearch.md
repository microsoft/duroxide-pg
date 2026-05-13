---
date: 2026-05-13T07:50:00-07:00
git_commit: b7a81df
branch: fix/native-tls-reqwest-feature
repository: microsoft/duroxide-pg
topic: "TLS connector missing from reqwest in the Entra credential chain"
tags: [research, codebase, entra, tls, cargo-features, reqwest, azure-identity]
status: complete
last_updated: 2026-05-13
---

# Research: TLS connector missing from reqwest in the Entra credential chain

## Research Question

Document why HTTPS requests issued by the Entra credential chain in
`duroxide-pg` fail at the connector layer with hyper's
`"invalid URL, scheme is not http"` error inside an AKS pod, and capture the
exact dependency-graph state that produces that runtime behavior.

## Summary

`duroxide-pg`'s Entra credential chain
(`build_default_chained_credential` in `src/entra.rs:288-301`) delegates to
`azure_identity = 0.35`, whose `WorkloadIdentityCredential` and
`ManagedIdentityCredential` POST to AAD over HTTPS using a reqwest-backed
HTTP transport that `azure_core` constructs internally. In the crate's
current dependency configuration, the resolved `reqwest 0.13.3` build has
**no TLS backend feature enabled** — `default-features = false` flows in
from `typespec_client_core` and no other crate in the graph activates a TLS
feature on `reqwest`. As a result, the compiled reqwest client supports only
plain HTTP. Every HTTPS request the chain makes is rejected by the connector
layer before any network I/O occurs.

The error surface that callers see is the `.context(...)` wrapper at
`src/provider.rs:246`: `"Entra credential resolution failed: could not
acquire an initial access token"`. The underlying chained-credential
aggregate message (assembled in `src/entra.rs:336-368`) and the
hyper/reqwest connector error are not exposed through `Display`, so the
true cause is invisible without `{:#}` formatting or a tracing subscriber.

## Documentation System

- **Framework**: Plain markdown
- **Docs Directory**: `docs/` (deep-dive notes), root for user-facing docs
- **Navigation Config**: N/A (no static-site generator)
- **Style Conventions**: README is the primary user surface; `docs/` holds
  operational deep-dives (`docs/entra-ci.md`, `docs/duroxide-blockers.md`,
  `docs/concurrent-instance-fetch-race.md`). Rustdoc on the public API
  surface uses runnable `rust,no_run` examples under
  `# async fn example()` to keep doctests typechecking.
- **Build Command**: `cargo doc --no-deps` for rustdoc
- **Standard Files**: `README.md` (root), `CHANGELOG.md` (root, semver-style
  per-version sections, see existing entries through `0.1.32`)

## Verification Commands

- **Test Command**: `cargo nextest run` (preferred if installed) or
  `cargo test` (fallback). Provider-validation suite:
  `cargo nextest run --test postgres_provider_test`. Live-Entra suite is
  opt-in via env var (see `tests/entra_live_test.rs:1-30`).
- **Lint Command**: `cargo clippy --all-targets` (per `.github/copilot-
  instructions.md` style guidance; no explicit lint script is checked in)
- **Build Command**: `cargo build --release`
- **Type Check**: `cargo check --all-targets` (Rust does typecheck as part
  of `cargo build`)
- **Dep-graph verification**: `cargo tree --target x86_64-unknown-linux-gnu
  --edges normal -i <crate>` (used below to verify TLS backend presence)

## Detailed Findings

### 1. Dependency declarations driving the resolved reqwest build

`Cargo.toml:46-63` declares the Azure SDK and accompanying dependencies:

- `Cargo.toml:46-59` — a multi-line comment block describing the intended
  FIPS posture. The comment claims:
  - That `default-features = false` on both Azure crates suppresses
    `reqwest_rustls`.
  - That the remaining features route HTTP transport through reqwest's own
    `default-tls` feature, "keeping the build on native-tls across all
    platforms."
  - That `cargo tree --target x86_64-unknown-linux-gnu --all-features
    --all-targets` was used to verify no rustls-based TLS crates appear.
- `Cargo.toml:60` — `azure_core = { version = "0.35", default-features =
  false, features = ["reqwest", "reqwest_deflate", "reqwest_gzip",
  "tokio"] }`.
- `Cargo.toml:61` — `azure_identity = { version = "0.35", default-features
  = false, features = ["tokio"] }`.
- `Cargo.toml:62-63` — `futures-util` (unrelated to TLS).

There is no explicit `reqwest` dependency in `[dependencies]`.

### 2. Feature-graph trace from `azure_core/reqwest` to the resolved reqwest features

Read from the registry sources resolved by `Cargo.lock`:

- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/azure_core-0.35.0/Cargo.toml`
  - `reqwest = ["typespec_client_core/reqwest"]`
  - `reqwest_deflate = ["reqwest", "typespec_client_core/reqwest_deflate"]`
  - `reqwest_gzip = ["reqwest", "typespec_client_core/reqwest_gzip"]`
  - `reqwest_rustls = ["reqwest", "typespec_client_core/reqwest_rustls"]`
  - `azure_core/reqwest` activates only the bare `dep:reqwest` path; no
    TLS feature.
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/typespec_client_core-0.14.0/Cargo.toml`
  - `[features] reqwest = ["dep:reqwest"]`
  - `[features] reqwest_rustls = ["reqwest", "reqwest/rustls"]`
  - `[dependencies.reqwest] version = "0.13.2"`, `features = ["stream"]`,
    `default-features = false`.
  - The `typespec_client_core` crate is the only declared parent of the
    optional `reqwest` dep; it pins `default-features = false`.
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/reqwest-0.13.3/Cargo.toml`
  - `default = ["default-tls"]`
  - `default-tls = ["rustls"]` (in reqwest 0.13.3 the named "default-tls"
    feature now resolves to rustls — it does **not** select native-tls).
  - `native-tls = ["__native-tls", "__native-tls-alpn"]`
  - `__native-tls = ["dep:hyper-tls", "dep:native-tls-crate", "__tls"]`
  - `rustls = ["__rustls-aws-lc-rs", "dep:rustls-platform-verifier",
    "__rustls"]`
  - `__rustls = ["dep:hyper-rustls", "dep:tokio-rustls", "dep:rustls", ...]`
  - The optional `hyper-tls` and `hyper-rustls` deps are gated under the
    `__native-tls` and `__rustls` feature flags respectively.

### 3. Resolved dependency tree on the current commit

Run from a `linux/amd64` `rust:1.91-trixie` container against `b7a81df`:

- `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i reqwest`
  shows `reqwest v0.13.3` activated by `typespec_client_core` only.
- `cargo tree -i rustls` → `error: package ID specification 'rustls' did
  not match any packages`.
- `cargo tree -i ring` → `did not match any packages`.
- `cargo tree -i aws-lc-rs` → `did not match any packages`.
- `cargo tree -i hyper-tls` → not in the resolved graph (verified by
  absence in the full `cargo tree` output).
- `cargo tree -i hyper-rustls` → not in the resolved graph.
- `native-tls v0.2.18` is present, pulled by sqlx-postgres via
  `runtime-tokio-native-tls` (the sqlx path, independent of reqwest).
- `tokio-native-tls v0.3.1` and `hyper-tls v0.6.0` are absent.

Net resolved reqwest build configuration: `default-features = false`,
features `["stream"]` (from `typespec_client_core`) plus `deflate` and
`gzip` (from `azure_core/reqwest_deflate` and `azure_core/reqwest_gzip` →
`reqwest/deflate` and `reqwest/gzip`). No TLS feature is activated on
reqwest.

### 4. Entra credential chain construction and usage

- `src/entra.rs:22` — `use azure_identity::{DeveloperToolsCredential,
  ManagedIdentityCredential, WorkloadIdentityCredential};`
- `src/entra.rs:25-30` — the audience constant
  `https://ossrdbms-aad.database.windows.net/.default` used for all
  default-chain token requests.
- `src/entra.rs:273-301` —
  `fn build_default_chained_credential() -> azure_core::Result<Arc<dyn
  TokenCredential>>`.
  - `src/entra.rs:295-297` —
    `WorkloadIdentityCredential::new(None)` is pushed onto the chain when
    construction succeeds (the constructor returns `Err` when the WI env
    vars are absent, so on developer machines it falls through silently).
  - `src/entra.rs:298` —
    `ManagedIdentityCredential::new(None)?` is pushed unconditionally.
  - `src/entra.rs:299` —
    `DeveloperToolsCredential::new(None)?` is pushed unconditionally.
- `src/entra.rs:315-368` — `struct ChainedCredential` and its
  `TokenCredential` impl:
  - `src/entra.rs:343-356` — sequential `get_token` calls per source,
    collecting `format!("{name}: {e}")` into `errors`.
  - `src/entra.rs:358-366` — final
    `azure_core::Error::with_message_fn(ErrorKind::Credential, ...)`
    producing the message
    `"All chained Entra credentials failed to acquire a token:\n  - <name>:
    <err>\n  - ..."`.

### 5. Public Entra constructors on `PostgresProvider`

- `src/provider.rs:232-255` —
  `pub(crate) async fn new_with_entra_with_token_source(host, port,
  database, user, schema_name, options, token_source, ssl_mode)`:
  - `src/provider.rs:243-246` — `token_source.fetch_token(&[audience])
    .await.context("Entra credential resolution failed: could not acquire
    an initial access token")?`. The `.context(...)` call wraps the inner
    azure_core::Error with an anyhow context layer; only the outer layer
    is shown by `Display`, so the chain aggregation from step 4 above is
    invisible without `{:#}` formatting.
- The public `connectWithEntra` / `connectWithSchemaAndEntra` constructors
  (referenced from the duroxide-node bindings) flow through this internal
  method with `EntraTokenSource::default_chain()` as the token source.

### 6. Runtime behavior observed inside an AKS pod

Direct execution of `WorkloadIdentityCredential::new(None)` followed by
`get_token(&["https://ossrdbms-aad.database.windows.net/.default"], None)`
inside a chkrawps7 AKS pod (Debian 13 / trixie-slim, glibc 2.41,
libssl3/libcrypto3 present) — using a standalone reproducer compiled
against the local `duroxide-pg` workspace — returns this error tree:

```
WorkloadIdentityCredential ERR (Display):
  WorkloadIdentityCredential authentication failed. retry policy expired
  and the request will no longer be retried
  To troubleshoot, visit https://aka.ms/azsdk/rust/identity/troubleshoot#workload

WorkloadIdentityCredential ERR (Debug):
  Error { context: CustomMessage(Custom { kind: Credential, error:
    Error { context: CustomMessage(Custom { kind: Connection, error:
      Error { context: CustomMessage(Custom { kind: Connection, error:
        reqwest::Error { kind: Request,
          url: "https://login.microsoftonline.com/<tenant>/oauth2/v2.0/token",
          source: hyper_util::client::legacy::Error(Connect,
            ConnectError("invalid URL, scheme is not http"))
        }
      }, "failed to execute `reqwest` request") }
    }, "retry policy expired and the request will no longer be retried") }
  }, "WorkloadIdentityCredential authentication failed. ...")
```

`hyper_util::client::legacy::Error(Connect, ConnectError("invalid URL,
scheme is not http"))` is hyper's identifying error when its connector
has no TLS support compiled in.

For the same pod and the same audience, `ManagedIdentityCredential` is
able to reach IMDS over plain HTTP (`http://169.254.169.254`) and receives
a structured HTTP 400 BadRequest body ("The requested identity has not
been assigned to this resource") — confirming the runtime can issue HTTP
requests; only HTTPS fails. `DeveloperToolsCredential` fails because the
container image has no `az` / `azd` on PATH (expected).

The Node-side `@azure/identity` package, run in the same pod against the
same audience, returns a 1952-byte bearer token from `WorkloadIdentity
Credential`, `ManagedIdentityCredential`, and `DefaultAzureCredential`
without modification — establishing that pod-level Workload Identity
plumbing, network egress to `login.microsoftonline.com`, and the federated
token file are all functioning.

### 7. Existing test coverage of the Entra path

- `tests/entra_live_test.rs:1-110` — opt-in live test gated on
  `DUROXIDE_PG_ENTRA_LIVE_TEST=1`. Exercises Entra against a real Azure
  Postgres instance using the developer-tools branch of the chain (relies
  on `az login` in the CI environment). The Rust reqwest client is only
  reached on the path that succeeds; failure modes of HTTPS calls are not
  asserted, and `WorkloadIdentityCredential` is not exercised by any
  in-repo test.
- `.github/workflows/entra-live-test.yml:1-113` — the CI workflow for the
  above test. Authenticates via `azure/login@v2` (which sets up the Azure
  CLI on the runner), so the credential chain resolves through
  `DeveloperToolsCredential` rather than `WorkloadIdentityCredential`.

## Code References

- `Cargo.toml:46-63` — Azure SDK feature configuration and FIPS-intent
  comment block.
- `Cargo.toml:60` — `azure_core` features.
- `Cargo.toml:61` — `azure_identity` features.
- `src/entra.rs:22` — credential imports.
- `src/entra.rs:25-30` — audience constant for PG AAD scope.
- `src/entra.rs:273-301` — default chain construction.
- `src/entra.rs:315-368` — `ChainedCredential` impl and aggregate error
  formatting.
- `src/provider.rs:232-255` — `new_with_entra_with_token_source`.
- `src/provider.rs:246` — anyhow `.context(...)` wrapper that hides the
  inner chain failure from `Display`.
- `tests/entra_live_test.rs:1-110` — live test (developer-tools branch
  only).
- `.github/workflows/entra-live-test.yml:1-113` — CI auth path.

## Architecture Documentation

- The crate's stated FIPS-compliance posture is "native-tls/openssl only,
  no rustls / ring / aws-lc-rs in the resolved graph"
  (`Cargo.toml:48-59`).
- The chained credential design (`src/entra.rs:273-368`) mirrors the
  spirit of `DefaultAzureCredential` from azure_identity in higher-level
  SDKs but is implemented locally because azure_identity 0.35 does not
  ship a `DefaultAzureCredential` aggregator. Sources are stored alongside
  a static class-name string, and the first successful credential is
  logged once per `ChainedCredential` instance via a `OnceLock` gate
  (`src/entra.rs:317`, `src/entra.rs:346-352`).
- The Entra refresh-task panic guard relies on `futures-util`'s
  `FutureExt::catch_unwind` (`Cargo.toml:62-63`).
- Provider-side error surfacing flows everything through `anyhow::Error`
  with `.context(...)` wrappers (`src/provider.rs:246`). This loses the
  underlying source chain at `Display`; only `{:#}` formatting or
  `Error::source()` traversal surfaces the inner error.

## Open Questions

None blocking. The dependency graph, runtime behavior, and code paths are
all observable from the local checkout and the registry sources resolved
by `Cargo.lock`.
