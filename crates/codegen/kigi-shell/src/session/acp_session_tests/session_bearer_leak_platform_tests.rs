//! LEAK GUARD, part 2: the model→platform lookup (H5) and the AUX resolver
//! decision (H3/H4). Shares the fixtures in
//! [`super::session_bearer_leak_tests`]; see that module's header for the chain
//! and the storage-discipline contract.

use super::session_bearer_leak_tests::{
    KIMI_TOKEN, actor_on_managed_model, actor_with_catalog, managed_entry,
};
use super::*;
use kigi_sampler::BearerResolver;
use std::sync::Arc;

#[tokio::test(flavor = "current_thread")]
async fn dual_credential_slug_collision_resolves_the_selected_oauth_platform() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // (api-key twin, oauth twin, shared slug, host)
            for (api_key_twin, oauth_twin, slug, base_url) in [
                (
                    "xai/grok-4.5",
                    "xai-grok/grok-4.5",
                    "grok-4.5",
                    "https://api.x.ai/v1",
                ),
                (
                    "anthropic/claude-opus-4-8",
                    "claude-pro-max/claude-opus-4-8",
                    "claude-opus-4-8",
                    "https://api.anthropic.com/v1",
                ),
                (
                    "openai/gpt-5.5-codex",
                    "openai-codex/gpt-5.5-codex",
                    "gpt-5.5-codex",
                    "https://chatgpt.com/backend-api/codex",
                ),
            ] {
                // API-key platform FIRST, exactly as `PlatformId::ALL` orders them.
                let catalog = vec![
                    managed_entry(api_key_twin, slug, base_url),
                    managed_entry(oauth_twin, slug, base_url),
                ];
                let (_dir, actor, _rx) = actor_with_catalog(catalog, oauth_twin, "unused").await;
                let cfg = actor.reconstruct_full_config().await;

                let resolver = cfg.bearer_resolver.as_ref().unwrap_or_else(|| {
                    panic!(
                        "{oauth_twin}: selecting the OAuth twin must keep a LIVE bearer_resolver \
                         (mid-session refresh); resolving {api_key_twin} instead drops it"
                    )
                });
                assert_ne!(
                    resolver.current_bearer(),
                    Some(KIMI_TOKEN.to_string()),
                    "{oauth_twin}: the resolver must read its OWN pool, never the Kimi primary"
                );

                let platform = kigi_models::parse_managed_model_key(oauth_twin)
                    .expect("managed key")
                    .0;
                assert_eq!(
                    cfg.anthropic_oauth,
                    platform.wire_api() == kigi_models::PlatformWireApi::Messages,
                    "{oauth_twin}: the Claude OAuth Messages adaptation must follow the \
                     SELECTED platform"
                );
                assert_eq!(
                    cfg.openai_codex,
                    platform.sends_codex_responses_headers(),
                    "{oauth_twin}: the Codex identity headers must follow the SELECTED platform"
                );
                assert_eq!(
                    cfg.github_copilot,
                    platform.sends_copilot_editor_headers(),
                    "{oauth_twin}: the Copilot editor headers must follow the SELECTED platform"
                );
            }
        })
        .await;
}

/// The other half of H5: selecting the API-KEY twin of a colliding slug must
/// still resolve the API-key platform — no bearer_resolver, no adaptations. The
/// unified lookup must not simply prefer OAuth.
#[tokio::test(flavor = "current_thread")]
async fn dual_credential_slug_collision_resolves_the_selected_api_key_platform() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let catalog = vec![
                managed_entry(
                    "anthropic/claude-opus-4-8",
                    "claude-opus-4-8",
                    "https://api.anthropic.com/v1",
                ),
                managed_entry(
                    "claude-pro-max/claude-opus-4-8",
                    "claude-opus-4-8",
                    "https://api.anthropic.com/v1",
                ),
            ];
            let (_dir, actor, _rx) =
                actor_with_catalog(catalog, "anthropic/claude-opus-4-8", "sk-ant-byok").await;
            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "selecting the API-key twin must get NO session bearer resolver"
            );
            assert!(
                !cfg.anthropic_oauth,
                "the API-key Anthropic Messages request must stay byte-identical"
            );
            assert_eq!(cfg.api_key.as_deref(), Some("sk-ant-byok"));
        })
        .await;
}

