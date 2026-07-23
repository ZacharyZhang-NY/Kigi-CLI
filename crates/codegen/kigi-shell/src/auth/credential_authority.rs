//! THE credential chokepoint.
//!
//! One authority answers, for every outgoing inference request, the only
//! question that matters: **which credential — if any — may ride it?**
//! ([`CredentialAuthority::credential_class`]). Every credential sink
//! (`ModelsManager`, `MvpAgent`, `SessionActor`, the aux/summary/subagent
//! paths) funnels through this one rule, so none can derive the answer
//! differently and leak a bearer.
//!
//! # How omission is structurally prevented
//!
//! 1. [`SessionCredential`] wraps the bearer and has **no production
//!    constructor outside this module**. The only function in the crate that
//!    can build one is [`CredentialAuthority::credential_for`], which *requires*
//!    `(platform, base_url)` and holds the session's `EndpointsConfig` and
//!    primary [`AuthManager`] privately.
//! 2. Every API that stamps a session credential onto a request —
//!    `resolve_credentials`, `resolve_aux_model_sampling_config`,
//!    `try_resolve_model_credentials`,
//!    `resolve_chat_state_auth_type` — takes `Option<&SessionCredential>`,
//!    never `Option<&str>`. A new call site therefore *cannot compile* a leak:
//!    there is no way to produce the value without going through the rule.
//! 3. The authority owns the primary manager privately and exposes it only via
//!    [`CredentialAuthority::manager_for`] /
//!    [`CredentialAuthority::bearer_resolver_for`], which take the same
//!    `(platform, base_url)` pair — so the `bearer_resolver` sink is funnelled
//!    through the identical rule as the `api_key` sink.
//! 4. A guard asks [`CredentialAuthority::credential_class`] and MATCHES on the
//!    answer. There is no second, similarly-named boolean to pick by mistake:
//!    C1 was `takes_session_credential` — *may **a** session credential
//!    ride?* — misread as license to hand-carry the PRIMARY bearer.
//!
//! SECURITY: no token is ever logged, `Debug`-printed or `Display`ed here.

use std::sync::Arc;

use crate::agent::config::{EndpointsConfig, ModelEntry};
use crate::auth::AuthManager;

/// A session bearer this authority has cleared for one specific request
/// endpoint.
///
/// Opaque by construction: the inner `String` is private, the type is not
/// `Debug`/`Clone`-into-`String`, and the only production constructor is
/// [`CredentialAuthority::credential_for`]. See the module docs for why that
/// matters.
pub(crate) struct SessionCredential(String);

impl SessionCredential {
    /// The raw bearer. SECURITY: callers stamp this straight onto a request —
    /// never log it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Test-only forgery, so unit tests can exercise the *downstream*
    /// credential plumbing (`resolve_credentials`' BYOK-vs-session precedence,
    /// aux config shapes) without standing up an `AuthManager`. Deliberately
    /// `#[cfg(test)]`: production code has no way to build one.
    #[cfg(test)]
    pub(crate) fn for_test(key: &str) -> Self {
        Self(key.to_owned())
    }
}

/// WHICH credential — if any — may ride a request routed to a given
/// `(platform, base_url)` pair.
///
/// ONE question with three answers, not two look-alike booleans
/// (`takes_session_credential` / `takes_primary_credential`: identical
/// signatures, near-identical names, opposite answers on a subscription host).
/// C1 was caused by asking the first and stamping the credential the second
/// describes; with a single classifier a call site must MATCH on the answer, so
/// that mistake is not expressible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialClass {
    /// `platform`'s OWN pooled subscription-OAuth token, at its own registry
    /// host. NEVER the primary session bearer and never the house key.
    Pooled,
    /// The credential that authorizes the SESSION's own coding endpoint:
    /// the primary (`kimi-code` / platform-less) bearer.
    ///
    /// Deliberately NOT split into a separate `HouseKey` variant: the house
    /// `KIGI_API_KEY` is accepted by exactly this endpoint and no other, so it
    /// rides precisely this class. A fourth variant would re-create the
    /// two-similar-answers hazard this enum exists to remove.
    Primary,
    /// Nothing rides: every API-key registry platform, an OAuth platform
    /// redirected off its own host, and any endpoint that is not the session's.
    None,
}

impl CredentialClass {
    /// Stable label for structured logs. SECURITY: names a channel, never a
    /// token.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pooled => "pooled",
            Self::Primary => "primary",
            Self::None => "none",
        }
    }
}

