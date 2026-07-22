//! LEAK GUARD (the chokepoint's own call sites) — H1, H2 and the H3 regression.
//!
//! The companion of `api_key_channel_leak_tests`, which drives
//! `MvpAgent::prepare_sampling_config_for_model`. These three pin the OTHER
//! producers of an outgoing credential: `ModelsManager::sampling_config()` (the
//! agent-wide baseline), the shared `MvpAgent::sampling_config` the login/seed
//! paths stamp, and the SESSION's effective coding endpoint as configured from
//! config.toml rather than the environment.
//!
//! Every assertion reads what the real resolution path produced; nothing is
//! hand-stamped.

use super::super::*;
use super::api_key_channel_leak_tests::{
    KIMI_TOKEN, kimi_session_agent, platform_entry, without_ambient_byok_env,
};
use crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID;
use crate::agent::config::{Config as AgentConfig, EndpointsConfig, ModelEntry};
use crate::auth::credential_authority::CredentialClass;
use crate::auth::{AuthManager, AuthMode, KimiAuth, KimiCodeConfig};
use kigi_sampler::BearerResolver;
use kigi_test_support::EnvGuard;

/// Re-seed the shared config the way `MvpAgent::with_models` does: the config
/// AND the platform it was BUILT from, from ONE `ModelsManager::sampling_config()`
/// against whatever `current_model_id` names right now. A test can therefore
/// never set one without the other — which is the whole point of H-a.
fn rebuild_shared_config(agent: &MvpAgent) {
    let baseline = agent.models_manager.sampling_config();
    agent.sampling_config_platform.set(baseline.platform);
    *agent.sampling_config.borrow_mut() = baseline.config;
}

/// H1 — `ModelsManager::sampling_config()`, the OTHER `api_key` producer, and a
/// byte-for-byte repeat of the round-1 defect: it resolved the session bearer
/// itself (`platform.oauth()`, else `auth_manager.current_or_expired()`), so
/// every non-OAuth platform got the primary Kimi token.
///
/// This config is not incidental — it is the `MvpAgent` baseline
/// (`Self::with_models`), which `resolve_sampling_config_for_model` returns
/// verbatim for an unresolved model id and `SubagentSpawnContext` clones as
/// every subagent's baseline, so its `api_key` reaches the wire against its own
/// `base_url`. Zero-config repro: a Kimi subscription + a bundled
/// `moonshot-cn/*` default.
///
/// Revert-to-red (L: this edit COMPILES — the previous wording named a
/// `Option<String>` argument that the `Option<&SessionCredential>` signature
/// rejects, so it could never have been run): in
/// `ModelsManager::sampling_config`, ask the authority about the SESSION's
/// endpoint instead of the current model's own —
/// `.credential_for(None, &config.endpoints.proxy_url())` in place of
/// `.credential_for_model(current_model)`. That is the round-1 defect's shape
/// (the credential decided by something other than the model's own platform +
/// endpoint) and every `assert_ne!` below sees `Some(KIMI_TOKEN)`.
#[tokio::test]
#[serial_test::serial]
async fn models_manager_sampling_config_never_carries_the_kimi_bearer() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    for (catalog_key, slug, base_url) in [
        (
            "moonshot-cn/kimi-k2-turbo-preview",
            "kimi-k2-turbo-preview",
            "https://api.moonshot.cn/v1",
        ),
        (
            "deepseek/deepseek-chat",
            "deepseek-chat",
            "https://api.deepseek.com/v1",
        ),
        ("openai/gpt-5.2", "gpt-5.2", "https://api.openai.com/v1"),
    ] {
        agent
            .models_manager
            .insert_test_entry(catalog_key, platform_entry(catalog_key, slug, base_url));
        agent
            .models_manager
            .set_current_model_id(acp::ModelId::new(catalog_key));
        let cfg = agent.models_manager.sampling_config().config;
        assert_eq!(cfg.base_url, base_url, "{catalog_key}: routed to its own host");
        assert_ne!(
            cfg.api_key.as_deref(),
            Some(KIMI_TOKEN),
            "LEAK: ModelsManager::sampling_config sent the Kimi bearer to {base_url}"
        );
    }

    // …and the first-party subscription channel is unchanged, which is what
    // proves the Kimi bearer was reachable above.
    let kimi_key = "kimi-code/kimi-for-coding";
    agent.models_manager.insert_test_entry(
        kimi_key,
        platform_entry(
            kimi_key,
            "kimi-for-coding",
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
        ),
    );
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new(kimi_key));
    assert_eq!(
        agent
            .models_manager
            .sampling_config()
            .config
            .api_key
            .as_deref(),
        Some(KIMI_TOKEN),
        "the kimi-code subscription channel must be byte-identical"
    );
}

