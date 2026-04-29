//! Microsoft Entra ID (formerly Azure Active Directory) authentication support
//! for [`PostgresProvider`](crate::PostgresProvider).
//!
//! This module exposes [`EntraAuthOptions`] — the configuration type passed to
//! `PostgresProvider::new_with_entra` and `PostgresProvider::new_with_schema_and_entra`
//! (added in Phase 2) — plus the internal credential abstractions used to
//! fetch and rotate Entra access tokens.
//!
//! Azure SDK types (`azure_core::credentials::TokenCredential`,
//! `azure_identity::ManagedIdentityCredential`, etc.) are intentionally **not
//! re-exported**. The public surface is just [`EntraAuthOptions`]; everything
//! else stays internal so we can adapt to upstream churn without a breaking
//! change.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_core::credentials::TokenCredential;
use azure_identity::{
    DeveloperToolsCredential, ManagedIdentityCredential, WorkloadIdentityCredential,
};

/// The default audience/scope used when requesting Entra ID access tokens for
/// Azure Database for PostgreSQL Flexible Server in the Azure public cloud.
///
/// Sovereign clouds (Azure US Government, Azure China, etc.) require a
/// different audience; override via [`EntraAuthOptions::audience`].
pub const DEFAULT_AUDIENCE: &str = "https://ossrdbms-aad.database.windows.net/.default";

/// Default `max_connections` for the pool. Matches the password-path default
/// in [`PostgresProvider::new_with_schema`](crate::PostgresProvider::new_with_schema)
/// when `DUROXIDE_PG_POOL_MAX` is not set.
const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// Default acquire timeout for the pool. Matches the password path.
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default refresh-interval ceiling. Treated as a *ceiling* — the actual
/// refresh schedule is driven by each token's `expires_at` minus a safety
/// margin (see Phase 2 of the implementation plan / refresh task in
/// `provider.rs`).
///
/// Entra access tokens for Azure Database for PostgreSQL typically expire
/// after one hour; refreshing every twenty minutes keeps connect options
/// fresh well before expiry under normal conditions while keeping background
/// chatter minimal.
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(20 * 60);

/// Configuration for connecting to Azure Database for PostgreSQL using
/// Microsoft Entra ID authentication.
///
/// Construct with [`EntraAuthOptions::new`] (sensible Azure-public-cloud
/// defaults) and customize via the chainable mutators below.
///
/// # Example
///
/// ```rust,no_run
/// use duroxide_pg::entra::EntraAuthOptions;
/// use std::time::Duration;
///
/// let opts = EntraAuthOptions::new()
///     .max_connections(20)
///     .acquire_timeout(Duration::from_secs(45));
/// # let _ = opts;
/// ```
#[derive(Clone, Debug)]
pub struct EntraAuthOptions {
    audience: String,
    max_connections: u32,
    acquire_timeout: Duration,
    refresh_interval: Duration,
}

impl Default for EntraAuthOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl EntraAuthOptions {
    /// Create options with Azure public cloud defaults.
    pub fn new() -> Self {
        let max_connections = std::env::var("DUROXIDE_PG_POOL_MAX")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);
        Self {
            audience: DEFAULT_AUDIENCE.to_string(),
            max_connections,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
        }
    }

    /// Override the token audience/scope. Required for sovereign clouds
    /// (e.g., Azure US Government uses
    /// `https://ossrdbms-aad.database.usgovcloudapi.net/.default`).
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = audience.into();
        self
    }

    /// Override the pool's maximum connection count.
    pub fn max_connections(mut self, max: u32) -> Self {
        self.max_connections = max;
        self
    }

    /// Override the pool's connection acquisition timeout.
    pub fn acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Override the upper bound on time between refresh attempts. The
    /// background refresh task may refresh sooner than this interval if a
    /// token's `expires_at` is approaching.
    pub fn refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Internal: read accessor for the audience.
    pub(crate) fn audience_str(&self) -> &str {
        &self.audience
    }

    /// Internal: read accessor for max pool connections.
    pub(crate) fn max_connections_value(&self) -> u32 {
        self.max_connections
    }

    /// Internal: read accessor for acquire timeout.
    pub(crate) fn acquire_timeout_value(&self) -> Duration {
        self.acquire_timeout
    }

    /// Internal: read accessor for refresh-interval ceiling.
    pub(crate) fn refresh_interval_value(&self) -> Duration {
        self.refresh_interval
    }

    /// Construct the default [`TokenSource`] (the [`AzureIdentityTokenSource`]
    /// wrapping the chained credential).
    ///
    /// Surfaces a descriptive error if the underlying Azure SDK fails to build
    /// any credential in the chain.
    pub(crate) fn default_token_source(&self) -> Result<Arc<dyn TokenSource>> {
        let credential = build_default_chained_credential()
            .context("Entra credential resolution failed")?;
        Ok(Arc::new(AzureIdentityTokenSource::new(credential)))
    }
}

