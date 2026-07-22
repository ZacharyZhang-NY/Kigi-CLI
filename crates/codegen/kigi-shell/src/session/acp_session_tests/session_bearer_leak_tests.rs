//! LEAK GUARD (bearer_resolver channel): the primary (Kimi) subscription bearer
//! must never be stamped on a request to a host that does not own it.
//!
//! Chain the guard closes: a session-based ACP method (`cached_token` /
//! `kimi-code` / any OAuth platform) + a selected API-key-platform model
//! classifies `ModelByok::NotByok` (the model carries no `[model.*]` key), the
//! pre-fix `session_token_auth_gate` returned `true` unconditionally on that
//! arm, the manager lookup fell through to the primary Kimi manager for a
//! non-OAuth platform, and `SamplingClient::post` then REPLACED the correctly
//! resolved provider key with the Kimi bearer on the wire.
//!
//! The `api_key` half of the same defect (the config never even gets the
//! provider key, because `resolve_credentials` stamps the session token) is
//! pinned in `agent/mvp_agent/tests/api_key_channel_leak_tests.rs`, which drives
//! the real `prepare_sampling_config_for_model` resolution path. These tests
//! deliberately do NOT hand-stamp a provider key except where the assertion is
//! about the resolver overwriting one that already resolved correctly.
//!
//! The counterpart contract these tests also pin: the four subscription-OAuth
//! platforms have non-first-party base URLs but MUST keep a live
//! `bearer_resolver` drawn from their OWN pooled `AuthManager`, or they lose
//! mid-session token refresh.
//!
//! STORAGE DISCIPLINE (H6/M8): nothing here touches the developer's real
//! `~/.kigi` and nothing hot-swaps the process-global OAuth pool. Under
//! `cfg(test)` `oauth_registry::pool_home()` is a per-process temp path that is
//! never created, so every pooled manager is empty — exactly what the
//! assertions need (a live resolver that is provably NOT the Kimi one) — and
//! the binary leaves nothing behind.

use super::support::*;
use super::*;
use crate::agent::auth_method::ModelByok;
use crate::agent::config::{ModelAuthFacts, ModelEntry, ModelInfo};
use crate::auth::{AuthManager, AuthMode, KimiAuth, KimiCodeConfig};
use kigi_sampler::BearerResolver;
use std::sync::Arc;
use tokio::sync::mpsc;

/// The primary session bearer. Any occurrence of this string in an outgoing
/// request to a third-party host is the defect.
pub(super) const KIMI_TOKEN: &str = "kimi-subscription-token-DO-NOT-LEAK";

/// `(tempdir, manager)` standing in for the session's primary Kimi
/// `AuthManager`, holding a live (unexpired) OAuth session bearer.
fn kimi_primary() -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    am.hot_swap(KimiAuth {
        key: KIMI_TOKEN.to_string(),
        auth_mode: AuthMode::OAuth,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..KimiAuth::test_default()
    });
    (dir, am)
}

/// One catalog entry: catalog key `catalog_key`, routing slug `slug`, routed at
/// `base_url`, carrying no credential of its own (the shape every fetched
/// registry model has).
pub(super) fn managed_entry(catalog_key: &str, slug: &str, base_url: &str) -> (String, ModelEntry) {
    let mut info = ModelInfo::fallback(slug);
    info.id = Some(catalog_key.to_string());
    info.base_url = base_url.to_string();
    (
        catalog_key.to_string(),
        ModelEntry {
            info,
            api_key: None,
            env_key: None,
            api_base_url: None,
        },
    )
}

