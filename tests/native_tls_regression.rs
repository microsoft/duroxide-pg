// Regression test for the missing-TLS-backend bug in the resolved reqwest build.
//
// Background: `azure_core 0.35`'s `reqwest` feature activates
// `typespec_client_core/reqwest`, which activates the optional reqwest dep
// with `default-features = false`. Without an explicit reqwest feature
// declaration in this crate's Cargo.toml, the resolved reqwest is compiled
// with NO TLS backend, and every HTTPS request fails at hyper's connector
// with `"invalid URL, scheme is not http"` — before any network I/O.
//
// This breaks `WorkloadIdentityCredential`, `ManagedIdentityCredential`
// (HTTPS branch), and `ClientAssertionCredential` token endpoints — the
// entire Entra credential chain assembled in `src/entra.rs`.
//
// This test exercises the exact `azure_core::http::new_http_client()`
// constructor used by `azure_identity` against an `https://` URL. The URL
// targets a closed local port so the request must fail at TCP-connect,
// NOT at connector initialization. The regression contract: the error
// `Display` must NOT contain `"scheme is not http"` — that substring is
// hyper's signature for "no TLS connector compiled in".
//
// See `CHANGELOG.md` (0.1.33) and `Cargo.toml`'s comment block for the
// dep-graph trace and rationale.

use azure_core::http::{new_http_client, Method, Request};
use url::Url;

#[tokio::test(flavor = "current_thread")]
async fn entra_https_connector_is_compiled_in() {
    let client = new_http_client(None);

    // Closed local port — fail-fast at TCP connect, no DNS, no network egress.
    let url = Url::parse("https://127.0.0.1:1/").expect("valid url");
    let req = Request::new(url, Method::Get);

    let err = client
        .execute_request(&req)
        .await
        .expect_err("https://127.0.0.1:1/ must fail to connect");

    // Format the full source chain — the bug's signature is buried under
    // anyhow-style wrapping otherwise.
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("scheme is not http"),
        "Regression: reqwest has no TLS backend compiled in. \
         The dependency graph dropped its TLS connector — likely the explicit \
         `reqwest = {{ ..., features = [\"native-tls\"] }}` line was removed from \
         Cargo.toml. Full error chain:\n{chain}"
    );
}
