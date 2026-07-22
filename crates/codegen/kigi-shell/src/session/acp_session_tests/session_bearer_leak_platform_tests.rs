//! LEAK GUARD, part 2: the model→platform lookup (H5) and the AUX resolver
//! decision (H3/H4). Shares the fixtures in
//! [`super::session_bearer_leak_tests`]; see that module's header for the chain
//! and the storage-discipline contract.

use super::session_bearer_leak_tests::{
    KIMI_TOKEN, actor_on_managed_model, actor_with_catalog, managed_entry,
};
use super::*;
use kigi_sampler::BearerResolver;
use kigi_test_support::EnvGuard;
use std::sync::Arc;

/// The host BOTH halves of the `anthropic` / `claude-pro-max` collision route
/// to, derived from the registry (as the sibling at
/// [`oauth_platform_models_keep_a_live_resolver_from_their_own_pool`] does) so
/// the fixture cannot drift, with the twin agreement asserted rather than
/// assumed — the collision is only a collision because both platforms serve the
/// same host.
fn anthropic_collision_host() -> String {
    let oauth_host = kigi_models::PlatformId::ClaudeProMax.base_url();
    assert_eq!(
        kigi_models::PlatformId::Anthropic.base_url(),
        oauth_host,
        "the API-key platform and its subscription-OAuth twin must serve the same host, \
         or this fixture is not testing the dual-credential collision"
    );
    oauth_host
}

/// Ambient BYOK env unset. `resolve_model_auth_facts` probes `std::env::var` at
/// call time, so a developer (or CI) holding `ANTHROPIC_API_KEY` flips the
/// fixture to `Byok` and switches off the session-token gate for a reason that
/// has nothing to do with the platform lookup under test. Every holder must be
/// `#[serial]`.
fn anthropic_collision_env_guard() -> [EnvGuard; 2] {
    [
        EnvGuard::unset("ANTHROPIC_API_KEY"),
        EnvGuard::unset("KIGI_CODE_BASE_URL"),
    ]
}

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
            // The base URL is the platform's OWN registry host, exactly as
            // `models_fetch::platform_fetch_base` builds every fetched entry —
            // derived here rather than hard-coded so the fixture cannot drift
            // from the registry (L10 compares against precisely this).
            for (catalog_key, slug) in [
                ("claude-pro-max/claude-opus-4-8", "claude-opus-4-8"),
                ("github-copilot/gpt-4.1", "gpt-4.1"),
                ("xai-grok/grok-4-latest", "grok-4-latest"),
                ("openai-codex/gpt-5.5", "gpt-5.5"),
            ] {
                let base_url = kigi_models::parse_managed_model_key(catalog_key)
                    .expect("managed key")
                    .0
                    .base_url();
                let base_url = base_url.as_str();
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
                let pooled = actor
                    .credential_authority()
                    .manager_for(
                        kigi_models::parse_managed_model_key(catalog_key).map(|(p, _)| p),
                        base_url,
                    )
                    .expect("an OAuth platform on its own host always resolves a manager");
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
/// the session-summary client all funnel through
/// `CredentialAuthority::bearer_resolver_for`). `SamplingClient::post` REPLACES
/// the request's auth header from the resolver, so an API-key-platform aux model
/// would have its own key overwritten by the Kimi bearer ON THE AUX HOST.
///
/// Revert-to-red: make `CredentialAuthority::governing_manager`'s
/// `Some(platform) => None` arm return `self.primary.clone()` and the deepseek /
/// openai / moonshot rows resolve `KIMI_TOKEN`.
#[test]
fn aux_bearer_resolver_clears_the_session_resolver_off_a_third_party_aux_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primary = Arc::new(crate::auth::AuthManager::new(
        dir.path(),
        crate::auth::KimiCodeConfig::default(),
    ));
    primary.hot_swap(crate::auth::KimiAuth {
        key: KIMI_TOKEN.to_string(),
        auth_mode: crate::auth::AuthMode::OAuth,
        ..crate::auth::KimiAuth::test_default()
    });
    let authority = crate::auth::credential_authority::CredentialAuthority::new(
        crate::agent::config::EndpointsConfig::default(),
        Some(primary),
    );
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
            authority
                .bearer_resolver_for(platform(key), base_url)
                .is_none(),
            "LEAK: an aux model on {base_url} must not inherit the session bearer resolver"
        );
    }
    assert!(
        authority
            .bearer_resolver_for(None, "https://api.openai.com/v1")
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
        let resolved = authority
            .bearer_resolver_for(key.and_then(platform), base_url)
            .unwrap_or_else(|| panic!("{key:?} @ {base_url} must keep the session resolver"));
        assert_eq!(resolved.current_bearer(), Some(KIMI_TOKEN.to_string()));
    }

    // Re-pointed: an OAuth aux model on ITS OWN host gets a LIVE resolver over
    // its own pool (empty here), never the Kimi primary. L10: the same model
    // redirected to a third-party host gets NOTHING.
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
            let p = platform(key).expect("managed key");
            let resolved = authority
                .bearer_resolver_for(Some(p), &p.base_url())
                .unwrap_or_else(|| panic!("{key} must keep a live resolver from its own pool"));
            assert_ne!(
                resolved.current_bearer(),
                Some(KIMI_TOKEN.to_string()),
                "{key}: the aux resolver must never resolve the Kimi session bearer"
            );
            assert!(
                authority
                    .bearer_resolver_for(Some(p), "https://example.invalid/v1")
                    .is_none(),
                "LEAK ({key}): an OAuth aux model redirected off its own host gets nothing"
            );
        }
    });
}

