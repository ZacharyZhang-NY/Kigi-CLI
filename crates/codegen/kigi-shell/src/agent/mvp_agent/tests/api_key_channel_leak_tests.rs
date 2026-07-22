//! LEAK GUARD (`api_key` channel) — C1/C2, driven through the REAL resolution
//! path, `MvpAgent::prepare_sampling_config_for_model`.
//!
//! This is the channel the `bearer_resolver` guard does NOT close, and the one
//! the first round of leak tests assumed away by hand-stamping a provider key
//! into chat state. The chain: `session_token_for_model` used to fall through to
//! `self.auth_manager.current_or_expired()` (the primary Kimi bearer) for every
//! non-OAuth model, `resolve_credentials` then took its
//! `else if let Some(key) = session_key` arm and set `api_key = <Kimi token>`
//! with the THIRD-PARTY `base_url`, and `SamplingClient` builds
//! `Authorization: Bearer <api_key>` straight into `default_headers` — which
//! `post()` only overrides when a resolver exists, so `bearer_resolver: None`
//! does not save it.
//!
//! Nothing here stamps a credential by hand: every assertion reads what the
//! resolution path actually produced.

use super::super::*;
use crate::agent::auth_method::{
    CACHED_TOKEN_AUTH_METHOD_ID, HOUSE_API_KEY_ENV_VAR, LEGACY_XAI_API_KEY_ENV_VAR,
    XAI_API_KEY_ENV_VAR,
};
use crate::agent::config::{Config as AgentConfig, EndpointsConfig, EnvKeys, ModelEntry};
use crate::auth::{AuthManager, AuthMode, KimiAuth, KimiCodeConfig};
use kigi_test_support::EnvGuard;

pub(super) const KIMI_TOKEN: &str = "kimi-subscription-token-DO-NOT-LEAK";

/// Ambient BYOK env vars unset, so a model with no resolvable credential ends up
/// with `api_key == None` rather than a global-key fallback that could mask the
/// leak under test. Every test holding these must be `#[serial]`.
pub(super) fn without_ambient_byok_env() -> [EnvGuard; 3] {
    [
        EnvGuard::unset(HOUSE_API_KEY_ENV_VAR),
        EnvGuard::unset(XAI_API_KEY_ENV_VAR),
        EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR),
    ]
}

/// An `MvpAgent` on a session-based (`cached_token`) ACP method holding a live
/// Kimi subscription bearer — the mainstream configuration in which the leak
/// fires. `(tempdir, agent)`; the tempdir is the auth store and is returned so
/// the caller keeps it alive.
pub(super) fn kimi_session_agent() -> (tempfile::TempDir, MvpAgent) {
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_manager = std::sync::Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    auth_manager.hot_swap(KimiAuth {
        key: KIMI_TOKEN.to_string(),
        auth_mode: AuthMode::OAuth,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..KimiAuth::test_default()
    });
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = MvpAgent::new(
        GatewaySender::new(tx),
        &AgentConfig::default(),
        auth_manager,
        None,
    )
    .expect("valid test config");
    agent.set_auth_method(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
    (dir, agent)
}

/// A catalog entry as `resolve_model_list` builds one for a fetched registry
/// model: managed catalog key, platform base URL, no credential of its own.
pub(super) fn platform_entry(catalog_key: &str, slug: &str, base_url: &str) -> ModelEntry {
    let mut entry = ModelEntry::fallback(slug, &EndpointsConfig::default());
    entry.info.id = Some(catalog_key.to_string());
    entry.info.base_url = base_url.to_string();
    entry
}

/// C1, the ZERO-CONFIGURATION repro. `default_models.json` bundles
/// `moonshot-cn/*` and `moonshot-ai/*` entries with `api_key: None`, and
/// `resolve_model_list` keeps the bundled defaults whenever no catalog fetch has
/// succeeded — so on first launch / offline a Kimi-subscription user sees them
/// in the picker with no configuration whatsoever. Selecting one used to send
/// `Authorization: Bearer <Kimi OAuth token>` to `api.moonshot.cn`, which is NOT
/// first-party.
///
/// Revert-to-red: make `CredentialAuthority::governing_manager`'s
/// `Some(platform) => None` arm return `self.primary.clone()` and every `api_key`
/// below becomes `Some(KIMI_TOKEN)`.
#[tokio::test]
#[serial_test::serial]
async fn bundled_default_moonshot_models_never_carry_the_kimi_bearer() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    let bundled = crate::agent::config::default_model_entries(&EndpointsConfig::default());
    let moonshot: Vec<_> = bundled
        .iter()
        .filter(|(key, _)| key.starts_with("moonshot-cn/") || key.starts_with("moonshot-ai/"))
        .collect();
    assert_eq!(
        moonshot.len(),
        4,
        "default_models.json still bundles the four moonshot open-platform entries"
    );

    for (key, entry) in moonshot {
        assert!(
            !entry.has_own_credentials(),
            "{key}: the bundled entry carries no credential of its own"
        );
        assert!(
            !crate::util::is_effective_coding_endpoint_url(&entry.info().base_url),
            "{key}: routes to a third-party host ({})",
            entry.info().base_url
        );
        let cfg = agent.prepare_sampling_config_for_model(entry, None);
        assert_ne!(
            cfg.api_key.as_deref(),
            Some(KIMI_TOKEN),
            "LEAK: selecting the bundled {key} sent the Kimi subscription bearer to {}",
            entry.info().base_url
        );
        assert_eq!(
            cfg.base_url,
            entry.info().base_url,
            "{key}: still routes to its own host (the fix must not reroute traffic)"
        );
    }
}

