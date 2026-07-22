//! Process-global per-provider OAuth `AuthManager` pool for INFERENCE-time auth.
//!
//! A session binds its primary (Kimi / first-party) [`AuthManager`] for the
//! subscription path, but a `uses_oauth` platform that carries an
//! [`kigi_models::OAuthConfig`] (xai-grok today) needs its OWN scope-keyed
//! manager for every per-turn decision — bearer resolution, proactive /
//! on-expiry refresh, and 401 recovery. Reusing the Kimi manager for a grok
//! turn would transmit the Kimi subscription bearer to `api.x.ai` (a
//! cross-provider leak, guaranteed 401) and, without proactive refresh, would
//! 401 every turn once the ~1h grok token expired until a process restart.
//!
//! The pool is the SINGLE SOURCE OF TRUTH: one long-lived `AuthManager` per
//! generic-oauth scope, each wired with the SAME lifecycle as the primary Kimi
//! manager (`configure_refresher()` + `start_proactive_refresh()`) so the
//! on-disk token stays fresh and a 401 recovers via the provider's own manager.
//! Managers are built ON DEMAND from the on-disk token ([`global_manager_for`]),
//! so a login landing AFTER a session spawned self-heals — no frozen per-session
//! snapshot. [`manager_for_model`] routes a managed catalog key to the pool
//! (oauth platform) or to the session's primary (everything else).
//!
//! SECURITY: access/refresh tokens and resolved bearers are NEVER logged here.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::auth::AuthManager;