/// The single authority over inference-time session credentials.
///
/// Construct one from the session's EFFECTIVE endpoints plus its primary
/// (first-party / Kimi) manager, then ask it about a request. Cheap to build
/// (a handful of `Option<String>` clones + an `Arc` clone).
#[derive(Clone)]
pub(crate) struct CredentialAuthority {
    /// The session's effective `[endpoints]` — config.toml layered over env.
    /// H3: `EndpointsConfig::proxy_url()` prefers `[endpoints]
    /// coding_api_base_url` from **config.toml** and only then falls back to
    /// `KIGI_CODE_BASE_URL`. A predicate that knows only the env var makes a
    /// managed/enterprise deployment lose its session bearer entirely (401 on
    /// every turn), so the endpoints are part of the authority's identity, not
    /// an afterthought.
    endpoints: EndpointsConfig,
    /// The primary session manager. PRIVATE: nothing hands it back, so a path
    /// holding a `CredentialAuthority` cannot reach `current_or_expired()`
    /// without naming an endpoint.
    primary: Option<Arc<AuthManager>>,
}

impl CredentialAuthority {
    pub(crate) fn new(endpoints: EndpointsConfig, primary: Option<Arc<AuthManager>>) -> Self {
        Self { endpoints, primary }
    }

    /// THE rule, stated once.
    ///
    /// - a subscription-OAuth platform (claude-pro-max, openai-codex,
    ///   github-copilot, xai-grok) rides ITS OWN pooled manager — never the
    ///   primary — and only to its own registry host (L10: a
    ///   `[model."claude-pro-max/x"]` override keeps `info.id` but can point
    ///   `base_url` anywhere, and would otherwise ship the Claude OAuth bearer
    ///   there);
    /// - `kimi-code` — the one `uses_oauth` platform with no `OAuthConfig` —
    ///   rides the PRIMARY session, and only at the session's own effective
    ///   coding endpoint;
    /// - every API-key registry platform (deepseek, openai, anthropic,
    ///   moonshot-*, …) rides NOTHING: its credential is that platform's API
    ///   key, already resolved into the catalog entry;
    /// - a platform-less model (a bare slug or a `[model.*]` block) is decided
    ///   purely by the ENDPOINT — BYOK detection probes `std::env::var` at call
    ///   time, so an unset/mistyped `env_key` must not turn into "send the
    ///   subscription bearer to `api.openai.com`".
    pub(crate) fn credential_class(
        &self,
        platform: Option<kigi_models::PlatformId>,
        base_url: &str,
    ) -> CredentialClass {
        match platform {
            Some(platform) => match platform.oauth() {
                Some(_) if self.endpoint_is_platform_host(platform, base_url) => {
                    CredentialClass::Pooled
                }
                // An OAuth platform pointed at a host that is NOT its own.
                Some(_) => CredentialClass::None,
                None if platform.uses_oauth() && self.is_session_coding_endpoint(base_url) => {
                    CredentialClass::Primary
                }
                // `kimi-code` off the session's endpoint, and every API-key
                // registry platform.
                None => CredentialClass::None,
            },
            None if self.is_session_coding_endpoint(base_url) => CredentialClass::Primary,
            None => CredentialClass::None,
        }
    }

    /// The manager behind [`Self::credential_class`]. Derived from the class, so
    /// the rule is stated exactly once and the two can never disagree.
    fn governing_manager(
        &self,
        platform: Option<kigi_models::PlatformId>,
        base_url: &str,
    ) -> Option<Arc<AuthManager>> {
        match self.credential_class(platform, base_url) {
            CredentialClass::Pooled => {
                platform
                    .and_then(kigi_models::PlatformId::oauth)
                    .map(|oauth| {
                        crate::auth::oauth_registry::global_manager_for(
                            &crate::auth::oauth_registry::pool_home(),
                            oauth,
                        )
                    })
            }
            CredentialClass::Primary => self.primary.clone(),
            CredentialClass::None => None,
        }
    }