/// C1 across the API-key registry platform shapes a fetched catalog produces.
#[tokio::test]
#[serial_test::serial]
async fn api_key_platform_models_never_carry_the_kimi_bearer_as_api_key() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    for (catalog_key, slug, base_url) in [
        ("deepseek/deepseek-chat", "deepseek-chat", "https://api.deepseek.com/v1"),
        ("openai/gpt-5.2", "gpt-5.2", "https://api.openai.com/v1"),
        ("anthropic/claude-opus-4-8", "claude-opus-4-8", "https://api.anthropic.com/v1"),
        ("groq/llama-4", "llama-4", "https://api.groq.com/openai/v1"),
        ("xai/grok-4.5", "grok-4.5", "https://api.x.ai/v1"),
    ] {
        let entry = platform_entry(catalog_key, slug, base_url);
        let cfg = agent.prepare_sampling_config_for_model(&entry, None);
        assert_ne!(
            cfg.api_key.as_deref(),
            Some(KIMI_TOKEN),
            "LEAK: {catalog_key} carried the primary Kimi session bearer to {base_url}"
        );
    }
}

/// C2, the `[model.*]` repro. A `[model.gpt-4o]` block has `info.id == None`, so
/// it has no platform at all — which used to be a blanket allow. BYOK is
/// `has_own_credentials()`, which probes `std::env::var` AT CALL TIME, so an
/// unset (or mistyped) `env_key` classifies the model NotByok and the Kimi
/// bearer went to `api.openai.com` on BOTH channels.
///
/// Revert-to-red: make `CredentialAuthority::is_session_coding_endpoint` return
/// `true` unconditionally and `api_key` here becomes `Some(KIMI_TOKEN)`.
#[tokio::test]
#[serial_test::serial]
async fn config_model_with_an_unset_env_key_never_carries_the_kimi_bearer() {
    let _env = without_ambient_byok_env();
    let _typo = EnvGuard::unset("OPENAI_API_KEY_TYPO");
    let (_dir, agent) = kimi_session_agent();

    let mut entry = ModelEntry::fallback("gpt-4o", &EndpointsConfig::default());
    entry.info.id = None; // a `[model.gpt-4o]` config block
    entry.info.base_url = "https://api.openai.com/v1".to_string();
    entry.env_key = Some(EnvKeys::single("OPENAI_API_KEY_TYPO"));
    assert!(
        !entry.has_own_credentials(),
        "the env var is unset, so this classifies NotByok — the precondition of the defect"
    );

    let cfg = agent.prepare_sampling_config_for_model(&entry, None);
    assert_ne!(
        cfg.api_key.as_deref(),
        Some(KIMI_TOKEN),
        "LEAK: a [model.*] block with an unset env_key sent the Kimi bearer to api.openai.com"
    );
    assert_eq!(cfg.api_key, None, "no credential resolves — fail fast");
}