/// A `SessionActor` on a session-based ACP method with a live Kimi primary,
/// whose live catalog holds `catalog` and whose SELECTED model is the catalog
/// key `selected` (the picker's own notion of "current"). `wire_key` is the
/// already-correctly-resolved provider credential sitting in chat state.
///
/// The per-model BYOK memo is pinned to `NotByok` on purpose: that is what a
/// fetched registry model actually resolves to (`resolve_model_auth_facts` only
/// ever sees `default_models.json` + `[model.*]`), and pinning it keeps the test
/// independent of the developer's on-disk `~/.kigi/config.toml`.
pub(super) async fn actor_with_catalog(
    catalog: Vec<(String, ModelEntry)>,
    selected: &str,
    wire_key: &str,
) -> (
    tempfile::TempDir,
    Arc<SessionActor>,
    mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (dir, am) = kimi_primary();
    let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
    let mut actor = create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await;
    actor.auth_manager = Some(am);
    actor.auth_method_id = test_auth_method_id("cached_token");

    let mut selected_entry = None;
    for (key, entry) in catalog {
        if key == selected {
            selected_entry = Some(entry.clone());
        }
        actor.models_manager.insert_test_entry(key, entry);
    }
    let selected_entry = selected_entry.expect("the selected key must be in the catalog");
    // H4: the SESSION owns its selection. The process-global
    // `ModelsManager::current_model_id()` is deliberately left UNSET (it still
    // names the startup default, which is not in this catalog) — exactly what
    // Leader mode produces, since `agent/handlers/model_switch.rs` never calls
    // `set_current_model_id` there, and what a second concurrent session on a
    // colliding slug produces (last writer wins). Every assertion below
    // therefore rides the per-session key, not the global cell.
    *actor.selected_catalog_key.borrow_mut() = Some(selected.to_string());

    let slug = selected_entry.info().model.clone();
    actor
        .chat_state_handle
        .update_sampling_config(kigi_sampling_types::SamplingConfig {
            base_url: selected_entry.info().base_url.clone(),
            model: slug.clone(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            chat_compat: Default::default(),
            extra_headers: Default::default(),
            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
            reasoning_effort: None,
            stream_tool_calls: None,
        });
    actor
        .chat_state_handle
        .update_credentials(kigi_chat_state::Credentials {
            api_key: Some(wire_key.to_string()),
            auth_type: kigi_chat_state::AuthType::SessionToken,
            ..Default::default()
        });
    actor.model_auth_facts.replace(Some((
        slug,
        ModelAuthFacts {
            byok: ModelByok::NotByok,
            auth_scheme: Default::default(),
        },
    )));
    (dir, Arc::new(actor), persistence_rx)
}

/// Single-entry convenience over [`actor_with_catalog`].
pub(super) async fn actor_on_managed_model(
    catalog_key: &str,
    slug: &str,
    base_url: &str,
    wire_key: &str,
) -> (
    tempfile::TempDir,
    Arc<SessionActor>,
    mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    actor_with_catalog(
        vec![managed_entry(catalog_key, slug, base_url)],
        catalog_key,
        wire_key,
    )
    .await
}

/// THE leak test, at the wire. A `deepseek/deepseek-chat` turn on a session
/// (`cached_token`) method with a live Kimi primary must send DeepSeek's own key
/// — the Kimi subscription bearer must not appear anywhere in the request.
///
/// Revert-to-red: dropping the `credential_class` conjunct from
/// `session_token_auth_gate` puts `Bearer <KIMI_TOKEN>` on this request.
#[tokio::test(flavor = "multi_thread")]
async fn deepseek_turn_under_a_kimi_session_sends_no_kimi_bearer_on_the_wire() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-1",
                "object": "chat.completion",
                "created": 0,
                "model": "deepseek-chat",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }]
            })),
        )
        .mount(&server)
        .await;
    let uri = server.uri();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, actor, _rx) = actor_on_managed_model(
                "deepseek/deepseek-chat",
                "deepseek-chat",
                &uri,
                "sk-deepseek-provider-key",
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "an API-key platform model must get NO session bearer resolver"
            );
            let client =
                kigi_sampler::SamplingClient::new(cfg).expect("sampling client must construct");
            let _ = client
                .chat_completion(kigi_sampling_types::ChatCompletionRequest::new(
                    "deepseek-chat",
                    vec![kigi_sampling_types::ChatRequestMessage::user("hi")],
                ))
                .await;
        })
        .await;

    let requests = server
        .received_requests()
        .await
        .expect("wiremock records requests");
    assert_eq!(requests.len(), 1, "exactly one inference request was sent");
    let auth = requests[0]
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .expect("the request must carry an Authorization header")
        .to_string();
    assert!(
        !auth.contains(KIMI_TOKEN),
        "the Kimi subscription bearer must never reach a third-party inference host"
    );
    assert_eq!(
        auth, "Bearer sk-deepseek-provider-key",
        "the correctly-resolved provider key must survive to the wire"
    );
}

/// The same guard for every other API-key registry platform shape: OpenAI
/// (Responses), Anthropic (x-api-key/Messages), Groq, Together and Z.AI CN — all
/// classify `NotByok`, all route to a non-first-party host, none may receive a
/// session bearer resolver.
#[tokio::test(flavor = "current_thread")]
async fn api_key_platform_models_get_no_session_bearer_resolver() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (catalog_key, slug, base_url) in [
                ("openai/gpt-5.2", "gpt-5.2", "https://api.openai.com/v1"),
                (
                    "anthropic/claude-opus-4-8",
                    "claude-opus-4-8",
                    "https://api.anthropic.com/v1",
                ),
                ("groq/llama-4", "llama-4", "https://api.groq.com/openai/v1"),
                ("together/qwen-3", "qwen-3", "https://api.together.xyz/v1"),
                (
                    "zai-coding-cn/glm-5",
                    "glm-5",
                    "https://open.bigmodel.cn/api/paas/v4",
                ),
            ] {
                let (_dir, actor, _rx) =
                    actor_on_managed_model(catalog_key, slug, base_url, "sk-provider-key").await;
                let cfg = actor.reconstruct_full_config().await;
                assert!(
                    cfg.bearer_resolver.is_none(),
                    "{catalog_key}: an API-key platform must get no session bearer resolver"
                );
                assert_eq!(
                    cfg.api_key.as_deref(),
                    Some("sk-provider-key"),
                    "{catalog_key}: the provider key must stay on the config"
                );
            }
        })
        .await;
}