    /// Whether `base_url` is the SESSION's own coding endpoint: the effective
    /// `[endpoints] coding_api_base_url` from **config.toml** (what a managed /
    /// enterprise deployment actually sets — H3), the `models_base_url`
    /// custom-endpoint mode, the `KIGI_CODE_BASE_URL` env override, a loopback
    /// dev proxy, or the compiled production endpoint.
    ///
    /// Deliberately NOT [`crate::util::is_first_party_url`], which is
    /// production-only and would break every custom deployment.
    fn is_session_coding_endpoint(&self, base_url: &str) -> bool {
        if crate::util::is_effective_coding_endpoint_url(base_url) {
            return true;
        }
        if crate::util::matches_trusted_base_url(base_url, &self.endpoints.proxy_url()) {
            return true;
        }
        self.endpoints
            .models_base_url
            .as_deref()
            .is_some_and(|models_base| crate::util::matches_trusted_base_url(base_url, models_base))
    }

    /// Whether `base_url` is `platform`'s own registry host — the guard that
    /// keeps a subscription-OAuth bearer from riding a redirected `[model.*]`
    /// override to a third party (L10).
    fn endpoint_is_platform_host(&self, platform: kigi_models::PlatformId, base_url: &str) -> bool {
        crate::util::matches_trusted_base_url(base_url, &platform.base_url())
    }

    /// The `AuthManager` that governs this request's bearer resolution,
    /// mid-session refresh and 401 recovery — or `None` when no session
    /// credential may ride (fail fast; never a silent fallback to the primary).
    pub(crate) fn manager_for(
        &self,
        platform: Option<kigi_models::PlatformId>,
        base_url: &str,
    ) -> Option<Arc<AuthManager>> {
        self.governing_manager(platform, base_url)
    }

    /// The session bearer to stamp as this request's `api_key`, or `None`.
    ///
    /// The ONLY production constructor of [`SessionCredential`].
    pub(crate) fn credential_for(
        &self,
        platform: Option<kigi_models::PlatformId>,
        base_url: &str,
    ) -> Option<SessionCredential> {
        self.governing_manager(platform, base_url)
            .and_then(|am| am.current_or_expired())
            .map(|auth| SessionCredential(auth.key))
    }

    /// A live sampler `bearer_resolver` over the governing manager, so the
    /// request keeps mid-session refresh / 401 recovery against the credential
    /// that actually belongs to its host.
    pub(crate) fn bearer_resolver_for(
        &self,
        platform: Option<kigi_models::PlatformId>,
        base_url: &str,
    ) -> Option<kigi_sampler::SharedBearerResolver> {
        self.manager_for(platform, base_url)
            .map(crate::session::acp_session::sampler_turn::auth_manager_bearer_resolver)
    }

    /// [`Self::credential_for`] for a resolved catalog entry: derives the
    /// platform and the base URL from the SAME entry, so the two can never be
    /// mismatched by a call site.
    pub(crate) fn credential_for_model(&self, entry: &ModelEntry) -> Option<SessionCredential> {
        let info = entry.info();
        self.credential_for(entry_platform(entry), &info.base_url)
    }

    /// [`Self::credential_for`] for the catalog model a routing slug resolves
    /// to. `current_key` is the SESSION's own selected catalog key (see
    /// [`crate::agent::models::entry_for_slug`]); pass `None` for aux /
    /// override slugs, which are not the session's selection.
    ///
    /// M5: a slug that is NOT in the catalog resolves through the SAME endpoint
    /// rule against the aux fallback endpoint
    /// (`EndpointsConfig::resolve_inference_base_url`, which is exactly where
    /// `resolve_aux_model_sampling_config`'s Tier-2 entry routes), not handed
    /// the primary unconditionally: `models_base_url` can point anywhere, so an
    /// off-catalog slug is not first-party by construction.
    ///
    /// M6: the platform and the base URL come from ONE
    /// [`crate::agent::models::entry_for_slug`] lookup, so they cannot disagree.
    pub(crate) fn credential_for_slug(
        &self,
        models: &indexmap::IndexMap<String, ModelEntry>,
        current_key: Option<&str>,
        slug: &str,
    ) -> Option<SessionCredential> {
        match crate::agent::models::entry_for_slug(models, current_key, slug) {
            Some(entry) => self.credential_for_model(entry),
            None => self.credential_for(None, &self.endpoints.resolve_inference_base_url()),
        }
    }
}