/// The first-party subscription channel must stay BYTE-IDENTICAL: `kimi-code/*`
/// (and a `[model.*]` block on the session's own coding endpoint, including a
/// `KIGI_CODE_BASE_URL` deployment / a local dev proxy) still carries the
/// primary session key. This assertion is also what proves the Kimi token is
/// live in the tests above — it WOULD leak if the guard were missing.
#[tokio::test]
#[serial_test::serial]
async fn the_first_party_subscription_channel_still_carries_the_session_key() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    let kimi = platform_entry(
        "kimi-code/kimi-for-coding",
        "kimi-for-coding",
        kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
    );
    assert_eq!(
        agent
            .prepare_sampling_config_for_model(&kimi, None)
            .api_key
            .as_deref(),
        Some(KIMI_TOKEN),
        "the kimi-code subscription channel must be unchanged"
    );

    for base_url in [
        kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
        "http://127.0.0.1:4141/v1",
        "http://localhost:8080/v1",
    ] {
        let mut bare = ModelEntry::fallback("kigi-4.5", &EndpointsConfig::default());
        bare.info.id = None;
        bare.info.base_url = base_url.to_string();
        assert_eq!(
            agent
                .prepare_sampling_config_for_model(&bare, None)
                .api_key
                .as_deref(),
            Some(KIMI_TOKEN),
            "{base_url}: a custom deployment / local proxy keeps the session key"
        );
    }
}

/// A subscription-OAuth model draws its `api_key` from ITS OWN pooled manager,
/// never the Kimi primary — and never falls back to it when that provider has no
/// stored session (the pool home is an empty TempDir under `cfg(test)`).
#[tokio::test]
#[serial_test::serial]
async fn oauth_platform_models_never_carry_the_kimi_bearer_as_api_key() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    for (catalog_key, slug, base_url) in [
        ("xai-grok/grok-4-latest", "grok-4-latest", "https://api.x.ai/v1"),
        (
            "claude-pro-max/claude-opus-4-8",
            "claude-opus-4-8",
            "https://api.anthropic.com/v1",
        ),
        ("github-copilot/gpt-4.1", "gpt-4.1", "https://api.githubcopilot.com"),
        (
            "openai-codex/gpt-5.5",
            "gpt-5.5",
            "https://chatgpt.com/backend-api/codex",
        ),
    ] {
        let entry = platform_entry(catalog_key, slug, base_url);
        let cfg = agent.prepare_sampling_config_for_model(&entry, None);
        assert_ne!(
            cfg.api_key.as_deref(),
            Some(KIMI_TOKEN),
            "LEAK: {catalog_key} carried the primary Kimi session bearer to {base_url}"
        );
    }
}

/// H5 at the api_key channel: `resolve_model_id` (the picker's own lookup) must
/// hand `prepare_sampling_config_for_model` the entry the user SELECTED, even
/// when an API-key platform and its subscription-OAuth twin list the same
/// routing slug in `PlatformId::ALL` order. Selecting the OAuth twin by catalog
/// key must not resolve the API-key twin — and vice versa.
#[tokio::test]
#[serial_test::serial]
async fn dual_credential_slug_collision_resolves_the_selected_catalog_key() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    // API-key platform FIRST, exactly as `PlatformId::ALL` orders them.
    for key in ["xai/grok-4.5", "xai-grok/grok-4.5"] {
        agent.models_manager.insert_test_entry(
            key,
            platform_entry(key, "grok-4.5", "https://api.x.ai/v1"),
        );
    }

    for key in ["xai/grok-4.5", "xai-grok/grok-4.5"] {
        let resolved = agent
            .resolve_model_id(&acp::ModelId::new(key))
            .expect("both twins resolve");
        assert_eq!(
            resolved.info().id.as_deref(),
            Some(key),
            "selecting {key} must resolve THAT catalog entry, not its slug twin"
        );
    }

    // And the bare slug resolves the same entry the picker's `resolve_catalog_key`
    // does — one direction, one answer (the auth layer used to first-match).
    let by_slug = agent
        .resolve_model_id(&acp::ModelId::new("grok-4.5"))
        .expect("the bare slug resolves");
    let models = agent.models_manager.models();
    let picker_key = crate::agent::models::resolve_catalog_key(
        &models,
        &acp::ModelId::new("grok-4.5"),
    )
    .expect("the picker resolves the bare slug");
    assert_eq!(
        by_slug.info().id.as_deref(),
        Some(picker_key.0.as_ref()),
        "the auth layer and the picker must resolve the SAME entry for one slug"
    );
    assert_eq!(
        crate::agent::config::find_model_by_id(&models, "grok-4.5")
            .and_then(|e| e.info().id.as_deref()),
        Some(picker_key.0.as_ref()),
        "find_model_by_id must agree with resolve_catalog_key by construction"
    );
}