/// H4 — TWO CONCURRENT SESSIONS on a colliding slug. The model→platform lookup
/// used to key on `ModelsManager::current_model_id()`, a single PROCESS-GLOBAL
/// `RwLock<acp::ModelId>` written by whichever session switched last. With one
/// session on `xai-grok/grok-4.5` and another on `xai/grok-4.5` — same routing
/// slug, by design — the loser resolved the OTHER session's platform:
/// the subscription session lost its live resolver (unrecoverable 401 ~1h in)
/// and the API-key session got the pooled OAuth bearer stamped over its own
/// `sk-…` key, which the provider rejects.
///
/// Here the global cell is deliberately set to the API-key twin for BOTH
/// sessions (last writer wins, and it was the API-key one). Each session must
/// still resolve ITS OWN selection.
///
/// Revert-to-red: replace `self.selected_catalog_key()` in
/// `SessionActor::model_platform` with
/// `Some(self.models_manager.current_model_id().0.as_ref())`. Under THIS
/// fixture both of the subscription session's assertions fail. (L: the fixture
/// is what makes that true — `managed_entry` carries `api_key: None` and
/// `actor_with_catalog` pins `NotByok`, which is exactly what a FETCHED registry
/// entry resolves to. A user who additionally sets `ANTHROPIC_API_KEY` /
/// `[model.*] env_key` classifies `Byok`, the gate is inactive for that reason
/// alone, and only the `anthropic_oauth` assertion would still catch the
/// mis-resolution — hence the env guard below.)
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn concurrent_sessions_on_a_colliding_slug_each_resolve_their_own_platform() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let api_key_twin = "anthropic/claude-opus-4-8";
            let oauth_twin = "claude-pro-max/claude-opus-4-8";
            let slug = "claude-opus-4-8";
            let host = &anthropic_collision_host();
            let catalog = || {
                vec![
                    // API-key platform FIRST, exactly as `PlatformId::ALL` orders them.
                    managed_entry(api_key_twin, slug, host),
                    managed_entry(oauth_twin, slug, host),
                ]
            };

            let (_d1, subscription, _r1) =
                actor_with_catalog(catalog(), oauth_twin, "unused").await;
            let (_d2, api_key, _r2) =
                actor_with_catalog(catalog(), api_key_twin, "sk-ant-user").await;
            // The other session switched last: the process-global cell now names
            // the API-key twin for BOTH.
            for actor in [&subscription, &api_key] {
                actor
                    .models_manager
                    .set_current_model_id(acp::ModelId::new(api_key_twin.to_string()));
            }

            let sub_cfg = subscription.reconstruct_full_config().await;
            let resolver = sub_cfg.bearer_resolver.as_ref().expect(
                "the subscription session must keep its own live bearer_resolver even when \
                 another session switched the process-global model last",
            );
            assert_ne!(
                resolver.current_bearer(),
                Some(KIMI_TOKEN.to_string()),
                "it must read the claude-pro-max pool, never the Kimi primary"
            );
            assert!(
                sub_cfg.anthropic_oauth,
                "the Claude OAuth Messages adaptation must follow the SUBSCRIPTION session"
            );

            let api_cfg = api_key.reconstruct_full_config().await;
            assert!(
                api_cfg.bearer_resolver.is_none(),
                "LEAK: the API-key session must get no session bearer_resolver"
            );
            assert!(
                !api_cfg.anthropic_oauth,
                "the API-key session must not get the OAuth Messages adaptation"
            );
            assert_eq!(
                api_cfg.api_key.as_deref(),
                Some("sk-ant-user"),
                "the API-key session keeps its own provider key"
            );
        })
        .await;
}