/// The registry platform a catalog entry belongs to (`info.id` is the managed
/// key `{platform}/{model}`). `None` for a bare / `[model.*]` entry.
pub(crate) fn entry_platform(entry: &ModelEntry) -> Option<kigi_models::PlatformId> {
    entry
        .info()
        .id
        .as_deref()
        .and_then(kigi_models::parse_managed_model_key)
        .map(|(platform, _)| platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthMode, KimiAuth, KimiCodeConfig};

    /// A primary holding a fixed in-memory bearer. The `TempDir` is returned so
    /// the caller keeps it alive; the token is read from memory, so on-disk
    /// contents are irrelevant.
    fn primary(key: &str) -> (tempfile::TempDir, Arc<AuthManager>) {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
        manager.hot_swap(KimiAuth {
            key: key.to_string(),
            auth_mode: AuthMode::OAuth,
            ..KimiAuth::test_default()
        });
        (dir, manager)
    }

    fn authority(endpoints: EndpointsConfig, primary: Arc<AuthManager>) -> CredentialAuthority {
        CredentialAuthority::new(endpoints, Some(primary))
    }

    fn platform(id: &str) -> kigi_models::PlatformId {
        kigi_models::PlatformId::parse(id).expect("known platform")
    }

    /// H3 (REGRESSION): the effective coding endpoint is
    /// `EndpointsConfig::proxy_url()`, which prefers `[endpoints]
    /// coding_api_base_url` from **config.toml** — the key the managed-config
    /// sync writes. A predicate that knows only `KIGI_CODE_BASE_URL` classifies
    /// such a deployment as third-party, withholds the api_key AND the
    /// resolver, and 401s on every turn.
    ///
    /// Revert-to-red: drop the `proxy_url()` arm from
    /// `is_session_coding_endpoint` (leaving only
    /// `is_effective_coding_endpoint_url`) and every assertion here fails —
    /// with NO env var set anywhere in the test.
    #[test]
    fn config_toml_coding_endpoint_still_rides_the_session_bearer() {
        let (_d, kimi) = primary("kimi-tok");
        let managed = "https://proxy.acme.com/v1";
        let auth = authority(
            EndpointsConfig {
                coding_api_base_url: Some(managed.to_string()),
                ..EndpointsConfig::default()
            },
            kimi.clone(),
        );
        assert_eq!(
            auth.credential_class(None, managed),
            CredentialClass::Primary,
            "a [model.*] entry inheriting the managed coding endpoint takes the session bearer"
        );
        assert_eq!(
            auth.credential_for(None, managed)
                .map(|c| c.expose().to_owned()),
            Some("kimi-tok".to_string()),
            "the managed deployment must still receive the session bearer"
        );
        assert!(
            auth.manager_for(None, managed).is_some(),
            "and must keep a live manager, or it loses refresh and 401 recovery"
        );
        // kimi-code entries route to `proxy_url()` too (models_fetch's
        // `platform_fetch_base`), so the platform arm must honour it as well.
        assert_eq!(
            auth.credential_for(Some(platform("kimi-code")), managed)
                .map(|c| c.expose().to_owned()),
            Some("kimi-tok".to_string()),
        );
        // A DIFFERENT authority (no managed key configured) must NOT trust it.
        let default_auth = authority(EndpointsConfig::default(), kimi);
        assert_eq!(
            default_auth.credential_class(None, managed),
            CredentialClass::None,
            "the managed host is only trusted for the session that configured it"
        );
    }

    /// The `models_base_url` custom-endpoint mode is equally invisible to the
    /// env-var-only predicate.
    #[test]
    fn config_toml_models_base_url_still_rides_the_session_bearer() {
        let (_d, kimi) = primary("kimi-tok");
        let custom = "https://models.acme.internal/v1";
        let auth = authority(
            EndpointsConfig {
                models_base_url: Some(custom.to_string()),
                ..EndpointsConfig::default()
            },
            kimi,
        );
        assert_eq!(
            auth.credential_for(None, custom)
                .map(|c| c.expose().to_owned()),
            Some("kimi-tok".to_string()),
        );
    }

    /// The compiled production endpoint and loopback proxies are unchanged.
    #[test]
    fn production_and_loopback_endpoints_are_unchanged() {
        let (_d, kimi) = primary("kimi-tok");
        let auth = authority(EndpointsConfig::default(), kimi);
        for url in [
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
            "http://127.0.0.1:8080/v1",
            "http://localhost:3000/v1",
            "http://[::1]:9000/v1",
        ] {
            assert_eq!(
                auth.credential_class(None, url),
                CredentialClass::Primary,
                "{url}: the session's own endpoint is byte-identical"
            );
        }
    }

    /// LEAK guard: every API-key registry platform, and any platform-less model
    /// on a third-party host, gets NO session credential and NO manager.
    #[test]
    fn third_party_endpoints_never_receive_the_primary() {
        let (_d, kimi) = primary("kimi-tok");
        let auth = authority(EndpointsConfig::default(), kimi);
        for id in [
            "deepseek",
            "openai",
            "anthropic",
            "moonshot-cn",
            "moonshot-ai",
        ] {
            let p = platform(id);
            assert!(
                auth.credential_for(Some(p), &p.base_url()).is_none(),
                "LEAK: {id} is an API-key platform — no session bearer may ride there"
            );
            assert!(auth.manager_for(Some(p), &p.base_url()).is_none());
        }
        for url in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com/v1",
            "https://api.moonshot.cn/v1",
            "",
        ] {
            assert!(
                auth.credential_for(None, url).is_none(),
                "LEAK: {url} is a third-party host"
            );
        }
    }

    /// L10: an OAuth platform whose `[model.*]` override redirects `base_url`
    /// to a third-party host keeps `info.id` — and must NOT ship that
    /// platform's pooled OAuth bearer there.
    #[tokio::test]
    async fn oauth_platform_redirected_to_a_third_party_host_gets_nothing() {
        let (_d, kimi) = primary("kimi-tok");
        let auth = authority(EndpointsConfig::default(), kimi);
        for id in [
            "claude-pro-max",
            "openai-codex",
            "github-copilot",
            "xai-grok",
        ] {
            let p = platform(id);
            assert!(
                auth.manager_for(Some(p), &p.base_url()).is_some(),
                "{id} keeps its own pooled manager on its own host"
            );
            assert!(
                auth.manager_for(Some(p), "https://third.party/v1")
                    .is_none(),
                "LEAK: {id} redirected to a third-party host must ship no bearer"
            );
            assert_eq!(
                auth.credential_class(Some(p), "https://third.party/v1"),
                CredentialClass::None,
                "LEAK: {id} redirected to a third-party host takes no session credential"
            );
        }
    }

    /// C1 — a subscription platform's own host DOES take a session credential,
    /// but it is that platform's POOLED token, never the primary / house key.
    #[test]
    fn a_subscription_host_classifies_pooled_never_primary() {
        let (_d, kimi) = primary("kimi-tok");
        let auth = authority(EndpointsConfig::default(), kimi);
        for id in [
            "claude-pro-max",
            "openai-codex",
            "github-copilot",
            "xai-grok",
        ] {
            let p = platform(id);
            assert_eq!(
                auth.credential_class(Some(p), &p.base_url()),
                CredentialClass::Pooled,
                "LEAK: {id}'s own host takes its POOLED token — never the primary / house key"
            );
        }
        // Every API-key registry platform: nothing at all.
        for id in ["deepseek", "openai", "anthropic", "moonshot-cn"] {
            let p = platform(id);
            assert_eq!(
                auth.credential_class(Some(p), &p.base_url()),
                CredentialClass::None
            );
        }
        // The primary channel is unchanged: kimi-code and a platform-less model
        // on the session's own endpoint, and nothing on a third-party host.
        for url in [
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
            "http://127.0.0.1:8080/v1",
        ] {
            assert_eq!(auth.credential_class(None, url), CredentialClass::Primary);
            assert_eq!(
                auth.credential_class(Some(platform("kimi-code")), url),
                CredentialClass::Primary
            );
        }
        assert_eq!(
            auth.credential_class(None, "https://api.openai.com/v1"),
            CredentialClass::None
        );
    }

    /// The four subscription-OAuth platforms draw from their OWN pooled
    /// managers — never the primary Kimi one, even under a Kimi session.
    #[tokio::test]
    async fn oauth_platforms_never_resolve_the_primary() {
        let (_d, kimi) = primary("kimi-tok");
        let auth = authority(EndpointsConfig::default(), kimi.clone());
        for id in [
            "claude-pro-max",
            "openai-codex",
            "github-copilot",
            "xai-grok",
        ] {
            let p = platform(id);
            let resolved = auth
                .manager_for(Some(p), &p.base_url())
                .expect("pooled manager");
            assert!(
                !Arc::ptr_eq(&resolved, &kimi),
                "{id} must NOT resolve the primary Kimi manager"
            );
            assert_ne!(
                auth.credential_for(Some(p), &p.base_url())
                    .map(|c| c.expose().to_owned()),
                Some("kimi-tok".to_string()),
                "{id} must never receive the primary Kimi bearer"
            );
        }
    }
}