/// H2 — the SHARED `MvpAgent::sampling_config`. `seed_client_config_auth_if_available`
/// (from `new_session` / `load_session`) and the `cached_token` / `kimi.com/oidc`
/// login handlers all stamped `sampling_config.api_key = Some(<primary bearer>)`
/// with NO platform or endpoint guard — while that very config may point at a
/// third-party model, and is both the subagent baseline and the
/// unresolved-model fallback. `agent_ops`' generic-OAuth login handler already
/// documents the correct rule ("Do NOT stamp this token onto the shared
/// sampling_config"); the Kimi handlers violated it.
///
/// Revert-to-red: make `stamp_session_credential` skip the authority entirely —
/// `self.sampling_config.borrow_mut().api_key =
/// self.auth_manager.current_or_expired().map(|a| a.key); return true;` — and
/// the third-party rows below become `Some(KIMI_TOKEN)`.
#[tokio::test]
#[serial_test::serial]
async fn shared_sampling_config_is_never_stamped_off_the_session_endpoint() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    for (catalog_key, slug, base_url) in [
        (
            "deepseek/deepseek-chat",
            "deepseek-chat",
            "https://api.deepseek.com/v1",
        ),
        (
            "moonshot-cn/kimi-k2-turbo-preview",
            "kimi-k2-turbo-preview",
            "https://api.moonshot.cn/v1",
        ),
    ] {
        agent
            .models_manager
            .insert_test_entry(catalog_key, platform_entry(catalog_key, slug, base_url));
        agent
            .models_manager
            .set_current_model_id(acp::ModelId::new(catalog_key));
        rebuild_shared_config(&agent);
        {
            let mut shared = agent.sampling_config.borrow_mut();
            assert_eq!(shared.model, slug, "{catalog_key}: built from this entry");
            assert_eq!(shared.base_url, base_url);
            shared.api_key = None;
        }
        // The `new_session` / `load_session` seed…
        agent.seed_client_config_auth_if_available();
        assert_eq!(
            agent.sampling_config.borrow().api_key, None,
            "LEAK: seeding stamped the Kimi bearer onto a config routed at {base_url}"
        );
        // …and the login handlers, which overwrite rather than seed.
        assert!(
            !agent.stamp_session_credential(true),
            "LEAK: a login handler stamped the Kimi bearer onto a config routed at {base_url}"
        );
        assert_eq!(agent.sampling_config.borrow().api_key, None);
    }

    // Byte-identical on the session's own endpoint: both paths still stamp.
    let kimi_key = "kimi-code/kimi-for-coding";
    agent.models_manager.insert_test_entry(
        kimi_key,
        platform_entry(
            kimi_key,
            "kimi-for-coding",
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
        ),
    );
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new(kimi_key));
    rebuild_shared_config(&agent);
    agent.sampling_config.borrow_mut().api_key = None;
    agent.seed_client_config_auth_if_available();
    assert_eq!(
        agent.sampling_config.borrow().api_key.as_deref(),
        Some(KIMI_TOKEN),
        "the subscription endpoint must still be seeded (this is what makes the \
         assertions above meaningful)"
    );
}