/// H4 in LEADER mode, where `agent/handlers/model_switch.rs` skips
/// `set_current_model_id` ENTIRELY, so the process-global cell is frozen at the
/// startup default for the whole process lifetime. `platform_for_slug` then
/// fell through to the `.rev()` scan, which returns the LAST match — the OAuth
/// twin — so a Leader-mode session on the API-KEY twin was handed the pooled
/// OAuth bearer plus the Messages adaptation, and Anthropic rejects both.
///
/// Revert-to-red: same edit as above; with the global cell naming the startup
/// default (not in this catalog) the `.rev()` fallback resolves
/// `claude-pro-max/*` and both assertions fail.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn leader_mode_session_resolves_its_own_platform_without_the_global_cell() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let slug = "claude-opus-4-8";
            let host = &anthropic_collision_host();
            let (_dir, actor, _rx) = actor_with_catalog(
                vec![
                    managed_entry("anthropic/claude-opus-4-8", slug, host),
                    managed_entry("claude-pro-max/claude-opus-4-8", slug, host),
                ],
                "anthropic/claude-opus-4-8",
                "sk-ant-user",
            )
            .await;
            // Leader mode never writes the global cell: it still names the
            // startup default, which is not in this catalog at all.
            assert!(
                !actor
                    .models_manager
                    .models()
                    .contains_key(actor.models_manager.current_model_id().0.as_ref()),
                "precondition: the process-global model id is stale (Leader mode)"
            );

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "LEAK: a Leader-mode API-key session must get no session bearer_resolver"
            );
            assert!(
                !cfg.anthropic_oauth,
                "a Leader-mode API-key session must not get the OAuth Messages adaptation"
            );
        })
        .await;
}

/// L13 — a user whose ACP auth method is an API-KEY registry platform (e.g.
/// `deepseek`) can still SELECT a subscription-OAuth model, and the chokepoint
/// hands it that platform's pooled bearer as the request's `api_key`. The gate
/// keys on the primary method, which is not session-based, so the config used
/// to carry NO `bearer_resolver`: the pooled token froze at selection time and
/// the session died with an unrecoverable 401 once it expired (~1h).
///
/// The model's own credential now makes the gate session-based, confined to
/// that platform's own host by the gate's `credential_class` conjunct.
#[tokio::test(flavor = "current_thread")]
async fn oauth_model_under_an_api_key_auth_method_keeps_its_pooled_resolver() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let base_url = kigi_models::PlatformId::ClaudeProMax.base_url();
            let (_dir, actor, _rx) = actor_with_catalog(
                vec![managed_entry(
                    "claude-pro-max/claude-opus-4-8",
                    "claude-opus-4-8",
                    &base_url,
                )],
                "claude-pro-max/claude-opus-4-8",
                "unused",
            )
            .await;
            // The PRIMARY ACP method is an API-key registry platform login.
            actor
                .auth_method_id
                .store(Some(std::sync::Arc::new(acp::AuthMethodId::new(
                    "deepseek",
                ))));

            let cfg = actor.reconstruct_full_config().await;
            let resolver = cfg.bearer_resolver.as_ref().expect(
                "a subscription-OAuth model keeps a live resolver whatever the primary \
                 ACP auth method is, or it cannot refresh mid-session",
            );
            assert_ne!(
                resolver.current_bearer(),
                Some(KIMI_TOKEN.to_string()),
                "and it reads the claude-pro-max pool, never the primary"
            );

            // An API-key-platform model under the same method stays resolver-free.
            let (_d2, deepseek, _r2) = actor_with_catalog(
                vec![managed_entry(
                    "deepseek/deepseek-chat",
                    "deepseek-chat",
                    "https://api.deepseek.com/v1",
                )],
                "deepseek/deepseek-chat",
                "sk-deepseek",
            )
            .await;
            deepseek
                .auth_method_id
                .store(Some(std::sync::Arc::new(acp::AuthMethodId::new(
                    "deepseek",
                ))));
            assert!(
                deepseek
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_none(),
                "LEAK: an API-key-platform model must never get a session resolver"
            );
        })
        .await;
}

