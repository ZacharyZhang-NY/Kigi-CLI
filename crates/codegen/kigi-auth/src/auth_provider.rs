//! Credential dependency-inversion seam for outbound HTTP made by the
//! data-collector. Shell installs `ShellAuthCredentialProvider` wrapping
//! `AuthManager` + `TokenRefresher`; data-collector code holds an
//! `Arc<dyn AuthCredentialProvider>`.

use reqwest::RequestBuilder;

use crate::visibility::HttpAuth;

/// Snapshot of the currently effective credentials, for callers that build
/// their own header maps (the OTel OTLP exporter) or that need the bearer
/// prefix for 401-attribution telemetry.
#[derive(Clone, Debug, Default)]
pub struct CredentialSnapshot {
    /// `None` when no auth is configured (CI / `--api-key` headless).
    pub token: Option<String>,
    /// Owner of `token`. `None` when no auth is configured or when the
    /// provider has no concept of user identity
    /// (`StaticAuthCredentialProvider`).
    pub user_id: Option<String>,
    /// `uuidv5(NAMESPACE_OID, deployment_key)`, set only for deployment-key auth.
    pub deployment_id: Option<String>,
    /// `uuidv5(NAMESPACE_OID, api_key)`, set only for `AuthMode::ApiKey`.
    pub api_key_id: Option<String>,
}

/// Source of truth for outbound auth on data-collector requests.
///
/// Supertrait of `HttpAuth` so a single impl satisfies both this trait
/// (refresh-aware snapshot + 401 recovery) and the visibility seam
/// (header construction).
#[async_trait::async_trait]
pub trait AuthCredentialProvider: HttpAuth + Send + Sync + 'static {
    /// Implementations should issue a cheap disk re-read
    /// (`AuthManager::refresh`) before snapshotting so callers see updates
    /// from sibling processes (`kigi-desktop`, `kigi login`). The `token`
    /// field MUST mirror the bearer that `HttpAuth::apply` would send on the
    /// wire so 401-attribution prefixes match the actual request.
    fn snapshot(&self) -> CredentialSnapshot;

    /// `true` if a different token was obtained, meaning the caller should
    /// retry the failed request once; `false` if no refresher is configured
    /// or the refresh failed.
    async fn refresh_after_unauthorized(&self) -> bool;

    /// Whether the provider holds a credential worth a real outbound attempt —
    /// an unexpired token (in memory or on disk), or a static key.
    fn has_usable_credential(&self) -> bool {
        true
    }
}

/// Non-refreshing provider for tests and for callers that pass a raw `&str`
/// token with no `AuthManager` available.
///
/// `bearer` duplicates whatever `inner` stamps into the `Authorization`
/// header; it exists so `snapshot().token` reports the same prefix that goes
/// out on the wire, which 401-attribution telemetry relies on.
pub struct StaticAuthCredentialProvider {
    inner: Box<dyn HttpAuth>,
    bearer: Option<String>,
}

impl StaticAuthCredentialProvider {
    /// `bearer` must be the token `inner.apply()` sends, or `snapshot()` will
    /// misreport the wire credential.
    pub fn new(inner: Box<dyn HttpAuth>, bearer: Option<String>) -> Self {
        Self { inner, bearer }
    }
}

impl std::fmt::Debug for StaticAuthCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticAuthCredentialProvider")
            .field("has_bearer", &self.bearer.is_some())
            .finish()
    }
}

impl HttpAuth for StaticAuthCredentialProvider {
    fn apply(&self, builder: RequestBuilder, base_url: &str) -> RequestBuilder {
        self.inner.apply(builder, base_url)
    }
}

#[async_trait::async_trait]
impl AuthCredentialProvider for StaticAuthCredentialProvider {
    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            token: self.bearer.clone(),
            ..Default::default()
        }
    }

    async fn refresh_after_unauthorized(&self) -> bool {
        false
    }
}
