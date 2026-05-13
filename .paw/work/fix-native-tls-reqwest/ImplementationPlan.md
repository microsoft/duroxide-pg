# Fix Native TLS Reqwest Implementation Plan

## Overview

Restore HTTPS capability to the Entra credential chain in `duroxide-pg` by
activating a TLS backend on the resolved `reqwest 0.13` build. The current
dependency graph leaves reqwest with no TLS connector, which causes every
HTTPS request issued by `WorkloadIdentityCredential`, `ManagedIdentity
Credential` (when going through AAD over HTTPS), and the
`ClientAssertionCredential` token endpoint to fail with hyper's
`"invalid URL, scheme is not http"` error at the connector layer — before
any network I/O occurs (`CodeResearch.md` §1, §3, §6).

The fix is a single-line `Cargo.toml` change that adds an explicit
`reqwest` dependency with `features = ["native-tls"]`. Cargo feature
unification activates the native-tls (openssl-backed) connector on the
resolved reqwest build without disturbing any source code. Verified via
`cargo tree` that no `rustls`, `ring`, or `aws-lc-rs` crates are introduced,
preserving the crate's stated FIPS-compliance posture (`CodeResearch.md`
§3).

## Current State Analysis

- `Cargo.toml:60-61` declares `azure_core` and `azure_identity` with
  `default-features = false` and a hand-picked feature list intended to
  preserve native-tls. The accompanying comment block at `Cargo.toml:46-59`
  states that "this routes through reqwest's own `default-tls` feature".
- The intent is incorrect on two grounds (`CodeResearch.md` §2):
  1. `azure_core/reqwest = ["typespec_client_core/reqwest"]` and
     `typespec_client_core/reqwest = ["dep:reqwest"]` activate only the
     bare optional reqwest dep; neither flag enables a TLS feature.
     `typespec_client_core` declares the reqwest dep with
     `default-features = false`, so reqwest's own defaults are suppressed.
  2. In `reqwest 0.13.3`, the `default-tls` feature resolves to **rustls**,
     not native-tls — the name no longer means what it did in earlier
     reqwest releases. So even if defaults were active, they would
     contradict the stated FIPS posture.
- The Entra credential chain (`src/entra.rs:288-301`) builds correctly,
  but every HTTPS call inside the chain's `get_token` implementation
  fails at the reqwest connector. The aggregate failure message is
  obscured by the `.context(...)` wrapper at `src/provider.rs:246`, so
  callers see only `"Entra credential resolution failed: could not
  acquire an initial access token"` without the underlying cause.
- Existing test coverage (`tests/entra_live_test.rs`, `.github/workflows/
  entra-live-test.yml`) authenticates via the Azure CLI (`az login`) on
  the runner, which exercises `DeveloperToolsCredential`. The reqwest
  HTTP transport on the WI / MI branches is never traversed by CI, which
  is why the missing TLS feature was not caught upstream.

## Desired End State

`Cargo.toml` declares an explicit reqwest dependency with
`default-features = false, features = ["native-tls"]`. The resolved
dependency graph activates `hyper-tls`, `tokio-native-tls`, and
`native-tls-crate` (openssl-backed) without introducing `rustls`,
`hyper-rustls`, `tokio-rustls`, `ring`, or `aws-lc-rs`. The accompanying
comment block in `Cargo.toml` is updated to describe the actual feature
graph rather than the previous mistaken claim.

A regression test asserts that the reqwest-backed HTTP transport used by
`azure_core` accepts an `https://` URL at the connector layer (i.e. does
not return the `"invalid URL, scheme is not http"` connect error). The
test runs offline and does not depend on Azure credentials.

The fix is verifiable by:
- `cargo build --release` succeeds.
- `cargo nextest run` (or `cargo test`) passes including the new
  regression test.
- `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i rustls`
  / `-i ring` / `-i aws-lc-rs` each return "did not match any packages".
- `cargo tree -i hyper-tls` shows `hyper-tls v0.6.0` activated from
  reqwest.
- Manual: a linux/amd64 binary built from `examples/entra_debug.rs` (the
  reproducer used during diagnosis) executed inside the chkrawps7 AKS pod
  shows `WorkloadIdentityCredential` returns a valid access token.