/// H-b — a `None` or STALE per-session catalog key must REFUSE, not degrade to
/// the subscription-OAuth twin.
///
/// `model_platform` falls through to `resolve_catalog_key`'s `.rev()` scan when
/// the session's own key does not name the slug, and that scan returns the LAST
/// match — the OAuth twin, because `PlatformId::ALL` orders every API-key
/// platform first. Combined with the L13 disjunct (a model whose own credential
/// is a pooled OAuth session is session-based BY ITSELF), an API-KEY session on
/// `anthropic/claude-opus-4-8` with no per-session key got
/// `is_session_based = true`, `credential_class = Pooled` (same
/// host) and, at `NotByok`, an ACTIVE gate — so `manager_for` handed it the
/// Claude POOLED manager, whose `bearer_resolver` REPLACES the user's own
/// `sk-ant-…` on the wire, plus the OAuth Messages adaptation. Anthropic rejects
/// both. This is exactly what H4 prevents, reached through the `None` path.
///
/// A key that is absent or names a different model is not evidence for either
/// twin: resolve to NO platform, which the chokepoint then decides purely by the
/// ENDPOINT (the OAuth host is not this session's coding endpoint ⇒ nothing
/// rides).
///
/// Revert-to-red (production, compiles): delete the
/// `platform.oauth().is_some() && !disambiguated && slug_collides_across_platforms(..)`
/// refusal from `crate::agent::models::platform_for_slug` and every
/// `bearer_resolver` / `anthropic_oauth` assertion below fails.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn a_missing_or_stale_session_key_refuses_instead_of_guessing_the_oauth_twin() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let api_key_twin = "anthropic/claude-opus-4-8";
            let slug = "claude-opus-4-8";
            let host = anthropic_collision_host();
            let catalog = || {
                vec![
                    // API-key platform FIRST, exactly as `PlatformId::ALL` orders them.
                    managed_entry(api_key_twin, slug, &host),
                    managed_entry("claude-pro-max/claude-opus-4-8", slug, &host),
                ]
            };

            for (case, stale_key) in [
                // No key at all: a session spawned on a model that left the
                // catalog, or one an older build never seeded.
                ("absent", None),
                // Stale: an `OverrideModelName` rename, or a key naming a model
                // this session is no longer on.
                ("stale", Some("claude-pro-max/some-other-model".to_string())),
            ] {
                let (_dir, actor, _rx) =
                    actor_with_catalog(catalog(), api_key_twin, "sk-ant-user").await;
                *actor.selected_catalog_key.borrow_mut() = stale_key;

                let cfg = actor.reconstruct_full_config().await;
                assert!(
                    cfg.bearer_resolver.is_none(),
                    "{case}: LEAK — an unresolvable selection must get NO session bearer \
                     resolver; the pooled OAuth bearer would REPLACE the user's own key"
                );
                assert!(
                    !cfg.anthropic_oauth,
                    "{case}: nor the Claude OAuth Messages adaptation"
                );
                assert_eq!(
                    cfg.api_key.as_deref(),
                    Some("sk-ant-user"),
                    "{case}: the user's own provider key must survive untouched"
                );
                assert!(
                    actor
                        .credential_authority()
                        .manager_for(
                            crate::agent::models::platform_for_slug(
                                &actor.models_manager.models(),
                                actor.selected_catalog_key().as_deref(),
                                slug,
                            ),
                            &host,
                        )
                        .is_none(),
                    "{case}: and no manager either — refuse, never guess"
                );
            }

            // …while a session that DID select the OAuth twin still gets its
            // pooled resolver: the refusal is about the guess, not the platform.
            let (_dir, selected, _rx) =
                actor_with_catalog(catalog(), "claude-pro-max/claude-opus-4-8", "unused").await;
            let cfg = selected.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_some() && cfg.anthropic_oauth,
                "a DELIBERATE subscription selection keeps its pooled resolver and \
                 adaptation (this is what makes the refusals above meaningful)"
            );
        })
        .await;
}