/// Process-wide pool of live per-scope OAuth managers.
///
/// Auth is process-global (one user), so a single manager per scope is correct
/// and lets the proactive-refresh task start exactly once per scope no matter
/// how many sessions spawn. Keyed by the OAuth `scope_key` (`oauth/xai`, …).
fn oauth_manager_pool() -> &'static Mutex<HashMap<&'static str, Arc<AuthManager>>> {
    static POOL: OnceLock<Mutex<HashMap<&'static str, Arc<AuthManager>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The kigi home every OAuth-pool call site resolves from. Single definition so
/// the pool, the aux/summary token routing and the session's inference manager
/// can never read different homes.
///
/// Production: [`crate::util::kigi_home::kigi_home`]. LIB TESTS: a
/// process-lifetime `TempDir`, unconditionally — the pool is process-global and
/// every manager it builds starts a never-cancelled proactive-refresh loop, so
/// a unit test resolving the real `~/.kigi` would read the developer's stored
/// OAuth tokens and, 60 s later, fire REAL refresh requests against them.
/// Deliberately not a per-test opt-in that can be forgotten: `kigi_home()` is
/// itself a `OnceLock` an earlier test has usually already resolved to the real
/// home, so setting `KIGI_SHARE_DIR` in a test cannot pin it after the fact.
pub(crate) fn pool_home() -> std::path::PathBuf {
    #[cfg(test)]
    {
        static TEST_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
        TEST_HOME
            .get_or_init(|| tempfile::tempdir().expect("tempdir for the test OAuth pool"))
            .path()
            .to_path_buf()
    }
    #[cfg(not(test))]
    crate::util::kigi_home::kigi_home()
}

/// Get-or-create the process-global manager for `oauth`, wiring the same
/// refresher + proactive-refresh lifecycle as the primary Kimi manager the
/// FIRST time a scope is seen. The manager reads the on-disk token at
/// construction (thereafter kept fresh by the proactive-refresh loop), so a
/// grok login that lands after this scope was first built is adopted on the
/// manager's own refresh tick — no session ever needs re-spawning.
///
/// MUST be called from within a Tokio runtime (the proactive-refresh loop
/// spawns a task, mirroring the primary).
pub(crate) fn global_manager_for(
    kigi_home: &Path,
    oauth: &'static kigi_models::OAuthConfig,
) -> Arc<AuthManager> {
    let mut pool = oauth_manager_pool().lock();
    if let Some(existing) = pool.get(oauth.scope_key) {
        return existing.clone();
    }
    let manager = Arc::new(AuthManager::new_oauth_provider(kigi_home, oauth));
    manager.configure_refresher();
    // Never-cancelled token = process-lifetime, matching the api-server /
    // per-session eager-refresh sites that pass a fresh token.
    manager.start_proactive_refresh(tokio_util::sync::CancellationToken::new());
    pool.insert(oauth.scope_key, manager.clone());
    manager
}

/// The `AuthManager` that governs INFERENCE auth for `managed_key`
/// (`{platform}/{model}`, e.g. `xai-grok/grok-4-latest`).
///
/// A generic device-code OAuth platform routes to ITS OWN scope-keyed manager
/// from the process-global pool ([`global_manager_for`], built on demand from
/// the on-disk token); every other key (Kimi, API-key platforms, `[model.*]`
/// entries, or an unprefixed bare id) routes to `primary`.
///
/// The pool is the single source of truth — there is no per-session snapshot to
/// freeze at spawn, so a grok login that happens AFTER a session spawned is
/// resolved correctly on the next grok turn. A grok key NEVER resolves to
/// `primary`: even before the user logs into grok the pooled manager simply
/// holds no token (its bearer / api_key is then `None`), so the Kimi
/// subscription bearer can never reach a third-party host — fail-fast, never a
/// silent fallback to the Kimi manager.
pub(crate) fn manager_for_model(
    kigi_home: &Path,
    managed_key: &str,
    primary: Option<&Arc<AuthManager>>,
) -> Option<Arc<AuthManager>> {
    if let Some((platform, _)) = kigi_models::parse_managed_model_key(managed_key)
        && let Some(oauth) = platform.oauth()
    {
        return Some(global_manager_for(kigi_home, oauth));
    }
    primary.cloned()
}

/// The SESSION token (the raw bearer/key string) that may ride an INFERENCE
/// request routed to `platform` at `base_url`. Used by the aux-model, summary
/// and subagent-override wire paths, where the result is stamped straight into
/// [`crate::agent::config::resolve_credentials`] as the request's `api_key`.
///
/// - a generic device-code OAuth platform (xai-grok, claude-pro-max,
///   github-copilot, openai-codex) draws from ITS OWN pooled manager; when that
///   provider has no stored session the result is `None` — never `primary`;
/// - `kimi-code`, and a platform-less model whose endpoint IS the session's own
///   coding endpoint (incl. a `KIGI_CODE_BASE_URL` deployment or a loopback
///   proxy), yield the primary's current-or-expired token — byte-identical to
///   reading it directly;
/// - every API-key registry platform, and every `[model.*]` block pointed at a
///   third-party host, yields `None`. Handing them `primary` put the user's
///   Kimi subscription bearer on `api.deepseek.com` / `api.moonshot.cn` / …
///   as the request's `api_key`.
///
/// SECURITY: the resolved token is never logged.
pub(crate) fn session_key_for_endpoint(
    platform: Option<kigi_models::PlatformId>,
    base_url: &str,
    primary: Option<&Arc<AuthManager>>,
) -> Option<String> {
    if let Some(oauth) = platform.and_then(kigi_models::PlatformId::oauth) {
        return global_manager_for(&pool_home(), oauth)
            .current_or_expired()
            .map(|a| a.key);
    }
    if !crate::agent::auth_method::platform_takes_session_credential(platform, base_url) {
        return None;
    }
    primary
        .and_then(|am| am.current_or_expired())
        .map(|a| a.key)
}

/// [`session_key_for_endpoint`] for the catalog model whose routing slug (or
/// catalog key) is `slug`.
///
/// A slug absent from the catalog keeps the pre-registry behaviour: the aux
/// resolver's Tier-2 fallback builds its entry against
/// `EndpointsConfig::resolve_inference_base_url` (first-party), so the primary
/// still governs.
pub(crate) fn session_key_for_catalog_model(
    models: &indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    slug: &str,
    primary: Option<&Arc<AuthManager>>,
) -> Option<String> {
    let Some(entry) = crate::agent::config::find_model_by_id(models, slug) else {
        return primary
            .and_then(|am| am.current_or_expired())
            .map(|a| a.key);
    };
    let info = entry.info();
    session_key_for_endpoint(
        info.id
            .as_deref()
            .and_then(kigi_models::parse_managed_model_key)
            .map(|(platform, _)| platform),
        &info.base_url,
        primary,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::KimiCodeConfig;
    use crate::auth::{AuthMode, KimiAuth};

    /// A Kimi manager holding a fixed in-memory bearer, standing in for a
    /// session's primary. The `TempDir` is returned so the caller keeps it
    /// alive; the token is read from memory (`current_or_expired`), so disk
    /// contents are irrelevant to the assertion.
    fn primary_with_token(key: &str) -> (tempfile::TempDir, Arc<AuthManager>) {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
        manager.hot_swap(KimiAuth {
            key: key.to_string(),
            auth_mode: AuthMode::OAuth,
            ..KimiAuth::test_default()
        });
        (dir, manager)
    }

    fn xai_oauth() -> &'static kigi_models::OAuthConfig {
        kigi_models::PlatformId::XaiGrok
            .oauth()
            .expect("xai-grok carries an OAuthConfig")
    }

    fn claude_oauth() -> &'static kigi_models::OAuthConfig {
        kigi_models::PlatformId::ClaudeProMax
            .oauth()
            .expect("claude-pro-max carries an OAuthConfig")
    }

    fn copilot_oauth() -> &'static kigi_models::OAuthConfig {
        kigi_models::PlatformId::GithubCopilot
            .oauth()
            .expect("github-copilot carries an OAuthConfig")
    }

    fn codex_oauth() -> &'static kigi_models::OAuthConfig {
        kigi_models::PlatformId::OpenaiCodex
            .oauth()
            .expect("openai-codex carries an OAuthConfig")
    }

    /// `session_key_for_endpoint` for a managed catalog key, resolving the
    /// platform and its base URL from the registry exactly as the catalog entry
    /// would.
    fn session_key_for_key(
        managed_key: &str,
        primary: Option<&Arc<AuthManager>>,
    ) -> Option<String> {
        let platform = kigi_models::parse_managed_model_key(managed_key).map(|(p, _)| p);
        let base_url = platform
            .map(kigi_models::PlatformId::base_url)
            .unwrap_or_default();
        session_key_for_endpoint(platform, &base_url, primary)
    }

    /// An `openai-codex/<model>` turn resolves to the process-global pooled
    /// openai-codex manager (its OWN `oauth/openai-codex` scope), NEVER the
    /// primary Kimi manager — the same leak-safe routing as the other OAuth
    /// platforms, and a DISTINCT pool entry from each. Fail-fast: even with a
    /// Kimi primary, a codex turn never yields the Kimi bearer.
    #[tokio::test]
    async fn openai_codex_model_resolves_to_its_own_manager_not_kimi() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved = manager_for_model(home.path(), "openai-codex/gpt-5.5", Some(&kimi))
            .expect("openai-codex model resolves to its pooled manager");
        assert!(
            !Arc::ptr_eq(&resolved, &kimi),
            "openai-codex must NOT resolve to the Kimi manager"
        );
        assert!(
            Arc::ptr_eq(&resolved, &global_manager_for(home.path(), codex_oauth())),
            "openai-codex must resolve to its OWN process-global pooled manager"
        );
        assert!(
            !Arc::ptr_eq(&resolved, &global_manager_for(home.path(), copilot_oauth())),
            "openai-codex and github-copilot must not share a pooled manager"
        );
        assert!(
            !Arc::ptr_eq(&resolved, &global_manager_for(home.path(), claude_oauth())),
            "openai-codex and claude-pro-max must not share a pooled manager"
        );
        assert_ne!(
            session_key_for_key("openai-codex/gpt-5.5", Some(&kimi)),
            Some("kimi-tok".to_string()),
            "an openai-codex model must never receive the primary Kimi token"
        );
    }

    /// A `github-copilot/<model>` turn resolves to the process-global pooled
    /// github-copilot manager (its OWN `oauth/github-copilot` scope), NEVER the
    /// primary Kimi manager — the same leak-safe routing as xai-grok /
    /// claude-pro-max, and a DISTINCT pool entry from either.
    #[tokio::test]
    async fn github_copilot_model_resolves_to_its_own_manager_not_kimi() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved = manager_for_model(home.path(), "github-copilot/gpt-4.1", Some(&kimi))
            .expect("github-copilot model resolves to its pooled manager");
        assert!(
            !Arc::ptr_eq(&resolved, &kimi),
            "github-copilot must NOT resolve to the Kimi manager"
        );
        assert!(
            Arc::ptr_eq(&resolved, &global_manager_for(home.path(), copilot_oauth())),
            "github-copilot must resolve to its OWN process-global pooled manager"
        );
        assert!(
            !Arc::ptr_eq(&resolved, &global_manager_for(home.path(), claude_oauth())),
            "github-copilot and claude-pro-max must not share a pooled manager"
        );
        // Fail-fast: even with a Kimi primary, a copilot turn never yields the
        // Kimi bearer — it draws from the copilot pool (its own token, or None).
        assert_ne!(
            session_key_for_key("github-copilot/gpt-4.1", Some(&kimi)),
            Some("kimi-tok".to_string()),
            "a github-copilot model must never receive the primary Kimi token"
        );
    }

    /// A `claude-pro-max/<model>` turn resolves to the process-global pooled
    /// claude-pro-max manager (its OWN `oauth/claude-pro-max` scope), NEVER the
    /// primary Kimi manager — the same leak-safe routing as xai-grok, and a
    /// DISTINCT pool entry from the xai manager.
    #[tokio::test]
    async fn claude_pro_max_model_resolves_to_its_own_manager_not_kimi() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved =
            manager_for_model(home.path(), "claude-pro-max/claude-opus-4-8", Some(&kimi))
                .expect("claude-pro-max model resolves to its pooled manager");
        assert!(
            !Arc::ptr_eq(&resolved, &kimi),
            "claude-pro-max must NOT resolve to the Kimi manager"
        );
        assert!(
            Arc::ptr_eq(&resolved, &global_manager_for(home.path(), claude_oauth())),
            "claude-pro-max must resolve to its OWN process-global pooled manager"
        );
        // And it is a DIFFERENT manager than xai-grok's pooled one.
        assert!(
            !Arc::ptr_eq(&resolved, &global_manager_for(home.path(), xai_oauth())),
            "claude-pro-max and xai-grok must not share a pooled manager"
        );
    }

    /// Fail-fast (no Kimi fallback): a claude-pro-max key with a Kimi primary
    /// never yields the Kimi session token — it draws from the claude pool (its
    /// own token, or `None`), so the Kimi bearer can never reach api.anthropic.
    #[tokio::test]
    async fn session_key_for_claude_pro_max_is_never_the_kimi_primary() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        assert_ne!(
            session_key_for_key("claude-pro-max/claude-opus-4-8", Some(&kimi)),
            Some("kimi-tok".to_string()),
            "a claude-pro-max model must never receive the primary Kimi session token"
        );
    }

    /// A non-OAuth managed key (moonshot-cn/…) and an unprefixed bare id both
    /// route to the primary Kimi manager — the Kimi / first-party path is
    /// untouched and never consults the pool (no runtime needed).
    #[test]
    fn non_oauth_and_bare_models_route_to_primary() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        for key in ["moonshot-cn/kimi-k2", "kimi-k2-0905-preview"] {
            let resolved = manager_for_model(home.path(), key, Some(&kimi))
                .expect("non-oauth key routes to the primary");
            assert!(
                Arc::ptr_eq(&resolved, &kimi),
                "{key} must resolve to the primary manager"
            );
            assert_eq!(resolved.current_or_expired().unwrap().key, "kimi-tok");
        }
    }

    /// The primary being `None` (test / BYOK sessions) still yields `None` for a
    /// non-oauth key, never a panic — and without touching the pool.
    #[test]
    fn none_primary_is_passed_through_for_non_oauth() {
        let home = tempfile::tempdir().unwrap();
        assert!(manager_for_model(home.path(), "kimi-k2", None).is_none());
    }

    /// An `xai-grok/<model>` turn resolves to the process-global pooled xai
    /// manager, NEVER the primary Kimi manager — the pool is the single source.
    #[tokio::test]
    async fn grok_model_resolves_to_pooled_xai_manager_not_kimi() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved = manager_for_model(home.path(), "xai-grok/grok-4-latest", Some(&kimi))
            .expect("grok model resolves to the pooled xai manager");
        assert!(
            !Arc::ptr_eq(&resolved, &kimi),
            "grok model must NOT resolve to the Kimi manager"
        );
        assert!(
            Arc::ptr_eq(&resolved, &global_manager_for(home.path(), xai_oauth())),
            "grok model must resolve to the process-global pooled xai manager"
        );
    }

    /// Facet B guard: the resolver routes purely by the model's platform, with
    /// no auth-method input — so even when the session's primary is a Kimi
    /// (session) manager holding "kimi-tok", a grok model never resolves that
    /// Kimi token.
    #[tokio::test]
    async fn grok_model_under_kimi_primary_never_yields_kimi_token() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved = manager_for_model(home.path(), "xai-grok/grok-4-fast", Some(&kimi))
            .expect("grok model resolves to its own pooled manager regardless of primary");
        assert!(!Arc::ptr_eq(&resolved, &kimi));
        assert_ne!(
            resolved.current_or_expired().map(|a| a.key),
            Some("kimi-tok".to_string()),
            "the Kimi bearer must never be what a grok turn resolves"
        );
    }

    /// Fail-fast: a grok key resolves to the pooled xai manager (never the Kimi
    /// primary) even with no stored grok session in the pool — the pooled
    /// manager then simply holds no token, so nothing (least of all the Kimi
    /// bearer) is sent to api.x.ai.
    #[tokio::test]
    async fn grok_never_falls_back_to_kimi_primary() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        let home = tempfile::tempdir().unwrap();
        let resolved = manager_for_model(home.path(), "xai-grok/grok-4-latest", Some(&kimi))
            .expect("grok routes to the pooled xai manager, not None");
        assert!(
            !Arc::ptr_eq(&resolved, &kimi),
            "an OAuth platform must never fall back to the primary Kimi manager"
        );
    }

    /// `session_key_for_endpoint`: the endpoints that genuinely ride the
    /// PRIMARY session — `kimi-code` (the subscription channel) and a
    /// platform-less model routed at the session's own coding endpoint (a
    /// `KIGI_CODE_BASE_URL` deployment or a loopback dev proxy) — yield the
    /// primary token exactly as reading it directly would. No runtime / pool
    /// touched.
    #[test]
    fn session_key_for_the_sessions_own_endpoint_is_the_primary_token() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        assert_eq!(
            session_key_for_key("kimi-code/kimi-for-coding", Some(&kimi)),
            Some("kimi-tok".to_string()),
            "kimi-code rides the primary session, unchanged"
        );
        for url in [
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
            "http://127.0.0.1:4000/v1",
        ] {
            assert_eq!(
                session_key_for_endpoint(None, url, Some(&kimi)),
                Some("kimi-tok".to_string()),
                "{url}: a platform-less model on the session's own endpoint is unchanged"
            );
        }
    }

    /// LEAK guard (aux / summary / subagent-override `api_key` channel): an
    /// API-key registry platform, and a `[model.*]` block pointed at a
    /// third-party host, must yield NO session token. Handing them the primary
    /// stamped the user's Kimi subscription bearer onto `api.moonshot.cn` /
    /// `api.deepseek.com` as the request's `api_key` — the channel the
    /// `bearer_resolver` guard alone does not close.
    ///
    /// Revert-to-red: dropping the `platform_takes_session_credential` term
    /// from `session_key_for_endpoint` returns `Some("kimi-tok")` here.
    #[test]
    fn session_key_for_a_third_party_endpoint_is_never_the_primary_token() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        for key in [
            "moonshot-cn/kimi-k2",
            "deepseek/deepseek-chat",
            "openai/gpt-5",
        ] {
            assert_eq!(
                session_key_for_key(key, Some(&kimi)),
                None,
                "LEAK: {key} is an API-key platform — no session token may ride there"
            );
        }
        assert_eq!(
            session_key_for_endpoint(None, "https://api.openai.com/v1", Some(&kimi)),
            None,
            "LEAK: a [model.*] block on a third-party host gets no session token"
        );
    }

    /// LEAK guard (aux-model + subagent-override token routing): a grok key with
    /// a Kimi primary NEVER yields the primary Kimi token — it draws from the
    /// pooled xai manager (its own token, or `None`). This is the exact source
    /// the aux `session_key` and the override `session_key` now use.
    #[tokio::test]
    async fn session_key_for_grok_is_never_the_kimi_primary() {
        let (_kd, kimi) = primary_with_token("kimi-tok");
        assert_ne!(
            session_key_for_key("xai-grok/grok-4-latest", Some(&kimi)),
            Some("kimi-tok".to_string()),
            "a grok aux/override model must never receive the primary Kimi session token"
        );
        // Even with `None` primary the routing is unchanged: grok → pool, never a panic.
        assert_ne!(
            session_key_for_key("xai-grok/grok-4-fast", None),
            Some("kimi-tok".to_string()),
        );
    }
}