The crate version is bumped to `0.1.33` and `CHANGELOG.md` documents the
bug fix.

## What We're NOT Doing

- Not modifying the Entra chain composition (`src/entra.rs:288-301`) — the
  bug is purely a build-configuration issue, not a code defect.
- Not changing the `.context(...)` error-wrapping behavior at
  `src/provider.rs:246`, even though it hides the underlying chain
  failure. Improving error surfacing is a separate, larger concern; see
  also the upstream comment hidden behind the wrapper. Tracked as a
  candidate for follow-up.
- Not adding a Workload-Identity live CI test. The reproducer
  (`examples/entra_debug.rs`) is preserved for local diagnosis but a full
  AKS-in-CI WI runner is out of scope for a bug-fix release.
- Not changing the sqlx TLS path (`runtime-tokio-native-tls`) — that path
  is unaffected; only the reqwest path used by `azure_core` is broken.
- Not bumping `azure_core` / `azure_identity` versions. The fix is
  agnostic to those crate versions and preserves the pinned `0.35`
  versions.

## Phase Status
- [x] **Phase 1: Apply Cargo.toml fix, regression test, version bump, CHANGELOG**
- [x] **Phase 2: Documentation** — `Docs.md` operator-facing note

## Phase Candidates
- [ ] Improve `provider.rs:246` error surfacing so the chained credential
  aggregate (`src/entra.rs:358-366`) is visible at `Display`, not only at
  `{:#}` or via `Error::source()` traversal. Today the actual chain
  failure is invisible to callers.
- [ ] Add a Workload-Identity smoke test to CI (requires deployed AKS test
  resource and a federated identity credential). Closes the test-coverage
  gap that allowed this bug to ship.

---

## Phase 1: Apply Cargo.toml fix, regression test, version bump, CHANGELOG

### Objective

Activate the native-tls connector on the resolved reqwest build through a
single `Cargo.toml` change. Add an offline regression test that exercises
the reqwest-backed HTTP transport's HTTPS connector. Bump the crate version
and add a `CHANGELOG.md` entry.

### Changes Required

- **`Cargo.toml`**:
  - Add a new line to `[dependencies]` (adjacent to the `azure_core` /
    `azure_identity` block) declaring
    `reqwest = { version = "0.13", default-features = false, features = ["native-tls"] }`.
    Cargo feature unification will activate the native-tls path on the
    reqwest already pulled in by `typespec_client_core`.
  - Update the comment block at `Cargo.toml:46-59` to:
    - Drop the incorrect claim that "this routes through reqwest's own
      `default-tls` feature".
    - Explain that `typespec_client_core` declares reqwest with
      `default-features = false`, so an explicit reqwest dep is required
      to opt into a TLS backend.
    - Note that `reqwest 0.13.3`'s `default-tls` resolves to rustls, so
      the explicit `native-tls` feature is the correct way to preserve
      the openssl posture.
    - Reference the verification: `cargo tree -i rustls / ring / aws-lc-rs`
      should all report "did not match any packages".
  - Bump `package.version` from `0.1.32` to `0.1.33`.