/// An Entra access token plus the wall-clock time at which it expires.
///
/// `Debug` is hand-written to redact the token secret. The struct is
/// `pub(crate)` so it never leaves the crate, but a panic backtrace or
/// `?token` formatter inside the crate could otherwise leak the bearer
/// string into logs.
#[derive(Clone)]
pub(crate) struct EntraToken {
    pub(crate) secret: String,
    pub(crate) expires_at: SystemTime,
}

impl std::fmt::Debug for EntraToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntraToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl EntraToken {
    pub(crate) fn new(secret: String, expires_at: SystemTime) -> Self {
        Self { secret, expires_at }
    }
}

/// Internal seam for fetching Entra tokens. Kept `pub(crate)` so the public
/// surface of `duroxide-pg` does not bind callers to a specific `azure_*`
/// version (mitigation for spec Risk #2). Tests in this crate inject fake
/// implementations via this trait.
#[async_trait]
pub(crate) trait TokenSource: Send + Sync {
    async fn fetch_token(&self, scopes: &[&str]) -> Result<EntraToken>;
}

/// Production [`TokenSource`] that delegates to an
/// [`azure_core::credentials::TokenCredential`].
pub(crate) struct AzureIdentityTokenSource {
    credential: Arc<dyn TokenCredential>,
}

impl AzureIdentityTokenSource {
    pub(crate) fn new(credential: Arc<dyn TokenCredential>) -> Self {
        Self { credential }
    }
}

#[async_trait]
impl TokenSource for AzureIdentityTokenSource {
    async fn fetch_token(&self, scopes: &[&str]) -> Result<EntraToken> {
        let access = self
            .credential
            .get_token(scopes, None)
            .await
            .map_err(|e| anyhow::anyhow!("Entra token acquisition failed: {e}"))?;

        let expires_at = offset_datetime_to_system_time(access.expires_on);
        Ok(EntraToken::new(access.token.secret().to_string(), expires_at))
    }
}

/// Convert an `azure_core::time::OffsetDateTime` (from `time` crate) into a
/// `SystemTime`. Treats pre-epoch values as `UNIX_EPOCH` (effectively immediate
/// expiry).
fn offset_datetime_to_system_time(t: azure_core::time::OffsetDateTime) -> SystemTime {
    let seconds = t.unix_timestamp();
    if seconds < 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds as u64))
        .unwrap_or(UNIX_EPOCH)
}