/// MANDATORY counterpart: the subscription-OAuth platforms have NON-first-party
/// base URLs, so the fix must not disable their resolver. Each must still get a
/// LIVE `bearer_resolver` — and it must read THAT platform's own pooled
/// `AuthManager`, never the Kimi primary. The pooled managers are empty here (a
/// TempDir pool home), which is what makes `current_bearer() == None` a proof
/// that the Kimi bearer cannot be what they resolve.
#[tokio::test(flavor = "current_thread")]
async fn oauth_platform_models_keep_a_live_resolver_from_their_own_pool() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (catalog_key, slug, base_url) in [
                (
                    "claude-pro-max/claude-opus-4-8",
                    "claude-opus-4-8",
                    "https://api.anthropic.com/v1",
                ),
                (
                    "github-copilot/gpt-4.1",
                    "gpt-4.1",
                    "https://api.githubcopilot.com",
                ),
                (
                    "xai-grok/grok-4-latest",
                    "grok-4-latest",
                    "https://api.x.ai/v1",
                ),
                (
                    "openai-codex/gpt-5.5",
                    "gpt-5.5",
                    "https://chatgpt.com/backend-api/codex",
                ),
            ] {
                let (_dir, actor, _rx) =
                    actor_on_managed_model(catalog_key, slug, base_url, "unused").await;
                let cfg = actor.reconstruct_full_config().await;
                let resolver = cfg.bearer_resolver.as_ref().unwrap_or_else(|| {
                    panic!("{catalog_key}: must keep a live bearer_resolver for refresh")
                });
                assert_ne!(
                    resolver.current_bearer(),
                    Some(KIMI_TOKEN.to_string()),
                    "{catalog_key}: the Kimi bearer must never be what it resolves"
                );

                // The resolver is LIVE over that platform's pooled manager: a
                // token rotated inside the pool is observed by the
                // already-built resolver (this is what mid-session refresh
                // does). The pool is read here, never mutated.
                let pooled = crate::auth::oauth_registry::manager_for_model(
                    &crate::auth::oauth_registry::pool_home(),
                    catalog_key,
                    actor.auth_manager.as_ref(),
                )
                .expect("an OAuth platform always resolves a manager");
                assert!(
                    !Arc::ptr_eq(
                        &pooled,
                        actor.auth_manager.as_ref().expect("primary is present")
                    ),
                    "{catalog_key}: must route to its OWN pooled manager, not the Kimi primary"
                );
                assert_eq!(
                    resolver.current_bearer(),
                    pooled.current_or_expired().map(|a| a.key),
                    "{catalog_key}: the resolver must read THIS platform's pooled manager"
                );
            }
        })
        .await;
}

/// H3/H4 — the stamped AUX paths (image-describe, the auto-mode classifier and
/// the session-summary client all funnel through `aux_bearer_resolver`).
/// `stamp_session_local_sampler_fields` copies the SESSION model's resolver onto
/// every aux config and `SamplingClient::post` REPLACES the request's auth
/// header from it, so an API-key-platform aux model would have its own key
/// overwritten by the Kimi bearer ON THE AUX HOST.
///
/// Revert-to-red: returning `stamped` unconditionally (the pre-fix
/// "re-point only when the aux model is OAuth" shape) makes the deepseek /
/// openai / `[model.*]`-on-openai.com rows resolve `KIMI_TOKEN`.
#[test]
fn aux_bearer_resolver_clears_the_session_resolver_off_a_third_party_aux_host() {
    #[derive(Debug)]
    struct Fixed(&'static str);
    impl BearerResolver for Fixed {
        fn current_bearer(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }
    let stamped: kigi_sampler::SharedBearerResolver = Arc::new(Fixed(KIMI_TOKEN));
    let platform =
        |key: &str| kigi_models::parse_managed_model_key(key).map(|(platform, _)| platform);

    // Cleared: every API-key registry platform, and a `[model.*]` aux model
    // pointed at a third-party host.
    for (key, base_url) in [
        ("deepseek/deepseek-chat", "https://api.deepseek.com/v1"),
        ("openai/gpt-5-mini", "https://api.openai.com/v1"),
        (
            "moonshot-cn/kimi-k2-turbo-preview",
            "https://api.moonshot.cn/v1",
        ),
    ] {
        assert!(
            crate::session::acp_session::sampler_turn::aux_bearer_resolver(
                Some(stamped.clone()),
                platform(key),
                base_url,
            )
            .is_none(),
            "LEAK: an aux model on {base_url} must not inherit the session bearer resolver"
        );
    }
    assert!(
        crate::session::acp_session::sampler_turn::aux_bearer_resolver(
            Some(stamped.clone()),
            None,
            "https://api.openai.com/v1",
        )
        .is_none(),
        "LEAK: a [model.*] aux model on a third-party host must not inherit it either"
    );

    // Kept (byte-identical): the first-party subscription channel and a
    // platform-less aux model on the session's own endpoint.
    for (key, base_url) in [
        (
            Some("kimi-code/kimi-for-coding"),
            kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
        ),
        (None, kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url),
        (None, "http://127.0.0.1:4141/v1"),
    ] {
        let resolved = crate::session::acp_session::sampler_turn::aux_bearer_resolver(
            Some(stamped.clone()),
            key.and_then(platform),
            base_url,
        )
        .unwrap_or_else(|| panic!("{key:?} @ {base_url} must keep the session resolver"));
        assert_eq!(resolved.current_bearer(), Some(KIMI_TOKEN.to_string()));
    }

    // Re-pointed: an OAuth aux model gets a LIVE resolver over its OWN pool
    // (empty here), never the stamped Kimi one.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the pooled manager's refresh task");
    rt.block_on(async {
        for key in [
            "xai-grok/grok-4-latest",
            "claude-pro-max/claude-opus-4-8",
            "github-copilot/gpt-4.1",
            "openai-codex/gpt-5.5",
        ] {
            let resolved = crate::session::acp_session::sampler_turn::aux_bearer_resolver(
                Some(stamped.clone()),
                platform(key),
                "https://example.invalid/v1",
            )
            .unwrap_or_else(|| panic!("{key} must keep a live resolver from its own pool"));
            assert_ne!(
                resolved.current_bearer(),
                Some(KIMI_TOKEN.to_string()),
                "{key}: the aux resolver must never resolve the Kimi session bearer"
            );
        }
    });
}
