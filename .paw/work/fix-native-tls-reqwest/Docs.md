# Docs: native-tls reqwest feature fix (duroxide-pg 0.1.33)

## Symptom

Every HTTPS request issued by the Microsoft Entra credential chain in
`duroxide-pg 0.1.31`–`0.1.32` failed at hyper's connector layer with:

```
reqwest::Error {
  url: "https://login.microsoftonline.com/.../oauth2/v2.0/token",
  source: hyper_util::client::legacy::Error(
    Connect,
    ConnectError("invalid URL, scheme is not http")
  )
}
```

The error originates at the connector layer — **before any DNS lookup or
TCP connect attempt**. The substring `"invalid URL, scheme is not http"`
is hyper's signature for "no TLS connector compiled into the HTTP client".

Affected callers: `WorkloadIdentityCredential` (AKS Workload Identity),
`ManagedIdentityCredential` when the IMDS endpoint required HTTPS, and
the `ClientAssertionCredential` token endpoint used by federated
identity flows. `DeveloperToolsCredential` (the `az login` branch) was
unaffected because it shells out to the Azure CLI binary rather than
calling AAD over reqwest.

Operational impact: `PostgresProvider::new_with_entra` and
`new_with_schema_and_entra` were unreachable in any production topology
that relied on Workload Identity — passwordless Entra was effectively
broken on AKS, Container Apps, and any other host that does not have the
Azure CLI installed locally.

## Root cause

The crate's `Cargo.toml` declared the Azure SDK dependencies as:

```toml
azure_core = { version = "0.35", default-features = false,
                features = ["reqwest", "reqwest_deflate", "reqwest_gzip", "tokio"] }
azure_identity = { version = "0.35", default-features = false,
                    features = ["tokio"] }
```

The intent — captured in the original comment block above those lines —
was to preserve a native-tls / openssl-only TLS posture for FIPS
compliance while still enabling reqwest as the HTTP transport. The
implementation was incorrect on two grounds:

**1. `azure_core/reqwest` does not enable any reqwest defaults.**

Tracing the Cargo feature graph for `azure_core 0.35`:

- `azure_core/reqwest = ["typespec_client_core/reqwest"]`
- `typespec_client_core/reqwest = ["dep:reqwest"]` (activates the
  optional dep only — no features)
- `typespec_client_core 0.14.0` declares
  `reqwest = { version = "0.13", default-features = false, features = ["stream"] }`

The resolved reqwest crate is built with `default-features = false`
plus `stream` only. No TLS backend is compiled in. The constructor at
`typespec_client_core::http::clients::reqwest::new_reqwest_client`
(used by `azure_core::http::new_http_client`) calls
`reqwest::ClientBuilder::new().build()` without configuring TLS
explicitly, so it inherits the crate's compile-time feature flags. With
no TLS feature active, the resulting hyper-util connector rejects every
HTTPS URL at request time.

**2. `reqwest 0.13`'s `default-tls` is rustls, not native-tls.**

The previous Cargo.toml comment claimed the configuration "routes through
reqwest's own `default-tls` feature, keeping the build on native-tls".
Even if `default-tls` had been active, it would NOT have routed through
native-tls — `default-tls` in reqwest 0.13.3 resolves to `rustls`. The
feature name was kept for compatibility but its meaning changed in the
reqwest 0.12 → 0.13 transition.

## Why CI didn't catch it

The only existing live test for the Entra path,
`tests/entra_live_test.rs`, authenticates via
`DeveloperToolsCredential` (`az login` on the CI runner). That
credential variant invokes the Azure CLI binary as a child process and
never traverses the reqwest-backed HTTP transport. The Workload Identity
branch of the credential chain has no live test in this repo, so the
missing TLS backend was invisible to CI.

## Fix

A single line added to `[dependencies]` in `Cargo.toml`:

```toml
reqwest = { version = "0.13", default-features = false, features = ["native-tls"] }
```

Cargo's feature unification merges this declaration with
`typespec_client_core`'s reqwest dep (same major.minor version range,
same resolved 0.13.3) and the union of features —
`["stream", "native-tls"]` — is applied to the single resolved reqwest
crate. The native-tls feature activates `hyper-tls` and the
`native-tls-crate` (openssl-backed on Linux), giving hyper a working
TLS connector for HTTPS URLs.

The accompanying comment block in `Cargo.toml:46-72` is updated to
describe the actual feature graph and the rationale for the explicit
dep.

No source code changes are required. The fix is purely a build-
configuration adjustment.

## Verification

### Automated

```
cargo build --release
cargo test --release --test native_tls_regression
cargo clippy --all-targets
```

The new test at `tests/native_tls_regression.rs` constructs the
`azure_core::http` client via the same `new_http_client(None)`
constructor that `azure_identity` uses internally, then issues a GET
to `https://127.0.0.1:1/`. The request must fail (port 1 is closed) but
the failure must be a *network* error, not a *connector* error. The
regression contract asserts the error chain does NOT contain the
substring `"scheme is not http"` — that substring is the signature of
the bug being fixed.

### Dependency graph

```
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i rustls
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i ring
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i aws-lc-rs
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i hyper-rustls
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i tokio-rustls
```

Each command must return `"package ID specification ... did not match
any packages"` — the FIPS-compliance posture (no rustls-based TLS) is
preserved.

```
cargo tree --target x86_64-unknown-linux-gnu --edges normal -i hyper-tls
```

Must show `hyper-tls v0.6.0` activated by `reqwest`.

### Live verification (manual)

Build a Linux x86_64 binary of the diagnostic reproducer (kept locally
outside the repo at `examples/entra_debug.rs` during diagnosis) against
the patched crate, then execute it inside an AKS pod with Workload
Identity configured. The output must include
`"WorkloadIdentityCredential OK: token_len=<N> expires_on=<RFC3339>"`.
Before the fix, the same binary printed
`"WorkloadIdentityCredential failed: ... invalid URL, scheme is not http"`.

## Forward-compatibility note

This fix depends on Cargo's per-crate-version feature unification
correctly merging the explicit `reqwest 0.13` declaration with
`typespec_client_core`'s declaration. If `typespec_client_core` is ever
bumped to declare reqwest at a different major (e.g. `0.14`), the two
declarations will resolve to *different* reqwest crates and the fix
will silently regress — the test in `tests/native_tls_regression.rs` is
the canary that catches that scenario.

If a future maintainer upgrades `azure_core` past `0.35`, re-verify the
feature graph by running the `cargo tree -i` commands above and
re-running the regression test before publishing.

## References

- `CHANGELOG.md` 0.1.33 entry
- `Cargo.toml:46-72` updated comment block
- `tests/native_tls_regression.rs` regression test
- `.paw/work/fix-native-tls-reqwest/CodeResearch.md` §1–§3 for the
  feature-graph trace
- `.paw/work/fix-native-tls-reqwest/ImplementationPlan.md` for the
  phased work record
- reqwest 0.13 feature definitions:
  https://docs.rs/crate/reqwest/0.13.3/source/Cargo.toml.orig
- typespec_client_core 0.14 reqwest dep:
  https://docs.rs/crate/typespec_client_core/0.14.0/source/Cargo.toml.orig