/// Build the default chained credential used when the caller does not provide
/// an explicit token source. The chain mimics the spirit of upstream
/// `DefaultAzureCredential` (which is not present in `azure_identity = 0.35`):
///
/// 1. `WorkloadIdentityCredential` — federated tokens for AKS Workload
///    Identity, GitHub OIDC, etc. (only succeeds when the corresponding env
///    vars are set).
/// 2. `ManagedIdentityCredential` — IMDS for Azure VMs, App Service,
///    Container Apps, Container Instances, Functions.
/// 3. `DeveloperToolsCredential` — Azure CLI (`az login`) and Azure Developer
///    CLI (`azd auth login`) for local development.
///
/// Returns an `azure_core::Error` from any failed inner constructor; the
/// caller wraps it into an `anyhow::Error`.
fn build_default_chained_credential() -> azure_core::Result<Arc<dyn TokenCredential>> {
    let mut sources: Vec<(&'static str, Arc<dyn TokenCredential>)> = Vec::new();
    // WorkloadIdentityCredential::new only succeeds when AZURE_FEDERATED_TOKEN_FILE
    // (and friends) are set. Skip silently otherwise so the chain still works
    // on a developer laptop. Note: leaving these env vars set on a NON-AKS host
    // can cause the chain to spend per-refresh latency budget here before
    // falling through; see Docs.md "Troubleshooting" for details.
    if let Ok(workload) = WorkloadIdentityCredential::new(None) {
        sources.push(("WorkloadIdentityCredential", workload));
    }
    sources.push(("ManagedIdentityCredential", ManagedIdentityCredential::new(None)?));
    sources.push(("DeveloperToolsCredential", DeveloperToolsCredential::new(None)?));
    Ok(Arc::new(ChainedCredential::new(sources)))
}

/// Tiny `TokenCredential` chain — tries each source in order, returns the
/// first that succeeds, aggregates errors otherwise. Modeled after upstream
/// [`DeveloperToolsCredential`]'s chaining behavior.
///
/// Sources are stored alongside a static class-name so the first successful
/// source can be logged at INFO — useful in production to confirm the
/// expected principal class is being used (e.g. AKS pods should show
/// `WorkloadIdentityCredential`, not `DeveloperToolsCredential`).
struct ChainedCredential {
    sources: Vec<(&'static str, Arc<dyn TokenCredential>)>,
}

impl ChainedCredential {
    fn new(sources: Vec<(&'static str, Arc<dyn TokenCredential>)>) -> Self {
        Self { sources }
    }
}

impl std::fmt::Debug for ChainedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChainedCredential")
    }
}

#[async_trait]
impl TokenCredential for ChainedCredential {
    async fn get_token(
        &self,
        scopes: &[&str],
        options: Option<azure_core::credentials::TokenRequestOptions<'_>>,
    ) -> azure_core::Result<azure_core::credentials::AccessToken> {
        let mut errors: Vec<String> = Vec::new();
        for (name, source) in &self.sources {
            match source.get_token(scopes, options.clone()).await {
                Ok(token) => {
                    tracing::info!(
                        target: "duroxide::providers::postgres",
                        credential = %name,
                        "Entra credential chain: token acquired",
                    );
                    return Ok(token);
                }
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Err(azure_core::Error::with_message_fn(
            azure_core::error::ErrorKind::Credential,
            move || {
                format!(
                    "All chained Entra credentials failed to acquire a token:\n  - {}",
                    errors.join("\n  - ")
                )
            },
        ))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test fixtures shared across crate unit tests (entra and provider).
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// `TokenSource` fixture used across `entra` and `provider` unit tests.
    /// Returns successive tokens from a script and records the scopes passed
    /// to each call.
    pub(crate) struct RecordingFakeTokenSource {
        scripted: Mutex<Vec<EntraToken>>,
        recorded_scopes: Mutex<Vec<Vec<String>>>,
        call_count: AtomicUsize,
        fail_with: Option<String>,
    }

    impl RecordingFakeTokenSource {
        pub(crate) fn with_tokens(tokens: Vec<EntraToken>) -> Arc<Self> {
            Arc::new(Self {
                scripted: Mutex::new(tokens),
                recorded_scopes: Mutex::new(Vec::new()),
                call_count: AtomicUsize::new(0),
                fail_with: None,
            })
        }

        pub(crate) fn always_failing(message: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                scripted: Mutex::new(Vec::new()),
                recorded_scopes: Mutex::new(Vec::new()),
                call_count: AtomicUsize::new(0),
                fail_with: Some(message.into()),
            })
        }

        pub(crate) fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        pub(crate) fn recorded_scopes(&self) -> Vec<Vec<String>> {
            self.recorded_scopes.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TokenSource for RecordingFakeTokenSource {
        async fn fetch_token(&self, scopes: &[&str]) -> Result<EntraToken> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.recorded_scopes
                .lock()
                .unwrap()
                .push(scopes.iter().map(|s| s.to_string()).collect());
            if let Some(msg) = &self.fail_with {
                return Err(anyhow::anyhow!("{msg}"));
            }
            let mut scripted = self.scripted.lock().unwrap();
            if scripted.is_empty() {
                return Err(anyhow::anyhow!("RecordingFakeTokenSource: script exhausted"));
            }
            Ok(scripted.remove(0))
        }
    }

    pub(crate) fn token(secret: &str, expires_in_secs: u64) -> EntraToken {
        EntraToken::new(
            secret.to_string(),
            SystemTime::now() + Duration::from_secs(expires_in_secs),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn defaults_match_password_path() {
        let opts = EntraAuthOptions::new();
        assert_eq!(opts.audience_str(), DEFAULT_AUDIENCE);
        assert_eq!(opts.max_connections_value(), 10);
        assert_eq!(opts.acquire_timeout_value(), Duration::from_secs(30));
        assert_eq!(opts.refresh_interval_value(), DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn audience_override_round_trips() {
        let opts = EntraAuthOptions::new()
            .audience("https://ossrdbms-aad.database.usgovcloudapi.net/.default");
        assert_eq!(
            opts.audience_str(),
            "https://ossrdbms-aad.database.usgovcloudapi.net/.default"
        );
    }

    #[test]
    fn pool_tunables_round_trip() {
        let opts = EntraAuthOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(45))
            .refresh_interval(Duration::from_secs(120));
        assert_eq!(opts.max_connections_value(), 5);
        assert_eq!(opts.acquire_timeout_value(), Duration::from_secs(45));
        assert_eq!(opts.refresh_interval_value(), Duration::from_secs(120));
    }

    #[tokio::test]
    async fn fake_token_source_returns_scripted_tokens() {
        let source = RecordingFakeTokenSource::with_tokens(vec![
            token("first", 3600),
            token("second", 3600),
        ]);
        let t1 = source.fetch_token(&[DEFAULT_AUDIENCE]).await.unwrap();
        let t2 = source.fetch_token(&[DEFAULT_AUDIENCE]).await.unwrap();
        assert_eq!(t1.secret, "first");
        assert_eq!(t2.secret, "second");
        assert_eq!(source.call_count(), 2);
        assert_eq!(
            source.recorded_scopes(),
            vec![vec![DEFAULT_AUDIENCE.to_string()], vec![DEFAULT_AUDIENCE.to_string()]]
        );
    }

    #[tokio::test]
    async fn fake_token_source_propagates_failures() {
        let source = RecordingFakeTokenSource::always_failing("simulated");
        let err = source
            .fetch_token(&[DEFAULT_AUDIENCE])
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("simulated"));
        assert_eq!(source.call_count(), 1);
    }

    #[test]
    fn offset_datetime_conversion_handles_negative() {
        // OffsetDateTime::UNIX_EPOCH is the canonical zero; pre-epoch values
        // should clamp to UNIX_EPOCH rather than panic / underflow.
        let pre_epoch = azure_core::time::OffsetDateTime::UNIX_EPOCH
            - azure_core::time::Duration::seconds(60);
        assert_eq!(offset_datetime_to_system_time(pre_epoch), UNIX_EPOCH);

        let post_epoch = azure_core::time::OffsetDateTime::UNIX_EPOCH
            + azure_core::time::Duration::seconds(120);
        let converted = offset_datetime_to_system_time(post_epoch);
        assert_eq!(converted, UNIX_EPOCH + Duration::from_secs(120));
    }

    /// Minimal `TokenCredential` stub for chain ordering tests.
    #[derive(Debug)]
    struct StubCred {
        ok: bool,
        label: &'static str,
    }

    #[async_trait]
    impl TokenCredential for StubCred {
        async fn get_token(
            &self,
            _scopes: &[&str],
            _options: Option<azure_core::credentials::TokenRequestOptions<'_>>,
        ) -> azure_core::Result<azure_core::credentials::AccessToken> {
            if self.ok {
                Ok(azure_core::credentials::AccessToken::new(
                    azure_core::credentials::Secret::new(format!("token-from-{}", self.label)),
                    azure_core::time::OffsetDateTime::UNIX_EPOCH
                        + azure_core::time::Duration::seconds(3_700_000_000),
                ))
            } else {
                Err(azure_core::Error::with_message(
                    azure_core::error::ErrorKind::Credential,
                    format!("{} failed", self.label),
                ))
            }
        }
    }

    #[tokio::test]
    async fn chained_credential_returns_first_success_in_chain_order() {
        let chain = ChainedCredential::new(vec![
            ("Failing", Arc::new(StubCred { ok: false, label: "Failing" })),
            ("Winner",  Arc::new(StubCred { ok: true,  label: "Winner" })),
            ("ShouldNotBeCalled", Arc::new(StubCred { ok: true, label: "ShouldNotBeCalled" })),
        ]);
        let token = chain.get_token(&["aud"], None).await.unwrap();
        assert_eq!(token.token.secret(), "token-from-Winner");
    }

    #[tokio::test]
    async fn chained_credential_aggregates_class_names_in_failure_message() {
        let chain = ChainedCredential::new(vec![
            ("Workload", Arc::new(StubCred { ok: false, label: "WorkloadIdentity" })),
            ("Managed",  Arc::new(StubCred { ok: false, label: "ManagedIdentity" })),
            ("Dev",      Arc::new(StubCred { ok: false, label: "DeveloperTools" })),
        ]);
        let err = chain.get_token(&["aud"], None).await.expect_err("all fail");
        let msg = format!("{err}");
        assert!(msg.contains("Workload"), "{msg}");
        assert!(msg.contains("Managed"), "{msg}");
        assert!(msg.contains("Dev"), "{msg}");
    }
}