/// H-c — coverage for the FIRST of the two production writers of
/// `selected_catalog_key`: the spawn seed
/// (`crate::agent::models::selected_catalog_key_for_spawn`, called from
/// `spawn.rs`). Every other test in this module sets the field by hand, so a
/// wrong seed was silent.
///
/// Both spawn shapes are covered: a FRESH session, spawned on the catalog key
/// the picker resolved, and a RESUME/LOAD, which spawns with the RAW persisted
/// `summary.current_model_id` — a BARE routing slug after any `SetSessionModel`,
/// since `handle_set_session_model` persists `sampling_config.model`. This seed
/// is where that slug becomes a key. The assertion is end-to-end: the seeded key
/// is fed to the very function the auth layer keys on.
#[test]
fn spawn_seeds_the_session_key_the_auth_layer_keys_on() {
    let slug = "claude-opus-4-8";
    let host = anthropic_collision_host();
    let api_key_twin = "anthropic/claude-opus-4-8";
    let oauth_twin = "claude-pro-max/claude-opus-4-8";
    let models: indexmap::IndexMap<String, crate::agent::config::ModelEntry> = [
        managed_entry(api_key_twin, slug, &host),
        managed_entry(oauth_twin, slug, &host),
    ]
    .into_iter()
    .collect();

    // FRESH: spawned on the catalog key. Idempotent, and it disambiguates.
    for selected in [api_key_twin, oauth_twin] {
        let seeded = crate::agent::models::selected_catalog_key_for_spawn(
            &models,
            &acp::ModelId::new(selected.to_string()),
        );
        assert_eq!(
            seeded.as_deref(),
            Some(selected),
            "a fresh session must record the catalog key it was spawned with"
        );
        assert_eq!(
            crate::agent::models::platform_for_slug(&models, seeded.as_deref(), slug),
            kigi_models::parse_managed_model_key(selected).map(|(p, _)| p),
            "…and that key must resolve THIS session's own platform for the bare slug"
        );
    }

    // RESUME/LOAD: `acp_agent::load_session` spawns with the RAW persisted id,
    // which after any model switch is the bare routing slug. THIS seed is what
    // turns it into a key — the picker's `.rev()` answer, which is the resume
    // default for a collided slug.
    let resumed = crate::agent::models::selected_catalog_key_for_spawn(
        &models,
        &acp::ModelId::new(slug.to_string()),
    );
    assert_eq!(
        resumed.as_deref(),
        Some(oauth_twin),
        "a bare persisted slug resolves through the picker's own lookup"
    );

    // A model that is no longer in the catalog seeds NOTHING, which (H-b) then
    // refuses rather than guessing a twin.
    assert_eq!(
        crate::agent::models::selected_catalog_key_for_spawn(
            &models,
            &acp::ModelId::new("gone/model".to_string()),
        ),
        None,
        "a model that left the catalog must not seed a key"
    );
}

/// H-c — coverage for the SECOND production writer: `SetSessionModel`
/// (`handle_set_session_model`), the picker's own path. The existing test
/// through this handler passes `None`, so a handler that dropped the key on the
/// floor stayed green.
///
/// End-to-end: after the switch the session's per-turn config must carry the
/// SELECTED twin's pooled resolver and adaptation, even though the bare slug in
/// the config is ambiguous.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn set_session_model_records_the_key_the_next_turn_resolves_on() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let slug = "claude-opus-4-8";
            let host = anthropic_collision_host();
            let api_key_twin = "anthropic/claude-opus-4-8";
            let oauth_twin = "claude-pro-max/claude-opus-4-8";
            let (_dir, actor, _rx) = actor_with_catalog(
                vec![
                    managed_entry(api_key_twin, slug, &host),
                    managed_entry(oauth_twin, slug, &host),
                ],
                api_key_twin,
                "sk-ant-user",
            )
            .await;

            // Switch to the SUBSCRIPTION twin, exactly as
            // `agent/handlers/model_switch.rs` does: the ambiguous slug in the
            // sampler config plus the catalog KEY the picker resolved.
            let models = actor.models_manager.models();
            let entry = models.get(oauth_twin).expect("catalog entry");
            let sampler = crate::agent::config::sampling_config_for_model(
                entry,
                crate::agent::config::resolve_credentials(entry, None),
                None,
            );
            actor
                .handle_set_session_model(
                    sampler,
                    Some(oauth_twin.to_string()),
                    false,
                    false,
                    true,
                    85,
                )
                .await
                .expect("model switch");

            assert_eq!(
                actor.selected_catalog_key().as_deref(),
                Some(oauth_twin),
                "SetSessionModel must record the picker's catalog key"
            );
            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_some(),
                "the switched-to subscription model must keep a live pooled resolver"
            );
            assert!(
                cfg.anthropic_oauth,
                "…and the Claude OAuth Messages adaptation"
            );

            // And back: switching to the API-key twin must UNDO both.
            let entry = models.get(api_key_twin).expect("catalog entry");
            let sampler = crate::agent::config::sampling_config_for_model(
                entry,
                crate::agent::config::resolve_credentials(entry, None),
                None,
            );
            actor
                .handle_set_session_model(
                    sampler,
                    Some(api_key_twin.to_string()),
                    false,
                    false,
                    true,
                    85,
                )
                .await
                .expect("model switch");
            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none() && !cfg.anthropic_oauth,
                "LEAK: switching back to the API-key twin must drop the pooled resolver \
                 and the OAuth adaptation"
            );
        })
        .await;
}