/// C1 — THE CRITICAL. The login stamp used to pair the right QUESTION (*may
/// **a** session credential ride here?*) with the wrong CREDENTIAL (always
/// `auth_manager.current_or_expired().key`, the primary Kimi bearer). For a
/// subscription-OAuth platform at its OWN host the answer is correctly "yes" —
/// the credential that may ride there is that platform's POOLED token
/// ([`CredentialClass::Pooled`]) — so a Claude Pro/Max user whose current model is
/// `claude-pro-max/*` ran `kigi login` and the Kimi subscription bearer landed
/// on a config routed at `api.anthropic.com`. From there it reaches the wire via
/// `resolve_sampling_config_for_model`'s verbatim fallback (offline / stale
/// catalog) and via `SubagentSpawnContext`'s baseline clone.
///
/// All FOUR subscription platforms, each at its own registry host, derived from
/// the registry so the fixture cannot drift.
///
/// The pooled managers are empty here (`pool_home()` is a per-process temp path
/// under `cfg(test)`), so the correct answer is `None` — and `None` is also what
/// proves the primary is not being substituted, because the same agent DOES
/// stamp `KIMI_TOKEN` on its own coding endpoint at the end of the test.
///
/// Revert-to-red (production, compiles): restore the old shape in
/// `MvpAgent::stamp_session_credential` —
/// ```ignore
/// if self.credential_authority().credential_class(platform, &base_url)
///     == CredentialClass::None
/// {
///     return false;
/// }
/// let Some(auth) = self.auth_manager.current_or_expired() else { return false };
/// self.sampling_config.borrow_mut().api_key = Some(auth.key);
/// true
/// ```
/// and every OAuth row below becomes `Some(KIMI_TOKEN)`.
#[tokio::test]
#[serial_test::serial]
async fn oauth_platform_shared_config_never_receives_the_primary_on_login() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    for (platform_id, slug) in [
        ("claude-pro-max", "claude-opus-4-8"),
        ("openai-codex", "gpt-5.5-codex"),
        ("github-copilot", "gpt-4.1"),
        ("xai-grok", "grok-4.5"),
    ] {
        let platform = kigi_models::PlatformId::parse(platform_id).expect("known platform");
        let base_url = platform.base_url();
        let catalog_key = format!("{platform_id}/{slug}");
        agent
            .models_manager
            .insert_test_entry(&catalog_key, platform_entry(&catalog_key, slug, &base_url));
        agent
            .models_manager
            .set_current_model_id(acp::ModelId::new(catalog_key.clone()));
        rebuild_shared_config(&agent);
        {
            let mut shared = agent.sampling_config.borrow_mut();
            assert_eq!(shared.model, slug, "{catalog_key}: built from this entry");
            assert_eq!(shared.base_url, base_url);
            shared.api_key = None;
        }

        // Precondition: this endpoint DOES take a session credential — that is
        // exactly why guarding a hand-carried primary with "may a session
        // credential ride?" was the bug. The class names WHICH one: the
        // platform's POOLED token, never the primary / house key.
        assert_eq!(
            agent
                .credential_authority()
                .credential_class(Some(platform), &base_url),
            CredentialClass::Pooled,
            "{catalog_key}: precondition — its own host takes its POOLED token, \
             and never the primary / house credential"
        );

        // `kigi login` (cached_token and kimi.com/oidc both land here) …
        assert!(
            !agent.stamp_session_credential(true),
            "LEAK: a Kimi login stamped a credential onto a config routed at {base_url}"
        );
        assert_eq!(
            agent.sampling_config.borrow().api_key,
            None,
            "LEAK: {catalog_key} received a bearer that is not its own pooled token"
        );
        // … and the `new_session` / `load_session` seed.
        agent.seed_client_config_auth_if_available();
        assert_ne!(
            agent.sampling_config.borrow().api_key.as_deref(),
            Some(KIMI_TOKEN),
            "LEAK: seeding sent the Kimi subscription bearer to {base_url}"
        );
    }

    // The first-party channel is untouched — this is what makes every
    // assertion above meaningful (the Kimi bearer IS live and IS stampable).
    let kimi_key = "kimi-code/kimi-for-coding";
    agent.models_manager.insert_test_entry(
        kimi_key,
        platform_entry(
            kimi_key,
            "kimi-for-coding",
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
        ),
    );
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new(kimi_key));
    rebuild_shared_config(&agent);
    agent.sampling_config.borrow_mut().api_key = None;
    assert!(agent.stamp_session_credential(true));
    assert_eq!(
        agent.sampling_config.borrow().api_key.as_deref(),
        Some(KIMI_TOKEN),
        "the Kimi subscription channel must be byte-identical"
    );
}

