//! Sampler-turn pipeline for `SessionActor`: tool definitions, model auth
//! facts/gates and retry, sampler config reconstruction, sampling-failure
//! recovery, and per-response usage recording.
use super::*;
/// Auth-failure detector for tool errors. Matches strictly on HTTP 401
/// when the error carries a structured status code, mirroring
/// `SamplingError::is_auth_error` in kigi-sampling-types: 403 is
/// deliberately excluded because it means "authenticated but forbidden"
/// (content-safety blocks, ZDR-gated requests, remote settings gates), where
/// a token refresh would be a no-op and would surface to the client as
/// a spurious auth_required teardown.
///
/// String fallbacks remain for tools that surface auth failures without
/// going through the structured `HttpFailure` path (e.g. JSON-only
/// `invalid_token` payloads, BYOK key-validation messages).
pub(super) fn is_auth_tool_error(err: &kigi_tool_runtime::ToolError) -> bool {
    if let Some(details) = &err.details
        && let Some(status) = details
            .get(HTTP_STATUS_DETAILS_KEY)
            .and_then(|s| s.as_u64())
    {
        return status == 401;
    }
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_token")
}
/// Gate inputs bundled with the composed decision so the 401-recovery log can
/// report the components.
#[derive(Clone, Copy)]
pub(crate) struct SessionTokenAuthGate {
    is_session_based: bool,
    model_byok: crate::agent::auth_method::ModelByok,
    /// Whether the request targets a first-party host. Lets an `Unknown`
    /// BYOK status still refresh against the first-party cli-chat-proxy hosts without
    /// risking a session-token leak to a third-party BYOK endpoint.
    endpoint_is_first_party: bool,
    /// WHICH credential this model's platform/endpoint pair accepts, per the
    /// single credential chokepoint
    /// ([`crate::auth::credential_authority::CredentialAuthority::credential_class`]).
    /// `None` for every API-key registry platform, which keeps the primary Kimi
    /// bearer off `api.deepseek.com` / `api.openai.com` / … .
    credential_class: crate::auth::credential_authority::CredentialClass,
}
impl SessionTokenAuthGate {
    /// Single place `is_session_based` / `endpoint_is_first_party` are derived,
    /// so all call sites assemble the gate identically. `model_platform` is the
    /// registry platform the model routes to (`None` for a bare / `[model.*]`
    /// entry) — it MUST be derived from the same lookup
    /// ([`SessionActor::model_platform`]) that
    /// [`SessionActor::auth_manager_for_endpoint`] uses, so the gate's verdict
    /// and the manager actually wrapped as the bearer resolver can never
    /// disagree. `authority` is that same chokepoint, so the gate cannot answer
    /// the endpoint question differently from the manager routing.
    pub(crate) fn new(
        auth_method_id: Option<&acp::AuthMethodId>,
        model_byok: crate::agent::auth_method::ModelByok,
        base_url: &str,
        model_platform: Option<kigi_models::PlatformId>,
        authority: &crate::auth::credential_authority::CredentialAuthority,
    ) -> Self {
        Self {
            // L13: a model whose OWN credential is a pooled subscription-OAuth
            // session is session-based BY ITSELF, whatever the primary ACP
            // method is. A user logged in with an API-KEY platform (e.g.
            // `deepseek`) who selects a `claude-pro-max/*` model still gets that
            // platform's pooled bearer as the request's `api_key` — without this
            // term the gate would be inactive, so the config would carry NO
            // resolver: the token freezes at selection time and the session dies
            // with an unrecoverable 401 once it expires (~1h). The outer
            // `credential_class` conjunct keeps this confined to that
            // platform's own host.
            is_session_based: auth_method_id
                .is_some_and(crate::agent::auth_method::is_session_based_method)
                || model_platform.is_some_and(|p| p.oauth().is_some()),
            model_byok,
            endpoint_is_first_party: crate::util::is_first_party_url(base_url),
            credential_class: authority.credential_class(model_platform, base_url),
        }
    }
    pub(crate) fn active(self) -> bool {
        crate::agent::auth_method::session_token_auth_gate(
            self.is_session_based,
            self.model_byok,
            self.endpoint_is_first_party,
            self.credential_class,
        )
    }
}
/// THE aux / summary `bearer_resolver` rule, stated ONCE.
///
/// `SamplingClient::post` REPLACES the request's auth header from the resolver,
/// so an aux model on a different provider would have its own correctly-resolved
/// key overwritten by the session bearer ON THE AUX HOST. An OAuth aux model
/// gets a live resolver over ITS OWN pooled manager (keeping mid-session
/// refresh); a first-party aux model gets the primary's, but ONLY when the
/// session-token gate is active; everything else gets `None`, so the aux model's
/// own key survives to the wire.
///
/// The FIRST-PARTY case honours the gate — it yields no resolver whenever the
/// gate is inactive. Without that, a BYOK / api-key session with a `[model.*]`
/// aux entry carrying its OWN `env_key` on the session's own coding endpoint has
/// that key REPLACED on the wire by the primary bearer on every image-describe /
/// auto-mode-classifier / summary request. A subscription-OAuth aux model is
/// deliberately NOT gated this way: its pooled token IS its credential, and
/// withholding the resolver only costs it mid-session refresh (L13).
///
/// Shared by [`SessionActor::aux_bearer_resolver`] and
/// `MvpAgent::summary_bearer_resolver`: the summary client is built by the
/// AGENT, not the session actor, so it holds its own private copy of this rule.
pub(crate) fn aux_bearer_resolver_for(
    authority: &crate::auth::credential_authority::CredentialAuthority,
    auth_method_id: Option<&acp::AuthMethodId>,
    platform: Option<kigi_models::PlatformId>,
    model_byok: crate::agent::auth_method::ModelByok,
    base_url: &str,
) -> Option<kigi_sampler::SharedBearerResolver> {
    let is_primary_channel = platform.is_none_or(|p| p.oauth().is_none());
    if is_primary_channel
        && !SessionTokenAuthGate::new(auth_method_id, model_byok, base_url, platform, authority)
            .active()
    {
        return None;
    }
    authority.bearer_resolver_for(platform, base_url)
}
/// Run a tool call; on an auth-shaped failure, attempt recovery via
/// `AuthManager` and one retry. When `shared_recovery` is `Some`, concurrent
/// 401s in the same batch deduplicate via `OnceCell::get_or_init`.
pub(super) async fn call_with_auth_retry<F, Fut>(
    auth_manager: Option<&std::sync::Arc<crate::auth::AuthManager>>,
    shared_recovery: Option<&tokio::sync::OnceCell<bool>>,
    tool_name: &str,
    mut call: F,
) -> Result<kigi_tools::types::output::ToolRunResult, kigi_tool_runtime::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<kigi_tools::types::output::ToolRunResult, kigi_tool_runtime::ToolError>,
        >,
{
    let result = call().await;
    let Err(ref err) = result else { return result };
    if !is_auth_tool_error(err) {
        return result;
    }
    let Some(am) = auth_manager else {
        return result;
    };
    let recovered = match shared_recovery {
        Some(cell) => *cell.get_or_init(|| am.try_recover_unauthorized()).await,
        None => am.try_recover_unauthorized().await,
    };
    if recovered {
        tracing::info!(
            tool = tool_name,
            "auth recovery: tool 401, recovered, retrying"
        );
        call().await
    } else {
        tracing::warn!(tool = tool_name, "auth recovery: tool 401, refresh failed");
        kigi_log::unified_log::warn(
            "auth recovery: tool 401, refresh failed",
            None,
            Some(serde_json::json!({ "tool" : tool_name })),
        );
        result
    }
}
/// Wraps an [`AuthManager`](crate::auth::AuthManager) as a sampler
/// [`BearerResolver`](kigi_sampler::BearerResolver), resolving the live
/// (current-or-expired) bearer at request time. Shared by
/// [`SessionActor::reconstruct_full_config`] (the session model) and the
/// aux-model bearer routing
/// ([`CredentialAuthority::bearer_resolver_for`](crate::auth::credential_authority::CredentialAuthority::bearer_resolver_for))
/// so both wrap ONE definition. SECURITY: the bearer is resolved per request
/// and never logged.
pub(crate) struct AuthManagerBearerResolver(pub(crate) std::sync::Arc<crate::auth::AuthManager>);
impl std::fmt::Debug for AuthManagerBearerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManagerBearerResolver").finish()
    }
}
impl kigi_sampler::BearerResolver for AuthManagerBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        self.0.current_or_expired().map(|a| a.key)
    }
}
pub(crate) fn auth_manager_bearer_resolver(
    am: std::sync::Arc<crate::auth::AuthManager>,
) -> kigi_sampler::SharedBearerResolver {
    std::sync::Arc::new(AuthManagerBearerResolver(am))
}
impl SessionActor {
    pub(super) async fn prepare_tool_definitions_timed(&self) -> (Vec<ToolDefinition>, u64) {
        let mcp_wait_start = std::time::Instant::now();
        match self.mcp_strategy {
            McpInitStrategy::Blocking => {
                if !self.mcp_state.lock().await.is_initialized() {
                    tracing::info!(
                        "Blocking strategy: waiting for MCP initialization before first prompt..."
                    );
                    self.wait_for_mcp_initialized().await;
                }
            }
            McpInitStrategy::Progressive => {}
        }
        let mcp_wait_ms = mcp_wait_start.elapsed().as_millis() as u64;
        let defs = self.prepare_tool_definitions_inner().await;
        (defs, mcp_wait_ms)
    }
    pub(super) async fn prepare_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.prepare_tool_definitions_timed().await.0
    }
    /// The exact tool specs a turn sends, BEFORE the turn-specific
    /// structured-output append. Single source of truth shared by the turn
    /// (`acp_session_impl/turn.rs`) and the `SnapshotToolDefinitions` handler, so
    /// a verbatim-fork child's tool prefix can never silently drift from what the
    /// parent turn actually sends. `defs` is the already-resolved tool list
    /// (`prepare_tool_definitions_*`); this applies only the `web_search` drop
    /// under backend search and the `ToolSpec::from` mapping.
    pub(crate) fn turn_base_tool_specs(&self, defs: &[ToolDefinition]) -> Vec<ToolSpec> {
        let use_backend_search =
            self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get();
        defs.iter()
            .filter(|td| !use_backend_search || td.function.name != "web_search")
            .cloned()
            .map(ToolSpec::from)
            .collect()
    }
    pub(super) async fn prepare_tool_definitions_inner(&self) -> Vec<ToolDefinition> {
        let bridge = self.agent.borrow().tool_bridge().clone();
        let defs = bridge.tool_definitions_builtins_only().await;
        let plan_active = self.plan_mode.lock().is_active();
        filter_cursor_tools_by_plan_mode(defs, plan_active)
    }
    /// Memoized per-model [`ModelAuthFacts`](crate::agent::config::ModelAuthFacts)
    /// for the SESSION's own model, keyed by `model_id`.
    ///
    /// A fresh `Unknown` (config currently unparseable) falls back to the last
    /// definite value for the same `model_id` rather than demoting a live session
    /// to non-refreshable api-key mode. Because a config edit can turn the
    /// currently-selected model into a per-model BYOK model without changing
    /// `model_id`, keying on `model_id` alone is insufficient — each
    /// model/credential chokepoint must clear this memo (`replace(None)`).
    pub(super) fn model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        self.resolve_auth_facts(model_id, true)
    }
    /// [`Self::model_auth_facts`] for a model that is NOT the session's own — an
    /// AUX / summary / image-describe slug.
    ///
    /// Identical resolution, but it NEVER WRITES the slot. The memo is a SINGLE
    /// slot: were the aux path to share it, one classifier or image-describe call
    /// would evict the session model's entry, and (a) the next
    /// [`Self::reconstruct_full_config`] would pay another `load_effective_config()`
    /// + `resolve_model_list()` disk read while (b) a transient `Unknown` for the
    /// SESSION model would then have no same-`model_id` definite value to fall
    /// back to, so it would degrade to `endpoint_is_first_party` — `false` for
    /// every subscription-OAuth host, costing the session its `bearer_resolver`
    /// and 401ing unrecoverably ~1h in (the failure L13 prevents). Reading a
    /// matching entry is still allowed: it can only hit when the slot already
    /// names this same slug.
    fn aux_model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        self.resolve_auth_facts(model_id, false)
    }
    /// Shared body of [`Self::model_auth_facts`] / [`Self::aux_model_auth_facts`].
    /// `memoize` is the ONLY difference, so the two can never resolve differently.
    fn resolve_auth_facts(
        &self,
        model_id: &str,
        memoize: bool,
    ) -> crate::agent::config::ModelAuthFacts {
        use crate::agent::auth_method::ModelByok;
        if let Some((cached_id, facts)) = self.model_auth_facts.borrow().as_ref()
            && cached_id == model_id
            && facts.byok != ModelByok::Unknown
        {
            return *facts;
        }
        let fresh = crate::agent::config::resolve_model_auth_facts(model_id);
        if fresh.byok == ModelByok::Unknown {
            if let Some((cached_id, facts)) = self.model_auth_facts.borrow().as_ref()
                && cached_id == model_id
            {
                return *facts;
            }
            return fresh;
        }
        if memoize {
            *self.model_auth_facts.borrow_mut() = Some((model_id.to_string(), fresh));
        }
        fresh
    }
    /// Gate inputs for the SESSION model `model_id` routed to `base_url`. See
    /// [`crate::agent::auth_method::session_token_auth_gate`] for the rationale
    /// (`base_url` keeps an `Unknown` BYOK status refreshable only
    /// against first-party xAI hosts).
    fn auth_gate(&self, model_id: &str, base_url: &str) -> SessionTokenAuthGate {
        let byok = self.model_auth_facts(model_id).byok;
        let auth_method = self.auth_method_id.load();
        SessionTokenAuthGate::new(
            auth_method.as_deref(),
            byok,
            base_url,
            self.model_platform(model_id),
            &self.credential_authority(),
        )
    }
    /// This session's credential chokepoint: its EFFECTIVE endpoints (so a
    /// managed `[endpoints] coding_api_base_url` deployment keeps the session
    /// bearer — H3) plus its primary manager, which the authority keeps
    /// private. Every inference-auth question this actor asks goes through it.
    pub(crate) fn credential_authority(
        &self,
    ) -> crate::auth::credential_authority::CredentialAuthority {
        crate::auth::credential_authority::CredentialAuthority::new(
            self.models_manager.endpoints(),
            self.auth_manager.clone(),
        )
    }
    /// The [`AuthManager`](crate::auth::AuthManager) that governs INFERENCE auth
    /// for the routing slug `model` against the endpoint the request will
    /// ACTUALLY be sent to, from the ONE chokepoint
    /// ([`crate::auth::credential_authority::CredentialAuthority`]).
    ///
    /// A subscription-OAuth platform routes to ITS OWN scope-keyed pooled
    /// manager; `kimi-code` and a platform-less model on the session's own
    /// coding endpoint route to the primary; every API-key registry platform,
    /// and any endpoint that is neither, routes to `None` — fail fast, never a
    /// silent fallback to the primary. `None` also for a BYOK / test session
    /// with no primary.
    ///
    /// Callers pass the LIVE sampling config's `base_url` so the manager, the
    /// gate and the wire can never be resolved against three different endpoints
    /// (an `OverrideModelName` session keeps its original `base_url` under a
    /// routing name absent from the catalog).
    pub(super) fn auth_manager_for_endpoint(
        &self,
        model: &str,
        base_url: &str,
    ) -> Option<std::sync::Arc<crate::auth::AuthManager>> {
        self.credential_authority()
            .manager_for(self.model_platform(model), base_url)
    }
    /// The SESSION credential (if any) that may ride a request for the routing
    /// slug `model`. The only producer is the chokepoint.
    pub(super) fn session_credential_for_model(
        &self,
        model: &str,
    ) -> Option<crate::auth::credential_authority::SessionCredential> {
        self.credential_authority()
            .credential_for(self.model_platform(model), &self.model_base_url(model))
    }
    /// The base URL a routing slug actually resolves to in the live catalog.
    /// Falls back to the session's own inference endpoint for an unlisted slug,
    /// which is exactly where `resolve_aux_model_sampling_config`'s Tier-2
    /// fallback entry routes — so the endpoint the rule is applied to is always
    /// the endpoint the request is sent to.
    fn model_base_url(&self, model: &str) -> String {
        let models = self.models_manager.models();
        match crate::agent::config::find_model_by_id(&models, model) {
            Some(entry) => entry.info().base_url.clone(),
            None => self.models_manager.endpoints().resolve_inference_base_url(),
        }
    }
    /// This SESSION's own selected catalog key (H4) — never the process-global
    /// `ModelsManager::current_model_id()`, which Leader mode never writes and
    /// which is last-writer-wins across concurrent sessions.
    pub(super) fn selected_catalog_key(&self) -> Option<String> {
        self.selected_catalog_key.borrow().clone()
    }
    /// Keep the session's own selected catalog key consistent with an
    /// `OverrideModelName` rename: KEEP it when it still names `model_name`
    /// (same entry, new routing name), otherwise CLEAR it.
    ///
    /// H-c: `OverrideModelName` is the one command that rewrites
    /// `SamplingConfig::model` without going through `SetSessionModel`, so it can
    /// leave the field naming a model the session is no longer on.
    /// Clearing rather than re-resolving is deliberate: re-resolving would put
    /// `resolve_catalog_key`'s `.rev()` guess INTO the field the whole rule
    /// treats as the session's deliberate selection, and a cleared field
    /// refuses a collided slug instead of guessing its OAuth twin (H-b).
    pub(super) fn retain_selected_catalog_key_for(&self, model_name: &str) {
        let models = self.models_manager.models();
        let still_names_it = self.selected_catalog_key().is_some_and(|key| {
            key == model_name
                || models
                    .get(key.as_str())
                    .is_some_and(|entry| entry.info.model == model_name)
        });
        if !still_names_it {
            *self.selected_catalog_key.borrow_mut() = None;
        }
    }
    /// The registry platform the routing slug `model` belongs to, from the SAME
    /// lookup [`Self::auth_manager_for_endpoint`] routes on. `None` for a bare /
    /// `[model.*]` / unlisted model.
    pub(super) fn model_platform(&self, model: &str) -> Option<kigi_models::PlatformId> {
        let models = self.models_manager.models();
        crate::agent::models::platform_for_slug(
            &models,
            self.selected_catalog_key().as_deref(),
            model,
        )
    }
    /// Whether `model` routes to the Claude Pro/Max OAuth-Messages platform
    /// (claude-pro-max) — the gate for the sampler's OAuth Messages adaptation
    /// (identity headers + "You are Claude Code" system prefix). A generic-OAuth
    /// platform speaking the Messages wire; every other model (incl. xai-grok,
    /// which is ChatCompletions) returns `false`, keeping the API-key Anthropic
    /// / MiniMax Messages requests byte-identical.
    fn model_is_anthropic_oauth(&self, model: &str) -> bool {
        self.model_platform(model).is_some_and(|platform| {
            platform.oauth().is_some()
                && platform.wire_api() == kigi_models::PlatformWireApi::Messages
        })
    }
    /// Whether `model` routes to the GitHub Copilot ChatCompletions platform
    /// (github-copilot) — the gate for the sampler's editor-identity headers +
    /// `X-Initiator`. Every other model returns `false`, keeping the other
    /// ChatCompletions providers byte-identical.
    fn model_is_github_copilot(&self, model: &str) -> bool {
        self.model_platform(model)
            .is_some_and(kigi_models::PlatformId::sends_copilot_editor_headers)
    }
    /// Whether `model` routes to the ChatGPT/Codex Responses platform
    /// (openai-codex) — the gate for the sampler's Codex identity headers
    /// (`chatgpt-account-id` + originator + OpenAI-Beta). Every other model
    /// returns `false`, keeping the API-key `openai` Responses request
    /// byte-identical.
    fn model_is_openai_codex(&self, model: &str) -> bool {
        self.model_platform(model)
            .is_some_and(kigi_models::PlatformId::sends_codex_responses_headers)
    }
    /// The `bearer_resolver` an AUX / summary `SamplerConfig` may carry — the
    /// shared [`aux_bearer_resolver_for`] rule applied to the AUX model's own
    /// platform + endpoint.
    ///
    /// The aux config never inherits the session resolver: it is passed this
    /// value explicitly (see
    /// [`crate::agent::config::stamp_session_local_sampler_fields`]), so
    /// "forgot to re-point" is not expressible.
    ///
    /// The BYOK status comes from [`Self::aux_model_auth_facts`], which does NOT
    /// write the session model's single-slot memo.
    pub(super) fn aux_bearer_resolver(
        &self,
        slug: &str,
        base_url: &str,
    ) -> Option<kigi_sampler::SharedBearerResolver> {
        let auth_method = self.auth_method_id.load();
        // An aux slug is NOT the session's selection, so it must not be resolved
        // against `selected_catalog_key` — the same rule the aux `api_key` obeys
        // (`credential_for_slug(.., None, ..)`). Keying an aux model on the
        // SESSION's selection let a colliding same-vendor slug resolve the OAuth
        // twin, whose pooled resolver would then overwrite the user's own key on
        // the aux request.
        let models = self.models_manager.models();
        aux_bearer_resolver_for(
            &self.credential_authority(),
            auth_method.as_deref(),
            crate::agent::models::platform_for_slug(&models, None, slug),
            self.aux_model_auth_facts(slug).byok,
            base_url,
        )
    }
    /// Emit a unified-log breadcrumb whenever the session-token refresh gate is
    /// evaluated with an **`Unknown`** per-model BYOK status on a session-based
    /// method — the condition that would otherwise silently demote live sessions
    /// to stale-token 401s. The uploaded per-turn unified log then shows whether
    /// the first-party-endpoint fallback kept refresh active or withheld it, so a
    /// residual demotion can be caught per session even when server-side metrics
    /// only show the aggregate 401. No-op for a definite `Byok`/`NotByok`, so
    /// steady-state turns stay quiet — a burst of these is itself the signal that
    /// `Unknown` is being hit in the field.
    fn log_auth_gate_unknown(&self, site: &str, gate: SessionTokenAuthGate, base_url: &str) {
        use crate::agent::auth_method::ModelByok;
        if gate.model_byok != ModelByok::Unknown || !gate.is_session_based {
            return;
        }
        let refresh_active = gate.active();
        let ctx = serde_json::json!(
            { "site" : site, "model_byok" : gate.model_byok.as_str(), "is_session_based"
            : gate.is_session_based, "endpoint_is_first_party" : gate
            .endpoint_is_first_party, "credential_class" : gate.credential_class
            .as_str(), "refresh_active" : refresh_active, "base_url" : base_url, }
        );
        let sid = Some(self.session_info.id.0.as_ref());
        if refresh_active {
            kigi_log::unified_log::info(
                "auth gate: Unknown BYOK on first-party endpoint — session-token refresh kept active",
                sid,
                Some(ctx),
            );
        } else {
            kigi_log::unified_log::warn(
                "auth gate: Unknown BYOK on non-first-party endpoint — refresh withheld (may surface stale-token 401)",
                sid,
                Some(ctx),
            );
        }
    }
    /// Reconstruct a full `SamplerConfig` (with credentials) by combining
    /// the actor's `SamplingConfig` and `Credentials`. Folds in the
    /// URL-derived headers (cli-chat-proxy auth, the staging auth header)
    /// so the sampler crate stays URL-agnostic.
    pub(super) async fn reconstruct_full_config(&self) -> SamplingConfig {
        #[allow(clippy::items_after_statements)]
        #[derive(Debug)]
        struct TraceContextInjector;
        impl kigi_sampler::HeaderInjector for TraceContextInjector {
            fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
                if let Some(tp) = kigi_file_utils::trace_context::current_traceparent()
                    && let Ok(v) = reqwest::header::HeaderValue::from_str(&tp)
                {
                    headers.insert("traceparent", v);
                }
            }
        }
        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap_or_else(|| kigi_sampling_types::SamplingConfig {
                base_url: String::new(),
                model: String::new(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                chat_compat: Default::default(),
                extra_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            });
        let creds = self.chat_state_handle.get_credentials().await;
        let model_facts = self.model_auth_facts(cfg.model.as_str());
        let auth_method = self.auth_method_id.load();
        let gate = SessionTokenAuthGate::new(
            auth_method.as_deref(),
            model_facts.byok,
            &cfg.base_url,
            self.model_platform(cfg.model.as_str()),
            &self.credential_authority(),
        );
        let use_bearer_resolver = gate.active();
        self.log_auth_gate_unknown("reconstruct_full_config", gate, &cfg.base_url);
        // Resolve the bearer from the ACTIVE model's OWN manager: a grok model
        // wraps the xai-grok manager, never the Kimi one (captured before
        // `cfg.model` is moved into the struct below). `None` when the gate is
        // inactive or the oauth provider has no manager (fail-fast, no Kimi
        // fallback).
        let inference_auth_manager = if use_bearer_resolver {
            self.auth_manager_for_endpoint(&cfg.model, &cfg.base_url)
        } else {
            None
        };
        // Claude Pro/Max OAuth Messages adaptation for THIS turn's model
        // (captured before `cfg.model` is moved into the struct below).
        let anthropic_oauth = self.model_is_anthropic_oauth(&cfg.model);
        // GitHub Copilot editor-identity headers for THIS turn's model.
        let github_copilot = self.model_is_github_copilot(&cfg.model);
        // ChatGPT/Codex identity headers for THIS turn's model.
        let openai_codex = self.model_is_openai_codex(&cfg.model);
        let auth_scheme = model_facts.auth_scheme;
        let mut extra_headers = cfg.extra_headers;
        crate::agent::config::inject_url_derived_headers(
            &mut extra_headers,
            creds.alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        let compaction_at_tokens = self.compaction_at_tokens.get();
        let compactions_remaining = self.compactions_remaining.get();
        if compactions_remaining.is_some() || compaction_at_tokens.is_some() {
            let has_compaction_summary = self
                .chat_state_handle
                .get_last_compaction_prompt_index()
                .await
                .is_some();
            if let Some(value) =
                compactions_remaining.and_then(|c| c.resolve(has_compaction_summary))
            {
                extra_headers.insert("x-compactions-remaining".to_string(), value.to_string());
            }
            if !has_compaction_summary
                && let Some(value) = compaction_at_tokens.and_then(|c| {
                    c.resolve(
                        cfg.context_window.get(),
                        self.compaction.threshold_percent.get(),
                    )
                })
            {
                extra_headers.insert("x-compaction-at".to_string(), value.to_string());
            }
        }
        SamplingConfig {
            api_key: creds.api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            max_completion_tokens: cfg.max_completion_tokens,
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            api_backend: cfg.api_backend,
            auth_scheme,
            anthropic_oauth,
            github_copilot,
            openai_codex,
            chat_compat: cfg.chat_compat,
            extra_headers,
            context_window: cfg.context_window.get(),
            reasoning_effort: cfg.reasoning_effort,
            force_http1: false,
            max_retries: Some(self.max_retries),
            stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
            idle_timeout_secs: None,
            origin_client: self.origin_client.clone(),
            attribution_callback: self.attribution_callback.clone(),
            bearer_resolver: inference_auth_manager.map(auth_manager_bearer_resolver),
            supports_backend_search: self.supports_backend_search.get(),
            compactions_remaining: self.compactions_remaining.get(),
            compaction_at_tokens: self.compaction_at_tokens.get(),
            doom_loop_recovery: self.doom_loop_recovery,
            header_injector: Some(std::sync::Arc::new(TraceContextInjector)),
        }
    }
    /// Install auto-mode permission classifier with a live LLM side-query
    /// (laziness-classifier pattern: `prepare_chat_completion` +
    /// `conversation_collect` on a LocalSet task; channel bridges the
    /// `Send` permission actor). Heuristic runs only when the side-query
    /// errors or returns unparseable text.
    pub(crate) async fn wire_permission_auto_llm_classifier(self: &Arc<Self>) {
        if !self.permissions.is_auto_mode() {
            return;
        }
        if self.permissions.has_llm_side_query() {
            return;
        }
        let auto_cfg = crate::util::config::resolve_auto_mode_config_from_disk();
        let session_model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let aux_classifier_sampler = match auto_cfg.classifier_model.as_deref() {
            Some(slug) => self.resolve_auto_classifier_sampler(slug).await,
            None => None,
        };
        let models = self.models_manager.models();
        let effective_supports_re = crate::agent::config::effective_classifier_supports_re(
            aux_classifier_sampler
                .as_ref()
                .map(|(_, model)| model.as_str()),
            &session_model,
            &models,
        );
        let (prompt_type, classifier_reasoning_effort) =
            crate::util::config::auto_mode_classifier_defaults(&auto_cfg, effective_supports_re);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
            Vec<kigi_workspace::permission::ClassifierMessage>,
            tokio::sync::oneshot::Sender<Result<String, String>>,
        )>();
        let session = Arc::clone(self);
        tokio::task::spawn_local(async move {
            const TIMEOUT_MS: u64 = 15_000;
            while let Some((messages, respond_to)) = rx.recv().await {
                let result = async {
                    let (sampling_client, model) = match &aux_classifier_sampler {
                        Some((client, model)) => (client.clone(), model.clone()),
                        None => {
                            let client = session
                                .prepare_chat_completion(false)
                                .await
                                .map_err(|e| e.to_string())?;
                            let model = session
                                .chat_state_handle
                                .get_sampling_config()
                                .await
                                .map(|c| c.model)
                                .unwrap_or_default();
                            (client, model)
                        }
                    };
                    let session_id = session.session_info.id.to_string();
                    let items = messages
                        .into_iter()
                        .map(|m| match m.role {
                            kigi_workspace::permission::ClassifierMessageRole::System => {
                                ConversationItem::system(m.text)
                            }
                            kigi_workspace::permission::ClassifierMessageRole::User => {
                                ConversationItem::user(m.text)
                            }
                        })
                        .collect::<Vec<_>>();
                    let request = ConversationRequest {
                        items,
                        tools: vec![],
                        hosted_tools: vec![],
                        tool_choice: None,
                        model: Some(model),
                        temperature: None,
                        max_output_tokens: None,
                        json_schema: Some(
                            kigi_workspace::permission::classifier_output_json_schema(),
                        ),
                        reasoning_effort: classifier_reasoning_effort,
                        x_kigi_conv_id: Some(session_id.clone()),
                        x_kigi_req_id: Some(format!("xai-perm-auto-{}", uuid::Uuid::new_v4())),
                        x_kigi_session_id: Some(session_id),
                        x_kigi_agent_id: Some(crate::util::agent_id::agent_id()),
                        ..ConversationRequest::default()
                    };
                    let fut = sampling_client.conversation_collect(request);
                    let response =
                        tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MS), fut)
                            .await
                            .map_err(|_| "permission auto classifier timed out".to_string())?
                            .map_err(|e| e.to_string())?;
                    Ok(response.assistant_text())
                }
                .await;
                let _ = respond_to.send(result);
            }
        });
        let clf =
            kigi_workspace::permission::LlmPermissionClassifier::with_channel(tx, prompt_type);
        debug_assert!(
            clf.has_side_query(),
            "channel-wired classifier must report has_side_query"
        );
        self.permissions.set_classifier_with_side_query(clf, true);
        tracing::info!(
            session_id = % self.session_info.id,
            "Wired live LLM permission auto-mode classifier (session sampling channel)"
        );
    }
    /// Resolve a standalone aux-model `SamplerConfig` for `slug` via the shared
    /// catalog routing (Tier-1 catalog creds / Tier-2 xAI-proxy via session token
    /// / `XAI_API_KEY` / deployment key), gathering the session-local auth context
    /// once. Shared by image-describe and the classifier so the gather can't
    /// drift. `None` ⇒ caller falls back to the session model.
    pub(super) async fn resolve_aux_sampler_config(
        &self,
        slug: &str,
    ) -> Option<kigi_sampler::SamplerConfig> {
        let creds = self.chat_state_handle.get_credentials().await;
        let models = self.models_manager.models();
        // Resolve the aux token by the aux model's OWN platform AND endpoint: a
        // grok (oauth-platform) aux model draws its pooled grok token or `None`,
        // and an API-key registry platform draws NOTHING — NEVER the primary
        // Kimi session token (which `resolve_credentials` would otherwise stamp
        // onto an api.x.ai / api.deepseek.com request). The first-party
        // subscription channel still gets the primary (byte-identical).
        // The platform AND the base URL the rule is applied to both come from
        // `credential_for_slug`'s single resolution of `slug` against this
        // catalog, so the platform and the endpoint cannot disagree. Aux slugs
        // are not the session's selection, so no `current_key`.
        let session_key = self
            .credential_authority()
            .credential_for_slug(&models, None, slug);
        let endpoints = self.models_manager.endpoints();
        crate::agent::config::resolve_aux_model_sampling_config(
            slug,
            &models,
            &endpoints,
            session_key.as_ref(),
            creds.alpha_test_key.clone(),
        )
    }
    /// Resolve a dedicated sampler for the Auto-mode classifier model `slug`,
    /// stamping session-local auth/attribution like image-describe (which relies
    /// on the resolver, not a config override, for `base_url`/`api_backend` so
    /// credentials stay consistent). `None` ⇒ caller falls back to the session
    /// client + model.
    async fn resolve_auto_classifier_sampler(
        &self,
        slug: &str,
    ) -> Option<(kigi_sampler::SamplingClient, String)> {
        let active_session_config = self.reconstruct_full_config().await;
        let mut cfg = self.resolve_aux_sampler_config(slug).await?;
        // LEAK 1b: the aux classifier must NOT inherit the SESSION model's
        // (Kimi) bearer_resolver — the resolver is decided by the AUX model's
        // own platform + endpoint at the chokepoint and passed in explicitly,
        // so there is no "copy then remember to re-point" step to forget.
        let aux_resolver = self.aux_bearer_resolver(slug, &cfg.base_url);
        crate::agent::config::stamp_session_local_sampler_fields(
            &mut cfg,
            &active_session_config,
            aux_resolver,
            Some(self.max_retries),
        );
        let model = cfg.model.clone();
        let client = kigi_sampler::SamplingClient::new(cfg)
            .map_err(|e| {
                tracing::warn!(
                    error = % e,
                    "auto classifier aux sampler build failed; using session model"
                )
            })
            .ok()?;
        Some((client, model))
    }
    #[tracing::instrument(
        name = "session.prepare_chat_completion",
        skip_all,
        fields(force_http1)
    )]
    pub(super) async fn prepare_chat_completion(
        &self,
        force_http1: bool,
    ) -> Result<kigi_sampler::SamplingClient, acp::Error> {
        self.refresh_token_if_expired().await;
        let mut full_config = self.reconstruct_full_config().await;
        full_config.force_http1 = force_http1;
        let sampling_client =
            kigi_sampler::SamplingClient::new(full_config).map_err(|e| self.to_acp_error(e))?;
        Ok(sampling_client)
    }
    /// Push a fresh `SamplerConfig` into the per-session sampler actor
    /// before each turn. Mirrors `prepare_chat_completion`'s
    /// auth-refresh + config rebuild, but routes the result to the
    /// `kigi-sampler` instead of constructing a new
    /// `OaiCompatClient`.
    ///
    /// Behaviour parity: we run the same `refresh_token_if_expired()`
    /// and `reconstruct_full_config()` so the sampler picks up any
    /// newly issued session token. The previous client cache inside
    /// the sampler actor is invalidated automatically by
    /// `update_config`.
    pub(crate) async fn prepare_sampler_for_turn(&self) {
        self.refresh_token_if_expired().await;
        let mut sampler_config = self.reconstruct_full_config().await;
        sampler_config.idle_timeout_secs = Some(self.inference_idle_timeout.as_secs());
        self.sampler_handle.update_config(sampler_config);
    }
    fn log_terminal_failure(&self, error_type: &str, status_code: Option<u16>, message: &str) {
        let auth = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired());
        let reauthable = is_reauthable_failure(Some(error_type), message);
        kigi_log::unified_log::warn(
            "turn.terminal_failure",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!(
                { "error_type" : error_type, "status_code" : status_code,
                "reauthable" : reauthable, "auth_mode" : auth.as_ref().map(| a |
                format!("{:?}", a.auth_mode)), "key_prefix" : auth.as_ref().map(| a |
                crate ::auth::token_suffix(& a.key).to_owned()), "expires_at" : auth
                .as_ref().and_then(| a | a.expires_at.map(| e | e.to_rfc3339())),
                "message" : crate ::util::truncate(message, 300), }
            )),
        );
    }
    pub(crate) async fn handle_sampling_failure(
        self: &Arc<Self>,
        error: kigi_sampler::SamplingErrorInfo,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        use kigi_sampler::SamplingErrorKind;
        if self.should_compact_on_error(&error).await {
            let cw = error
                .model_metadata
                .as_ref()
                .and_then(|m| m.context_window)
                .expect("should_compact_on_error guarantees context_window");
            {
                let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
                let percentage = kigi_token_estimation::usage_percentage_u8(total_tokens, cw);
                if let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await
                    && let Some(new_cw) = std::num::NonZeroU64::new(cw)
                    && self.compaction.context_window_override.is_none()
                {
                    cfg.context_window = new_cw;
                    self.chat_state_handle.update_sampling_config(cfg);
                }
                let trigger_info = compaction::AutoCompactTriggerInfo {
                    tokens_used: total_tokens,
                    context_window: cw,
                    percentage,
                };
                self.run_compact_only(trigger_info).await?;
                return Ok(SamplerFailureRecovery::CompactAndResubmit);
            }
        }
        let detailed_message = error.message.clone();
        if matches!(error.kind, SamplingErrorKind::Api)
            && error.status_code == Some(400)
            && error.message.contains("encrypted_content")
        {
            self.signals_handle()
                .record_error_typed("encrypted_content_mismatch");
            let friendly = "This session's conversation history is incompatible \
                            with the current model. Please start a new session."
                .to_string();
            self.log_terminal_failure("encrypted_content_mismatch", error.status_code, &friendly);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "encrypted_content_mismatch".to_string(),
                    message: friendly.clone(),
                },
            ))
            .await;
            return Err(acp::Error::invalid_params().data(friendly));
        }
        if matches!(error.kind, SamplingErrorKind::RateLimited) {
            self.log_terminal_failure("rate_limited", error.status_code, &detailed_message);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Exhausted {
                    attempts: 0,
                    reason: detailed_message.clone(),
                    is_rate_limited: true,
                },
            ))
            .await;
            let acp_err = acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "Rate limited".to_string(),
            )
            .data(detailed_message);
            return Err(acp_err);
        }
        let auth_recovery_eligible = matches!(error.kind, SamplingErrorKind::Auth) && {
            let (model_id, base_url) = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| (c.model, c.base_url))
                .unwrap_or_default();
            let gate = self.auth_gate(&model_id, &base_url);
            let eligible = gate.active();
            self.log_auth_gate_unknown("handle_sampling_failure", gate, &base_url);
            if !eligible {
                tracing::warn!(
                    session_id = % self.session_info.id.0, is_session_based = gate
                    .is_session_based, model_byok = gate.model_byok.as_str(),
                    endpoint_is_first_party = gate.endpoint_is_first_party,
                    credential_class = gate.credential_class.as_str(),
                    "auth recovery: sampler 401 not refreshable (api-key auth) — surfacing 401",
                );
                kigi_log::unified_log::warn(
                    "auth recovery: sampler 401 not eligible (api-key auth)",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!(
                        { "kind" : error.kind.as_str(), "status_code" : error
                        .status_code, "is_session_based" : gate.is_session_based,
                        "model_byok" : gate.model_byok.as_str(),
                        "endpoint_is_first_party" : gate.endpoint_is_first_party,
                        "credential_class" : gate.credential_class.as_str(), }
                    )),
                );
            }
            eligible
        };
        if !matches!(error.kind, SamplingErrorKind::Auth) && error.status_code == Some(401) {
            kigi_log::unified_log::warn(
                "auth recovery: sampler 401 not eligible (non-auth error kind)",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!(
                    { "kind" : error.kind.as_str(), "status_code" : error
                    .status_code, }
                )),
            );
        }
        // Recover via the ACTIVE model's OWN manager: a grok 401 recovers the
        // xai-grok session via the xai-grok manager, never the Kimi one. For a
        // Kimi / non-oauth model this resolves to the primary — byte-identical.
        if auth_recovery_eligible {
            let (recovery_model, recovery_base_url) = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| (c.model, c.base_url))
                .unwrap_or_default();
            if let Some(am) = self.auth_manager_for_endpoint(&recovery_model, &recovery_base_url) {
                if am.try_recover_unauthorized().await {
                    tracing::info!(
                        session_id = % self.session_info.id.0,
                        "auth recovery: sampler 401, recovered, retrying"
                    );
                    kigi_log::unified_log::info(
                        "auth recovery: sampler 401, recovered, retrying",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                    self.prepare_sampler_for_turn().await;
                    return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit);
                }
                tracing::warn!(
                    session_id = % self.session_info.id.0,
                    "auth recovery: sampler 401, refresh failed"
                );
                kigi_log::unified_log::warn(
                    "auth recovery: sampler 401, refresh failed",
                    Some(self.session_info.id.0.as_ref()),
                    None,
                );
            }
        }
        if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
            self.signals_handle().record_idle_timeout();
        }
        if matches!(error.kind, SamplingErrorKind::EmptyResponse) {
            if let Some(ref ctx) = error.empty_response_context {
                tracing::warn!(
                    empty_response = true, empty_reason = ctx.reason.as_str(),
                    had_reasoning = ctx.had_reasoning, content_len = ctx.content_len,
                    tool_call_count = ctx.tool_call_count, completion_tokens = ctx
                    .completion_tokens.unwrap_or(0), reasoning_tokens = ctx
                    .reasoning_tokens.unwrap_or(0), finish_reason = ctx
                    .finish_reason_str(), first_choice_seen = ctx.first_choice_seen,
                    model = % ctx.model,
                    "empty response after retries exhausted: {reason}", reason = ctx
                    .reason,
                );
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    cap.reasoning_tokens = ctx.reasoning_tokens;
                    cap.completion_tokens = ctx.completion_tokens;
                    cap.finish_reason = ctx.finish_reason.clone();
                    cap.empty_reason = Some(ctx.reason.as_str().to_owned());
                }
            }
            self.signals_handle().record_error_typed("empty_response");
        }
        let auth_mode = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current())
            .map(|a| a.auth_mode)
            .unwrap_or(crate::auth::AuthMode::ApiKey);
        let auth_mode_str = format!("{auth_mode:?}");
        let client_version = kigi_version::VERSION;
        let is_model_404 =
            error.status_code == Some(404) && detailed_message.contains("does not exist");
        let is_auth_401 =
            error.status_code == Some(401) || matches!(error.kind, SamplingErrorKind::Auth);
        let detailed_message = if is_model_404 || is_auth_401 {
            let current_model = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_else(|| "unknown".to_string());
            let available: Vec<String> = self
                .models_manager
                .models()
                .values()
                .map(|m| m.model.clone())
                .collect();
            let mut msg = format!("{detailed_message}\n");
            msg.push_str(&format!("\n  Model:     {current_model}"));
            msg.push_str(&format!("\n  Auth:      {auth_mode_str}"));
            msg.push_str(&format!("\n  Version:   {client_version}"));
            if available.is_empty() {
                msg.push_str("\n  Available: (none)");
            } else {
                msg.push_str(&format!("\n  Available: {}", available.join(", ")));
            }
            if is_model_404 && !available.iter().any(|m| m == &current_model) {
                msg.push_str(&format!(
                    "\n\n  '{}' is not in your available models.",
                    current_model
                ));
                msg.push_str("\n  Switch models with /model or start a new session.");
            }
            msg
        } else {
            detailed_message
        };
        let error_type = if kigi_sampling_types::is_context_length_error(&error.message) {
            "context_length"
        } else {
            error.kind.as_str()
        };
        self.log_terminal_failure(error_type, error.status_code, &detailed_message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_string(),
                message: detailed_message.clone(),
            },
        ))
        .await;
        Err(
            acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
                detailed_message,
                error.status_code,
                error.kind,
            )),
        )
    }
    /// Drive a single turn through the sampler-based path.
    ///
    /// Calls `prepare_sampler_for_turn` first (auth refresh + config
    /// push), then submits via `SamplerHandle::submit_and_collect` and
    /// returns:
    /// * `Ok(SamplerTurnOutcome::Response(_))` - model responded.
    /// * `Ok(SamplerTurnOutcome::CompactAndResubmit)` - compaction
    ///    ran, the outer turn loop should `continue`.
    /// * `Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)` - auth 401
    ///    recovery succeeded, credentials refreshed, retry once.
    /// * `Err(acp::Error)` - terminal failure already reported via
    ///    `send_xai_notification(RetryState::Failed)`.
    pub(crate) async fn run_turn_via_sampler(
        self: &Arc<Self>,
        request: ConversationRequest,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        self.prepare_sampler_for_turn().await;
        let stream_drained_rx = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *self.turn_stream_drained.lock() = Some(tx);
            rx
        };
        let request_id = kigi_sampler::RequestId::random();
        let request_id_str = request_id.as_str().to_string();
        match self
            .sampler_handle
            .submit_and_collect(request_id, request)
            .await
        {
            Ok((response, metrics)) => {
                let span = tracing::Span::current();
                span.record("request_id", request_id_str.as_str());
                if let Some(ttft) = metrics.time_to_first_token_ms {
                    span.record("ttft_ms", ttft as i64);
                }
                if metrics.attempts > 0 {
                    span.record("attempt", i64::from(metrics.attempts));
                }
                if tokio::time::timeout(std::time::Duration::from_secs(5), stream_drained_rx)
                    .await
                    .is_err()
                {
                    self.turn_stream_drained.lock().take();
                    tracing::warn!(
                        "stream-drain barrier timed out; proceeding to emit tool \
                         calls (eventId ordering may be imperfect this turn)"
                    );
                }
                Ok(SamplerTurnOutcome::Response(
                    Box::new(response),
                    Box::new(metrics),
                ))
            }
            Err(rich_err) => {
                self.turn_stream_drained.lock().take();
                let info = kigi_sampler::SamplingErrorInfo::from(&rich_err);
                match self.handle_sampling_failure(info).await? {
                    SamplerFailureRecovery::CompactAndResubmit => {
                        Ok(SamplerTurnOutcome::CompactAndResubmit)
                    }
                    SamplerFailureRecovery::RefreshAuthAndResubmit => {
                        Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)
                    }
                }
            }
        }
    }
    pub(super) async fn refresh_token_if_expired(&self) {
        let (model_id, base_url) = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| (c.model, c.base_url))
            .unwrap_or_default();
        // Refresh the ACTIVE model's OWN manager: a grok model refreshes the
        // xai-grok token via the xai-grok manager, never the Kimi one. For a
        // Kimi / non-oauth model this resolves to the primary — byte-identical.
        if let Some(am) = self.auth_manager_for_endpoint(&model_id, &base_url) {
            let creds = self.chat_state_handle.get_credentials().await;
            if self.auth_gate(&model_id, &base_url).active()
                && let Ok(key) = am.get_valid_token().await
            {
                if creds.api_key.as_deref() != Some(&key) {
                    let mut creds = creds;
                    creds.api_key = Some(key);
                    self.chat_state_handle.update_credentials(creds);
                }
                return;
            }
        } else {
            kigi_log::unified_log::debug(
                "token refresh skipped: no auth manager",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        // BYOK path: pick up an externally rotated per-model key from
        // config.toml. Kimi bearers are opaque (no client-side expiry
        // probing); a changed on-disk key is adopted, an unchanged one is a
        // no-op.
        let creds = self.chat_state_handle.get_credentials().await;
        let current_key = creds.api_key;
        let current_model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let Some(ref key) = current_key else { return };
        // A registry-platform model's key normally comes from that platform's
        // credential resolved into its catalog entry; the session gate is
        // inactive for every API-key platform, so those turns reach here.
        // Falling through would pay a `load_effective_config()` disk read PER
        // TURN and log a permanently false "Model not found in config.toml
        // [model.*]" warning.
        //
        // But a `[model."deepseek/deepseek-chat"]` override DOES keep the base
        // entry's `info.id` (`ConfigModelOverride::apply`), so "has a platform"
        // does NOT imply "has no `[model.*]` block" — skipping on the platform
        // alone would freeze an on-disk key rotation for the whole session.
        // Skip only when the catalog entry carries no own credential at all,
        // which is exactly the "key came from the platform, not from config"
        // case the disk read cannot improve on.
        if self.model_platform(&current_model_id).is_some()
            && !self.model_has_own_credential(&current_model_id)
        {
            return;
        }
        let Some(new_key) = self.reload_api_key_from_config(&current_model_id) else {
            return;
        };
        if key == &new_key {
            return;
        }
        tracing::info!(
            model = % current_model_id, key_len = new_key.len(),
            "Refreshed API token from config.toml"
        );
        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }
    /// Whether the live catalog entry for `slug` carries its own credential —
    /// an `api_key`/`env_key` from a `[model.*]` block, which a config edit can
    /// rotate mid-session. A platform entry whose key came from the platform
    /// credential has none.
    fn model_has_own_credential(&self, slug: &str) -> bool {
        let models = self.models_manager.models();
        crate::agent::config::find_model_by_id(&models, slug)
            .is_some_and(crate::agent::config::ModelEntry::has_own_credentials)
    }
    fn reload_api_key_from_config(&self, current_model_id: &str) -> Option<String> {
        let raw_config = crate::config::load_effective_config()
            .map_err(|e| tracing::warn!(error = % e, "Failed to reload config"))
            .ok()?;
        let config = crate::agent::config::Config::new_from_toml_cfg(&raw_config)
            .map_err(|e| tracing::warn!(error = % e, "Failed to parse reloaded config.toml"))
            .ok()?;
        let config_model = config
            .config_models
            .iter()
            .find(|(k, v)| v.model.as_deref().unwrap_or(k.as_str()) == current_model_id)
            .map(|(_, v)| v);
        let Some(model) = config_model else {
            tracing::warn!(
                model = % current_model_id, available = ? config.config_models.keys()
                .collect::< Vec < _ >> (), "Model not found in config.toml [model.*]"
            );
            return None;
        };
        let key = crate::agent::config::first_own_credential(
            model.api_key.as_deref(),
            model.env_key.as_ref(),
        );
        if key.is_none() {
            tracing::warn!(
                model = % current_model_id, env_key = ? model.env_key,
                "No api_key or env_key resolved for model"
            );
        }
        key
    }
    /// Propagate the model-reported token usage from a turn response into
    /// chat state, the per-prompt usage ledger, and per-turn signals.
    ///
    /// This is the only place per-turn `total_tokens` is refreshed in the
    /// post-sampler-refactor path; without it `state.total_tokens` would
    /// stay frozen at the `estimate_conversation_tokens` seed from
    /// `ChatState::new`, freezing `/context` and corrupting the resume
    /// restore that reads `meta.totalTokens` from `updates.jsonl`.
    /// Resetting `estimated_tokens_since_model = 0` here also keeps the
    /// preflight-overflow guard accurate against the next turn's
    /// tool-result deltas.
    pub(crate) fn record_response_token_usage(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
    ) {
        if let Some(ref u) = response.usage {
            self.chat_state_handle
                .record_token_usage(u64::from(u.total_tokens));
            self.chat_state_handle.record_last_turn_usage(u.clone());
            self.chat_state_handle.record_model_call_usage(
                response.assistant().and_then(|a| a.model_id.clone()),
                u.clone(),
                api_duration_ms,
                response.cost_usd_ticks,
            );
            self.signals_handle()
                .record_token_usage(u.completion_tokens, u.reasoning_tokens);
        }
    }
    pub(super) async fn record_assistant_response(&self, assistant_item: ConversationItem) {
        self.signals_handle().record_assistant_message();
        if let ConversationItem::Assistant(ref a) = assistant_item {
            tracing::info!(
                model_id = ? a.model_id, "DEBUG record_assistant_response model_id"
            );
        }
        if let ConversationItem::Assistant(ref a) = assistant_item
            && let Some(first_call) = a.tool_calls.first()
        {
            tracing::info!("Assistant requested tool call: {}", first_call.id);
        }
        self.chat_state_handle
            .push_assistant_response(assistant_item);
    }
}
#[cfg(test)]
mod bearer_resolver_tests {
    use super::AuthManagerBearerResolver;
    use kigi_sampler::BearerResolver;