/// H-c — `OverrideModelName` is the one command that rewrites
/// `SamplingConfig::model` WITHOUT going through `SetSessionModel`, so it used
/// to leave `selected_catalog_key` naming a model the session is no longer on.
/// It must keep the field consistent: KEEP when the key still names the new
/// routing name, CLEAR otherwise — never re-resolve, which would put the
/// `.rev()` guess into the field the rule treats as a deliberate selection.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn override_model_name_keeps_the_session_key_consistent() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let slug = "claude-opus-4-8";
            let host = anthropic_collision_host();
            let oauth_twin = "claude-pro-max/claude-opus-4-8";
            let (_dir, actor, _rx) = actor_with_catalog(
                vec![
                    managed_entry("anthropic/claude-opus-4-8", slug, &host),
                    managed_entry(oauth_twin, slug, &host),
                ],
                oauth_twin,
                "unused",
            )
            .await;

            // A rename to the SAME model's routing slug (or to its catalog key)
            // keeps the selection.
            for same in [slug, oauth_twin] {
                actor.retain_selected_catalog_key_for(same);
                assert_eq!(
                    actor.selected_catalog_key().as_deref(),
                    Some(oauth_twin),
                    "{same}: still names the selected entry — keep it"
                );
            }

            // A rename to a DIFFERENT name makes the key stale: clear it, so the
            // collided slug refuses (H-b) instead of resolving the old model.
            actor.retain_selected_catalog_key_for("some-harness-model-name");
            assert_eq!(
                actor.selected_catalog_key(),
                None,
                "a stale key must be cleared, not carried into the next turn's \
                 platform lookup"
            );
        })
        .await;
}