/// H-a (AVAILABILITY, HIGH) — the stamp guard must resolve the model the shared
/// config was BUILT from, not the one `current_model_id()` names NOW.
///
/// The shared config is built ONCE (`MvpAgent::with_models`) and never rebuilt,
/// while `current_model_id()` moves on every non-Leader model switch
/// (`handlers/model_switch.rs`) and on catalog reselection. Once they drift, a
/// guard re-resolving the config's BARE slug against the live cell fails
/// `entry_for_slug_resolution`'s `entry.info.model == slug` test and falls
/// through to `resolve_catalog_key`'s `.rev()` scan.
///
/// `kimi-code` (subscription, `uses_oauth`) and `kimi-coding` (API-key twin,
/// SAME coding host, `uses_oauth: false`) list the same routing slug, and
/// `PlatformId::ALL` puts `kimi-code` 1st and `kimi-coding` 19th — so that scan
/// answers `kimi-coding`, which takes NO session credential and therefore has NO
/// governing manager. Because `stamp_session_credential` only overwrites ON
/// SUCCESS, a Kimi-subscription user who also has `KIMI_API_KEY` set kept the
/// EXPIRED bearer in the shared config after a successful re-login: every
/// unresolved-model fallback and every subagent baseline turn 401'd until
/// restart.
///
/// The switch happens AFTER the config is built — the previous version of this
/// test left both on the same model, so it could not catch this.
///
/// Revert-to-red (production, compiles): make `MvpAgent::shared_config_platform`
/// re-resolve from the live cell instead of returning the captured value —
/// ```ignore
/// fn shared_config_platform(&self) -> Option<kigi_models::PlatformId> {
///     let model = self.sampling_config.borrow().model.clone();
///     let current_key = self.models_manager.current_model_id();
///     crate::agent::models::platform_for_slug(
///         &self.models_manager.models(),
///         Some(current_key.0.as_ref()),
///         &model,
///     )
/// }
/// ```
/// and the re-login assertion below sees the stale bearer.
#[tokio::test]
#[serial_test::serial]
async fn relogin_restamps_the_shared_config_after_the_model_switched() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();

    let coding_host = kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url;
    let slug = "kimi-for-coding";
    // Insertion order mirrors `PlatformId::ALL`: the subscription platform
    // first, its API-key twin later — so the `.rev()` scan answers the twin.
    for catalog_key in ["kimi-code/kimi-for-coding", "kimi-coding/kimi-for-coding"] {
        agent
            .models_manager
            .insert_test_entry(catalog_key, platform_entry(catalog_key, slug, coding_host));
    }
    assert_eq!(
        crate::agent::models::platform_for_slug(&agent.models_manager.models(), None, slug),
        Some(kigi_models::PlatformId::KimiCoding),
        "precondition: a `None`-keyed slug scan answers the API-key twin, which takes \
         no session credential"
    );

    // Startup: the picker selected the SUBSCRIPTION entry, and the shared config
    // was built from it (config + platform, one call).
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new("kimi-code/kimi-for-coding"));
    rebuild_shared_config(&agent);
    assert_eq!(
        agent.sampling_config.borrow().model,
        slug,
        "precondition: the shared config carries the BARE slug, which collides"
    );

    // The user switches model: `current_model_id` moves to the API-key twin
    // while the shared config still represents the subscription entry.
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new("kimi-coding/kimi-for-coding"));

    // The session expires and `kigi login` re-mints it. The handlers overwrite
    // (`stamp_session_credential(true)`) AFTER the manager holds the new token.
    agent.sampling_config.borrow_mut().api_key = Some("expired-bearer".to_string());
    assert!(
        agent.stamp_session_credential(true),
        "a successful re-login must restamp the shared config"
    );
    assert_eq!(
        agent.sampling_config.borrow().api_key.as_deref(),
        Some(KIMI_TOKEN),
        "the re-login must REPLACE the expired bearer: resolving the API-key twin \
         instead finds no governing manager, leaves the stale token in place, and \
         401s every subagent baseline turn until restart"
    );

    // And the seed path (`new_session` / `load_session`) agrees.
    agent.sampling_config.borrow_mut().api_key = None;
    agent.seed_client_config_auth_if_available();
    assert_eq!(
        agent.sampling_config.borrow().api_key.as_deref(),
        Some(KIMI_TOKEN),
    );
}