    /// LEAK 1b: the shared `AuthManagerBearerResolver` resolves the LIVE bearer
    /// of the manager it wraps. So an aux bearer_resolver built over grok's OWN
    /// (oauth) pooled manager yields grok's token (or `None`) — NEVER the Kimi
    /// session token that a Kimi-manager resolver would. The
    /// [`CredentialAuthority::bearer_resolver_for`](crate::auth::credential_authority::CredentialAuthority::bearer_resolver_for)
    /// wraps exactly this grok manager for a grok aux model.
    #[tokio::test]
    async fn resolver_resolves_the_wrapped_manager_never_kimi() {
        let dir = tempfile::tempdir().unwrap();
        let kimi = std::sync::Arc::new(crate::auth::AuthManager::new(
            dir.path(),
            crate::auth::KimiCodeConfig::default(),
        ));
        kimi.hot_swap(crate::auth::KimiAuth {
            key: "kimi-tok".to_string(),
            auth_mode: crate::auth::AuthMode::OAuth,
            ..crate::auth::KimiAuth::test_default()
        });
        // The Kimi-manager resolver yields the Kimi bearer.
        assert_eq!(
            AuthManagerBearerResolver(kimi.clone()).current_bearer(),
            Some("kimi-tok".to_string()),
        );
        // The grok (oauth) pooled manager is distinct — its resolver never
        // yields the Kimi bearer (grok's own token, or None).
        let oauth = kigi_models::PlatformId::XaiGrok
            .oauth()
            .expect("xai-grok carries an OAuthConfig");
        let grok = crate::auth::oauth_registry::global_manager_for(
            &crate::auth::oauth_registry::pool_home(),
            oauth,
        );
        assert_ne!(
            AuthManagerBearerResolver(grok).current_bearer(),
            Some("kimi-tok".to_string()),
            "a grok aux bearer_resolver must never resolve the Kimi session token",
        );
    }
}