/// M3 — the FIRST-PARTY aux case must honour the session gate, which is what
/// the old shape did implicitly.
///
/// `stamp_session_local_sampler_fields` used to copy
/// `active_session_config.bearer_resolver`, and that field is `None` whenever
/// the gate is inactive. Re-pointing the aux resolver at the chokepoint (the
/// LEAK 1b fix) made it `Some(primary)` for the session's own coding endpoint
/// REGARDLESS of the gate — so a BYOK / api-key session with a `[model.*]` aux
/// entry carrying its own key on that endpoint had that key REPLACED by the
/// primary bearer on every image-describe / auto-mode-classifier / summary
/// request (`SamplingClient::post` overrides the auth header from the resolver).
///
/// Revert-to-red (production, compiles): delete the
/// `if is_primary_channel && !SessionTokenAuthGate::new(…).active()` early
/// return from `sampler_turn::aux_bearer_resolver_for` and the first two rows
/// below resolve `KIMI_TOKEN`.
#[tokio::test(flavor = "current_thread")]
async fn first_party_aux_resolver_honours_the_session_gate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coding_host = kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url;
            let aux_slug = "kigi-aux";
            let mut info = crate::agent::config::ModelInfo::fallback(aux_slug);
            info.id = None; // a `[model.kigi-aux]` block, not a registry entry
            info.base_url = coding_host.to_string();
            let aux_entry = crate::agent::config::ModelEntry {
                info,
                api_key: None,
                env_key: None,
                api_base_url: None,
            };

            // (case, ACP auth method, the aux model's own BYOK status, expected)
            for (case, auth_method, byok, expect_resolver) in [
                (
                    "an API-key session: the aux model's own key must survive",
                    "deepseek",
                    crate::agent::auth_method::ModelByok::NotByok,
                    false,
                ),
                (
                    "a BYOK aux entry under a session method: its env_key wins",
                    "cached_token",
                    crate::agent::auth_method::ModelByok::Byok,
                    false,
                ),
                (
                    "the first-party subscription aux channel: byte-identical",
                    "cached_token",
                    crate::agent::auth_method::ModelByok::NotByok,
                    true,
                ),
            ] {
                let (_dir, actor, _rx) = actor_with_catalog(
                    vec![(aux_slug.to_string(), aux_entry.clone())],
                    aux_slug,
                    "",
                )
                .await;
                actor
                    .auth_method_id
                    .store(Some(Arc::new(acp::AuthMethodId::new(auth_method))));
                actor.model_auth_facts.replace(Some((
                    aux_slug.to_string(),
                    crate::agent::config::ModelAuthFacts {
                        byok,
                        auth_scheme: Default::default(),
                    },
                )));

                let resolved = actor.aux_bearer_resolver(aux_slug, coding_host);
                assert_eq!(
                    resolved.is_some(),
                    expect_resolver,
                    "{case}: aux resolver presence on the session's own endpoint"
                );
                if let Some(resolver) = resolved {
                    assert_eq!(
                        resolver.current_bearer(),
                        Some(KIMI_TOKEN.to_string()),
                        "{case}: and when it IS kept it is the primary's, live"
                    );
                }
            }
        })
        .await;
}

/// M-aux (REGRESSION this remediation introduced) — an aux call must NOT evict
/// the SESSION model's memoized auth facts.
///
/// `SessionActor::model_auth_facts` is a SINGLE slot. When `aux_bearer_resolver`
/// began asking it about the AUX slug, a definite result overwrote the session
/// model's entry, and:
///   (a) the next `reconstruct_full_config` re-paid `load_effective_config()` +
///       `resolve_model_list()` — the per-turn disk read M7/M9 removed — on top
///       of the one the aux call itself paid; and
///   (b) the memo's documented purpose (a transient `Unknown` falling back to
///       the last DEFINITE value FOR THE SAME model_id) was defeated: with the
///       aux slug in the slot, the session model's `Unknown` degrades to
///       `endpoint_is_first_party`, which is `false` for every
///       subscription-OAuth host — the session loses its `bearer_resolver` and
///       401s unrecoverably ~1h in, the failure L13 exists to prevent.
///
/// Round 3's deleted `repoint_aux_bearer_resolver` never touched the memo.
///
/// Revert-to-red (production, compiles): make `SessionActor::aux_bearer_resolver`
/// call `self.model_auth_facts(slug)` instead of `self.aux_model_auth_facts(slug)`
/// — the slot then names the aux slug and both assertions below fail.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn an_aux_call_does_not_evict_the_session_models_auth_facts() {
    let _env = anthropic_collision_env_guard();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let session_slug = "claude-opus-4-8";
            let host = anthropic_collision_host();
            let oauth_twin = "claude-pro-max/claude-opus-4-8";
            let (_dir, actor, _rx) = actor_with_catalog(
                vec![managed_entry(oauth_twin, session_slug, &host)],
                oauth_twin,
                "unused",
            )
            .await;

            // The session model's DEFINITE facts, as a turn would have memoized
            // them.
            actor.model_auth_facts.replace(Some((
                session_slug.to_string(),
                crate::agent::config::ModelAuthFacts {
                    byok: crate::agent::auth_method::ModelByok::NotByok,
                    auth_scheme: Default::default(),
                },
            )));

            // An aux turn: the auto-mode classifier / image-describe slug, which
            // is NOT the session's model.
            let _ = actor.aux_bearer_resolver("kigi-aux-classifier", &host);

            let memo = actor.model_auth_facts.borrow();
            let (cached_id, facts) = memo
                .as_ref()
                .expect("the session model's memo must survive an aux call");
            assert_eq!(
                cached_id, session_slug,
                "an aux call evicted the SESSION model's memo: the next turn re-reads \
                 config from disk, and a transient Unknown loses its definite fallback"
            );
            assert_eq!(facts.byok, crate::agent::auth_method::ModelByok::NotByok);
        })
        .await;
}