/// H3 (REGRESSION) — a MANAGED deployment configures its coding endpoint with
/// `[endpoints] coding_api_base_url` in **config.toml** (what the managed-config
/// sync writes), NOT the `KIGI_CODE_BASE_URL` env var. The previous round's
/// predicate knew only the env var, so `EndpointsConfig::proxy_url()`'s
/// config-key branch was invisible: every model inheriting the managed endpoint
/// classified third-party, lost its api_key AND its resolver, and 401'd on every
/// turn.
///
/// NOTE: no env var is set anywhere in this test — that is the point.
///
/// Revert-to-red: drop the `proxy_url()` arm from
/// `CredentialAuthority::is_session_coding_endpoint` and both `assert_eq!`s
/// below become `None`.
#[tokio::test]
#[serial_test::serial]
async fn managed_config_toml_coding_endpoint_keeps_the_session_bearer() {
    let _env = without_ambient_byok_env();
    let _no_env_override = EnvGuard::unset("KIGI_CODE_BASE_URL");
    let managed = "https://proxy.acme.example/v1";

    let dir = tempfile::tempdir().expect("tempdir");
    let auth_manager = std::sync::Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    auth_manager.hot_swap(KimiAuth {
        key: KIMI_TOKEN.to_string(),
        auth_mode: AuthMode::OAuth,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..KimiAuth::test_default()
    });
    let cfg = AgentConfig {
        endpoints: EndpointsConfig {
            coding_api_base_url: Some(managed.to_string()),
            ..EndpointsConfig::default()
        },
        ..AgentConfig::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth_manager, None)
        .expect("valid test config");
    agent.set_auth_method(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
    assert_eq!(
        agent.models_manager.endpoints().proxy_url(),
        managed,
        "precondition: the session's effective coding endpoint is the config.toml key"
    );

    // A `[model.*]` entry inheriting the managed endpoint …
    let mut bare = ModelEntry::fallback("kigi-4.5", &cfg.endpoints);
    bare.info.id = None;
    bare.info.base_url = managed.to_string();
    assert_eq!(
        agent
            .prepare_sampling_config_for_model(&bare, None)
            .api_key
            .as_deref(),
        Some(KIMI_TOKEN),
        "a managed deployment must still receive the session bearer"
    );

    // … and the kimi-code catalog entry, whose base_url IS `proxy_url()`.
    let kimi = platform_entry("kimi-code/kimi-for-coding", "kimi-for-coding", managed);
    assert_eq!(
        agent
            .prepare_sampling_config_for_model(&kimi, None)
            .api_key
            .as_deref(),
        Some(KIMI_TOKEN),
        "the managed kimi-code entry must still receive the session bearer"
    );

    // The spec's requirement was "rides AND still refreshes": an api_key alone
    // freezes at login and 401s unrecoverably ~1h in. The managed endpoint must
    // also keep a LIVE manager — the primary's, so mid-session refresh and 401
    // recovery run against the credential that actually owns that host.
    for platform in [None, Some(kigi_models::PlatformId::KimiCode)] {
        let manager = agent
            .credential_authority()
            .manager_for(platform, managed)
            .unwrap_or_else(|| {
                panic!("{platform:?}: a managed deployment must keep a live manager")
            });
        assert!(
            std::sync::Arc::ptr_eq(&manager, &agent.auth_manager),
            "{platform:?}: and it must be the session's OWN primary manager"
        );
        let resolver = agent
            .credential_authority()
            .bearer_resolver_for(platform, managed)
            .unwrap_or_else(|| panic!("{platform:?}: … exposed as a live bearer_resolver"));
        assert_eq!(
            resolver.current_bearer(),
            Some(KIMI_TOKEN.to_string()),
            "{platform:?}: the resolver reads the live primary session bearer"
        );
    }

    // The guard still holds: a third-party host under the SAME managed config
    // gets nothing — no key, and no resolver either.
    let third_party = platform_entry(
        "deepseek/deepseek-chat",
        "deepseek-chat",
        "https://api.deepseek.com/v1",
    );
    assert_eq!(
        agent
            .prepare_sampling_config_for_model(&third_party, None)
            .api_key,
        None,
        "LEAK: a managed deployment must not widen the trust set to third parties"
    );
    assert!(
        agent
            .credential_authority()
            .bearer_resolver_for(
                Some(kigi_models::PlatformId::DeepSeek),
                "https://api.deepseek.com/v1"
            )
            .is_none(),
        "LEAK: nor hand it a live resolver over the primary"
    );
}

/// M3 (COMPLETION) — the SUMMARY client's `bearer_resolver` must honour the
/// session-token gate, exactly as the session actor's aux path does.
///
/// `build_summary_client` set `cfg.bearer_resolver =
/// authority.bearer_resolver_for(platform_for_slug(…), &cfg.base_url)` with NO
/// gate while `SessionActor::aux_bearer_resolver` had one. `SamplingClient::post`
/// REPLACES the request's auth header from that resolver, so an api-key /
/// house-key session whose `[model.session-summary]` block carries its OWN
/// `env_key` on the session's own coding endpoint had that key overwritten by
/// the primary bearer on every summary request. Both now go through ONE rule,
/// `sampler_turn::aux_bearer_resolver_for`.
///
/// The summary slug is deliberately absent from the catalog and from any
/// config: it classifies `NotByok` definitively, so the only variable left is
/// the ACP auth method — which is the gate term under test.
///
/// Revert-to-red (production, compiles): in `MvpAgent::summary_bearer_resolver`,
/// return `self.credential_authority().bearer_resolver_for(platform, base_url)`
/// directly (the pre-fix shape) and the api-key row below resolves `KIMI_TOKEN`.
#[tokio::test]
#[serial_test::serial]
async fn summary_client_resolver_honours_the_session_gate() {
    let _env = without_ambient_byok_env();
    let (_dir, agent) = kimi_session_agent();
    let coding_host = kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url;
    let slug = "kigi-summary-aux-not-in-any-catalog";
    let models = agent.models_manager.models();

    // A session-based method on the session's OWN endpoint: byte-identical, the
    // summary model keeps a LIVE resolver over the primary.
    agent.set_auth_method(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
    let resolver = agent
        .summary_bearer_resolver(&models, slug, coding_host)
        .expect("the first-party subscription summary channel keeps its resolver");
    assert_eq!(
        resolver.current_bearer(),
        Some(KIMI_TOKEN.to_string()),
        "…and it reads the live primary session bearer"
    );

    // An API-KEY session: the gate is inactive, so the summary model's own key
    // must survive to the wire instead of being replaced by the primary.
    agent.set_auth_method(acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    ));
    assert!(
        agent
            .summary_bearer_resolver(&models, slug, coding_host)
            .is_none(),
        "LEAK: an api-key session's summary model had its own key replaced on the \
         wire by the primary bearer"
    );
}