- **`tests/native_tls_regression.rs`** (new): a single
  `#[tokio::test(flavor = "current_thread")]` that:
  - Builds a `reqwest::Client` via `azure_core::http::new_http_client(None)`
    (or, if that constructor signature is unstable across the 0.35 minor
    range, builds via `reqwest::ClientBuilder::new().build()` and reaches
    the same connector code path).
  - Issues a single request to `https://127.0.0.1:1/` (a port chosen so the
    request fails fast at TCP-connect time).
  - Asserts the returned `reqwest::Error` is a *connection* failure (e.g.
    `ConnectionRefused`, `Os { code: ECONNREFUSED, .. }`, or a generic
    connect error whose `Display` does NOT contain the substring
    `"scheme is not http"`. The negative assertion is the regression
    contract: presence of that substring indicates the connector was
    rejected before any network attempt, which is the symptom this fix
    addresses.
  - Includes a documentation comment explaining the regression scenario
    and linking to `CHANGELOG.md` for the 0.1.33 entry.

  The test is offline, deterministic, fast (single connect attempt),
  and self-contained — it does not require Azure, federated tokens, or
  network egress to login.microsoftonline.com.

- **`CHANGELOG.md`**: Add a new entry at the top, above the existing
  `## 0.1.32 ...` section, following the existing convention. Suggested
  content (one-line summary + a paragraph describing the symptom, root
  cause, and fix):
  - **Title**: `## 0.1.33 — <date> (bug fix: native-tls reqwest feature)`
  - **Section**: `### Fixed` with a bullet covering the broken-HTTPS
    symptom, the missing `native-tls` feature on the resolved reqwest
    build, and a one-line note that `WorkloadIdentityCredential` /
    `ManagedIdentityCredential` (HTTPS-bound) chains were unreachable
    before this fix.

- **Tests**: `tests/native_tls_regression.rs` (above). No changes to
  `tests/entra_live_test.rs` — that test exercises the developer-tools
  branch and is unaffected by this fix.

### Success Criteria

#### Automated Verification

- [ ] `cargo build --release` succeeds.
- [ ] `cargo nextest run` (or `cargo test`) passes the full suite,
  including the new `tests/native_tls_regression.rs`.
- [ ] `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i
  rustls` returns "did not match any packages".
- [ ] `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i
  ring` returns "did not match any packages".
- [ ] `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i
  aws-lc-rs` returns "did not match any packages".
- [ ] `cargo tree --target x86_64-unknown-linux-gnu --edges normal -i
  hyper-tls` shows `hyper-tls v0.6.0` activated by reqwest.

#### Manual Verification

- [ ] Build the existing reproducer
  (`examples/entra_debug.rs`, preserved in the user's local checkout
  outside `.paw/`) for `x86_64-unknown-linux-gnu` against the patched
  crate. The binary, when executed inside the chkrawps7 pod, prints
  `"WorkloadIdentityCredential OK: token_len=<N> expires_on=..."` — i.e.
  a valid bearer token is acquired. Before this fix the same binary
  prints `"invalid URL, scheme is not http"` for WorkloadIdentity
  Credential.
- [ ] `Cargo.lock` updates reflect the dep-graph additions
  (`hyper-tls`, `tokio-native-tls`, `rustls-pki-types`) and the absence
  of `rustls`, `hyper-rustls`, `tokio-rustls`, `ring`, `aws-lc-rs`.

---

## Phase 2: Documentation

### Objective

Capture the as-built record for this fix in `Docs.md` for operator-facing
reference. No user-facing API changes — `README.md` is unaffected.

### Changes Required

- **`.paw/work/fix-native-tls-reqwest/Docs.md`**: Operator-facing note
  capturing:
  - One-paragraph symptom description (HTTPS calls from the Entra chain
    failing with `"invalid URL, scheme is not http"`).
  - The dependency-graph trace showing why reqwest was built without a TLS
    backend (`azure_core/reqwest` → `typespec_client_core/reqwest` →
    `dep:reqwest` with `default-features = false`).
  - The fix (explicit `reqwest` dep with `features = ["native-tls"]`) and
    the rationale (preserves the openssl posture; `reqwest 0.13.3`'s
    `default-tls` is rustls, so it cannot be used as the workaround).
  - Verification commands (the `cargo tree -i ...` invocations from the
    success criteria).
  - Cross-references to `CHANGELOG.md` 0.1.33 and the upstream PR.
  - Implementer should load `paw-docs-guidance` at the start of this phase
    for the Docs.md template and any cross-cutting conventions.

- **`README.md`**: No changes. The bug fix is invisible at the public API
  surface; the Entra section already documents the intended posture and
  remains accurate post-fix.

### Success Criteria

#### Automated Verification

- [ ] No new doc-build infrastructure exists; running `cargo doc --no-deps`
  succeeds without warnings introduced by this work.

#### Manual Verification

- [ ] `Docs.md` is internally consistent with `CHANGELOG.md` 0.1.33 and
  `Cargo.toml`'s updated comment block.
- [ ] All file:line references in `Docs.md` resolve to the corresponding
  source locations on the head of `fix/native-tls-reqwest-feature`.

---

## References

- Issue: none (filed as bug observed during PilotSwarm AKS deploy; see
  `Initial Prompt` in `.paw/work/fix-native-tls-reqwest/WorkflowContext.md`)
- Spec: not produced (minimal workflow)
- Research: `.paw/work/fix-native-tls-reqwest/CodeResearch.md`