/// C2 at the resolver channel: a `[model.*]` entry has NO platform
/// (`info.id == None`), which used to be a blanket allow. Pointed at a
/// third-party host it must get no session resolver; pointed at the session's
/// own coding endpoint (a config.toml `[endpoints] coding_api_base_url`
/// deployment, a `KIGI_CODE_BASE_URL` override, or a local dev proxy) it must
/// keep one — that is why the predicate is not `is_first_party_url`.
///
/// Revert-to-red: make `CredentialAuthority::is_session_coding_endpoint` return
/// `true` unconditionally and a Kimi resolver lands on the openai.com config.
#[tokio::test(flavor = "current_thread")]
async fn config_model_entry_takes_a_session_resolver_only_on_its_own_endpoint() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for base_url in ["https://api.openai.com/v1", "https://api.deepseek.com/v1"] {
                let mut info = ModelInfo::fallback("gpt-4o");
                info.id = None; // a `[model.gpt-4o]` block
                info.base_url = base_url.to_string();
                let entry = ModelEntry {
                    info,
                    api_key: None,
                    // An env_key that is NOT set: `has_own_credentials()` probes
                    // `std::env::var` at call time, so this classifies NotByok.
                    env_key: None,
                    api_base_url: None,
                };
                let (_dir, actor, _rx) =
                    actor_with_catalog(vec![("gpt-4o".to_string(), entry)], "gpt-4o", "").await;
                let cfg = actor.reconstruct_full_config().await;
                assert!(
                    cfg.bearer_resolver.is_none(),
                    "LEAK: a [model.*] block at {base_url} must get no session bearer resolver"
                );
            }

            for base_url in [
                kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
                "http://127.0.0.1:4141/v1",
            ] {
                let mut info = ModelInfo::fallback("kigi-4.5");
                info.id = None;
                info.base_url = base_url.to_string();
                let entry = ModelEntry {
                    info,
                    api_key: None,
                    env_key: None,
                    api_base_url: None,
                };
                let (_dir, actor, _rx) =
                    actor_with_catalog(vec![("kigi-4.5".to_string(), entry)], "kigi-4.5", "").await;
                let resolver = actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .expect("the session's own endpoint keeps the session resolver");
                assert_eq!(
                    resolver.current_bearer(),
                    Some(KIMI_TOKEN.to_string()),
                    "{base_url}: a custom deployment / dev proxy is unchanged"
                );
            }
        })
        .await;
}

/// The Kimi / first-party subscription channel must be BYTE-IDENTICAL: the
/// session model keeps its live bearer_resolver AND the pre-flight refresh
/// still heals a stale buffered key. This is also what proves the Kimi bearer is
/// live in every LEAK assertion in this module — it WOULD leak if the guard
/// were missing.
///
/// (L12: the slug-collision commentary that used to sit here belongs to the
/// collision tests in `session_bearer_leak_platform_tests`, which is where its
/// revert-to-red actually reproduces; on this first-party test it never could.)
#[tokio::test(flavor = "current_thread")]
async fn kimi_first_party_model_still_rides_the_primary_session_bearer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, actor, _rx) = actor_on_managed_model(
                "kimi-code/kimi-for-coding",
                "kimi-for-coding",
                kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url,
                "stale-buffered-token",
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;
            let resolver = cfg
                .bearer_resolver
                .as_ref()
                .expect("the subscription model must keep the live session resolver");
            assert_eq!(
                resolver.current_bearer(),
                Some(KIMI_TOKEN.to_string()),
                "the first-party model resolves the primary session bearer"
            );

            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some(KIMI_TOKEN),
                "the first-party pre-flight refresh must still heal the stale key"
            );
        })
        .await;
}

/// The persistence half of the defect: `refresh_token_if_expired` used to write
/// the Kimi session token into `chat_state` `creds.api_key` for ANY
/// session-method turn, from where it propagated to subagents and aux configs.
/// A deepseek turn must leave the provider key untouched.
///
/// M7 rides along: a registry-platform model must not fall into
/// `reload_api_key_from_config` at all (a `load_effective_config()` disk read
/// per turn plus a permanently false "not found in config.toml" warning), so
/// the key is left exactly as resolved.
#[tokio::test(flavor = "current_thread")]
async fn preflight_refresh_never_writes_the_kimi_token_into_a_platform_credential() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, actor, _rx) = actor_on_managed_model(
                "deepseek/deepseek-chat",
                "deepseek-chat",
                "https://api.deepseek.com/v1",
                "sk-deepseek-provider-key",
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("sk-deepseek-provider-key"),
                "the Kimi session token must never overwrite a platform credential"
            );
        })
        .await;
}
