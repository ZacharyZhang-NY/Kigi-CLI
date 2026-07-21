//! Model catalog fetch (PRD F4).
//!
//! Fetches the model catalog with `GET {base}/models` per enabled platform
//! (the subscription platform via the OAuth session, the open platforms via
//! their API keys), plus the custom-endpoint OpenAI-compatible listing path.
//!
//! This is the network surface relocated out of the deleted xAI-proxy
//! backend client (`remote/`); it talks only to the configured platform
//! model endpoints (plus the models.dev metadata refresh when an enabled
//! platform needs enrichment — see `enrichment_fetch`), never to a proxy
//! backend.
use crate::auth::KimiAuth;
use indexmap::IndexMap;
use serde::Deserialize;

/// Errors from a model-catalog fetch.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BackendError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Request failed: {status} - {body}")]
    RequestFailed { status: u16, body: String },
    #[error("Auth error: {0}")]
    Auth(String),
}
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 256_000;
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<serde_json::Value>,
}
/// The models-fetch origin key for this endpoints/auth shape. Used as the
/// models disk-cache origin: cached entries embed absolute `base_url`s from
/// the backend(s) that served them, so a catalog fetched against one fetch
/// plan (env override, different set of platform credentials, a test's mock
/// server) must be a cache miss for any other. Encodes URLs and enabled
/// platform NAMES only — never credential values.
pub(crate) fn models_fetch_origin(
    endpoints: &crate::agent::config::EndpointsConfig,
    fetch_auth: crate::agent::models::ModelFetchAuth,
    has_oauth: bool,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> String {
    match fetch_auth {
        crate::agent::models::ModelFetchAuth::CustomEndpoint => endpoints.resolve_models_list_url(),
        crate::agent::models::ModelFetchAuth::Platforms => {
            let parts: Vec<String> = enabled_platforms(has_oauth, platform_keys)
                .into_iter()
                .map(|p| format!("{}={}", p.as_str(), platform_models_url(p, endpoints)))
                .collect();
            format!("platforms[{}]", parts.join(";"))
        }
    }
}
/// The platforms with usable credentials, in registry order (kimi-code first
/// so "default model = first list item" favors the subscription).
fn enabled_platforms(
    has_oauth: bool,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Vec<kigi_models::PlatformId> {
    kigi_models::PlatformId::ALL
        .into_iter()
        .filter(|p| {
            if p.uses_oauth() {
                has_oauth
            } else {
                platform_keys.key_for(*p).is_some()
            }
        })
        .collect()
}
/// `{base}/models` for one platform. The subscription platform resolves its
/// base through the endpoints config (`coding_api_base_url` override,
/// else `KIGI_CODE_BASE_URL` / production default via kigi-env); the open
/// platforms use their fixed bases.
fn platform_models_url(
    platform: kigi_models::PlatformId,
    endpoints: &crate::agent::config::EndpointsConfig,
) -> String {
    let base = if platform.uses_oauth() {
        endpoints.proxy_url()
    } else {
        platform.base_url()
    };
    format!("{}/models", base.trim_end_matches('/'))
}
/// Fetch result: model entries + optional etag from the subscription platform.
pub struct FetchModelsResult {
    pub models: Vec<crate::agent::config::ModelEntryConfig>,
    pub etag: Option<String>,
    /// The OAuth platform answered 401. The async layer forces a token
    /// refresh and retries once (port of kimi-cli `refresh_managed_models`).
    pub oauth_unauthorized: bool,
}
/// Fetch the model catalog (PRD F4).
///
/// - Custom endpoint mode (`KIGI_MODELS_BASE_URL` / `models_list_url`): a
///   single OpenAI-compatible listing fetched with the BYOK key or session
///   bearer, parsed leniently ([`parse_remote_model_value`]).
/// - Otherwise, the fixed platform registry: `GET {base}/models` with
///   `Authorization: Bearer <oauth-token or api-key>` per enabled platform,
///   parsed per the F4 wire contract with capability derivation and the
///   `kimi-k` prefix filter for the open platforms.
///
/// Succeeds when at least one platform delivers; per-platform failures are
/// logged (status codes only, never credentials).
pub(crate) fn fetch_models_blocking(
    endpoints: &crate::agent::config::EndpointsConfig,
    auth: Option<&KimiAuth>,
    fetch_auth: crate::agent::models::ModelFetchAuth,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Result<FetchModelsResult, BackendError> {
    match fetch_auth {
        crate::agent::models::ModelFetchAuth::CustomEndpoint => {
            fetch_custom_endpoint_models_blocking(endpoints, auth)
        }
        crate::agent::models::ModelFetchAuth::Platforms => {
            fetch_platform_models_blocking(endpoints, auth, platform_keys)
        }
    }
}
fn fetch_custom_endpoint_models_blocking(
    endpoints: &crate::agent::config::EndpointsConfig,
    auth: Option<&KimiAuth>,
) -> Result<FetchModelsResult, BackendError> {
    let client = crate::http::shared_blocking_client();
    let url = endpoints.resolve_models_list_url();
    let inference_base_url = endpoints.resolve_inference_base_url();
    tracing::info!("Fetching models from custom endpoint {}", url);
    let api_key = crate::agent::auth_method::read_xai_api_key_env()
        .or_else(|_| {
            auth.map(|a| a.key.clone())
                .ok_or(std::env::VarError::NotPresent)
        })
        .map_err(|_| {
            BackendError::Auth("No API key for custom models endpoint. Set XAI_API_KEY.".into())
        })?;
    let request = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key));
    let response = request.send()?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        tracing::warn!("Failed to fetch models: {} - {}", status, body);
        return Err(BackendError::RequestFailed { status, body });
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let models_response: ModelsResponse = response.json()?;
    tracing::info!("Fetched {} models from {}", models_response.data.len(), url);
    let mut models = Vec::with_capacity(models_response.data.len());
    for (idx, value) in models_response.data.into_iter().enumerate() {
        match parse_remote_model_value(&value, &inference_base_url) {
            Some(model) => models.push(model),
            None => {
                tracing::warn!(
                    "Skipping model at index {}: missing required field ('model' or 'context_window') or invalid types",
                    idx
                )
            }
        }
    }
    Ok(FetchModelsResult {
        models,
        etag,
        oauth_unauthorized: false,
    })
}
/// Registry fetch across all platforms with usable credentials.
fn fetch_platform_models_blocking(
    endpoints: &crate::agent::config::EndpointsConfig,
    auth: Option<&KimiAuth>,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Result<FetchModelsResult, BackendError> {
    let enabled = enabled_platforms(auth.is_some(), platform_keys);
    if enabled.is_empty() {
        return Err(BackendError::Auth(
            "No platform credentials: log in with `kigi login`, paste a platform API key in \
             the login screen (stored in ~/.kigi/auth.json), or set a platform env var such \
             as KIGI_MOONSHOT_API_KEY."
                .into(),
        ));
    }

    let mut models = Vec::new();
    let mut etag = None;
    let mut oauth_unauthorized = false;
    let mut successes = 0usize;
    let mut last_error: Option<BackendError> = None;
    // Loaded once per fetch pass; zero IO while every enabled platform
    // serves its own metadata (kimi/moonshot today).
    let enrichment = crate::agent::enrichment_fetch::load_enrichment_catalog(&enabled);
    for platform in &enabled {
        let bearer = if platform.uses_oauth() {
            auth.map(|a| a.key.clone())
                .expect("enabled_platforms gated on auth presence")
        } else {
            platform_keys
                .key_for(*platform)
                .expect("enabled_platforms gated on key presence")
                .to_owned()
        };
        match fetch_one_platform_models(*platform, endpoints, &bearer, &enrichment) {
            Ok((platform_models, platform_etag)) => {
                tracing::info!(
                    platform = platform.as_str(),
                    count = platform_models.len(),
                    "platform models fetch succeeded"
                );
                successes += 1;
                if platform.uses_oauth() {
                    etag = platform_etag;
                }
                models.extend(platform_models);
            }
            Err(e) => {
                if platform.uses_oauth()
                    && matches!(&e, BackendError::RequestFailed { status: 401, .. })
                {
                    oauth_unauthorized = true;
                }
                tracing::warn!(
                    platform = platform.as_str(),
                    error = %e,
                    "platform models fetch failed"
                );
                last_error = Some(e);
            }
        }
    }

    if successes == 0 {
        // All enabled platforms failed. When the failure includes an OAuth
        // 401, return `Ok` with the flag set (and no models) so the async
        // layer can force a token refresh and retry — an `Err` would drop
        // the signal. Non-401 failures propagate as the last error.
        if oauth_unauthorized {
            return Ok(FetchModelsResult {
                models: Vec::new(),
                etag: None,
                oauth_unauthorized: true,
            });
        }
        return Err(last_error.unwrap_or_else(|| {
            BackendError::Auth("no platform models fetch was attempted".into())
        }));
    }
    Ok(FetchModelsResult {
        models,
        etag,
        oauth_unauthorized,
    })
}
/// `GET {base}/models` for one platform (PRD F4 wire contract):
/// `Authorization: Bearer <token>` → `{data:[{id, context_length,
/// supports_reasoning, supports_image_in, supports_video_in, display_name?}]}`.
/// Applies the platform's `kimi-k` prefix filter and capability derivation,
/// and keys each entry `{platform_id}/{model_id}`.
fn fetch_one_platform_models(
    platform: kigi_models::PlatformId,
    endpoints: &crate::agent::config::EndpointsConfig,
    bearer: &str,
    enrichment: &kigi_models::enrichment::EnrichmentCatalog,
) -> Result<(Vec<crate::agent::config::ModelEntryConfig>, Option<String>), BackendError> {
    let client = crate::http::shared_blocking_client();
    let url = platform_models_url(platform, endpoints);
    tracing::info!(platform = platform.as_str(), url = %url, "fetching platform models");
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", bearer))
        .send()?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().unwrap_or_default();
        return Err(BackendError::RequestFailed { status, body });
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let listing: kigi_models::WireModelsResponse = response.json()?;
    let total = listing.data.len();
    let filtered = kigi_models::filter_allowed_models(platform, listing.data);
    if filtered.len() != total {
        tracing::info!(
            platform = platform.as_str(),
            total,
            kept = filtered.len(),
            "applied platform model-prefix filter"
        );
    }
    let base_url = if platform.uses_oauth() {
        endpoints.proxy_url()
    } else {
        platform.base_url()
    };
    let models = filtered
        .into_iter()
        .map(|mut wire| {
            // Metadata-poor listings (bare ids) get context window / thinking
            // levels from the models.dev catalog; wire-served platforms skip
            // this entirely and wire values always win (enrich_wire_model).
            if !platform.wire_serves_metadata()
                && let Some(dev_id) = platform.models_dev_id()
            {
                match kigi_models::enrichment::lookup(enrichment, dev_id, &wire.id) {
                    Some(meta) => kigi_models::enrichment::enrich_wire_model(&mut wire, meta),
                    None => tracing::debug!(
                        platform = platform.as_str(), model = %wire.id,
                        "no enrichment entry; defaults will apply"
                    ),
                }
            }
            platform_wire_model_to_entry(platform, wire, &base_url)
        })
        .collect();
    Ok((models, etag))
}
/// Map one F4 wire model to a catalog entry config.
///
/// SECURITY: the entry carries only env-var NAMES (`env_key`) for the open
/// platforms — never key values — because raw fetched entries are persisted
/// to the models disk cache. Config-file keys are stamped in-memory later by
/// `resolve_model_list`'s platform-credentials layer.
/// Map a live `think_efforts` block to catalog effort options. The wire
/// token stays the option id/label (`"max"` → label `"Max"`) so the UI
/// mirrors the server's vocabulary, while the canonical value maps through
/// the [`kigi_sampling_types::ReasoningEffort`] parser (`"max"` → `Xhigh`).
/// Unknown tokens are dropped with a warning rather than inventing a level.
fn think_efforts_to_options(
    think: &kigi_models::WireThinkEfforts,
) -> Vec<kigi_sampling_types::ReasoningEffortOption> {
    think
        .valid_efforts
        .iter()
        .filter_map(|token| {
            let value = match token.parse::<kigi_sampling_types::ReasoningEffort>() {
                Ok(v) => v,
                Err(error) => {
                    tracing::warn!(%token, %error, "unknown think_efforts token; dropping");
                    return None;
                }
            };
            let mut label: String = token.clone();
            if let Some(first) = label.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            Some(kigi_sampling_types::ReasoningEffortOption {
                id: token.clone(),
                value,
                label,
                description: None,
                default: think.default_effort.as_deref() == Some(token.as_str()),
            })
        })
        .collect()
}

pub(crate) fn platform_wire_model_to_entry(
    platform: kigi_models::PlatformId,
    wire: kigi_models::WireModel,
    base_url: &str,
) -> crate::agent::config::ModelEntryConfig {
    let capabilities = wire.capabilities();
    // Selectable thinking levels (live wire `think_efforts`, e.g. K3's
    // low/high/max). `support: false` or absence both mean "no levels".
    let think_efforts = wire.think_efforts.as_ref().filter(|t| t.support);
    let context_window = std::num::NonZeroU64::new(wire.context_length).unwrap_or_else(|| {
        tracing::debug!(
            model = %wire.id,
            default = DEFAULT_CONTEXT_WINDOW,
            "platform model missing context_length; using default"
        );
        std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW).expect("non-zero")
    });
    let env_key = (!platform.uses_oauth())
        .then(|| crate::agent::config::EnvKeys::new(platform.api_key_env_names().iter().copied()));
    crate::agent::config::ModelEntryConfig {
        id: Some(platform.managed_model_key(&wire.id)),
        name: Some(wire.display_name.clone().unwrap_or_else(|| wire.id.clone())),
        model: wire.id,
        base_url: base_url.to_owned(),
        description: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_key: None,
        env_key,
        api_backend: Default::default(),
        auth_scheme: None,
        reasoning_effort: think_efforts
            .and_then(|t| t.default_effort.as_deref())
            .and_then(|s| s.parse().ok()),
        supports_reasoning_effort: think_efforts.is_some(),
        reasoning_efforts: think_efforts
            .map(think_efforts_to_options)
            .unwrap_or_default(),
        capabilities,
        extra_headers: IndexMap::new(),
        context_window,
        auto_compact_threshold_percent: None,
        system_prompt_label: None,
        api_base_url: None,
        use_concise: false,
        agent_type: crate::agent::config::default_agent_type(),
        inference_idle_timeout_secs: None,
        max_retries: None,
        hidden: false,
        // Subscription models require the OAuth session; open-platform
        // models are usable by API-key users.
        supported_in_api: !platform.uses_oauth(),
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        show_model_fingerprint: false,
        stream_tool_calls: None,
        laziness_detector: Default::default(),
    }
}
/// Parse a single model entry from the /models response.
/// Used by both initial model fetch and session-resume metadata refresh.
pub fn parse_remote_model_value(
    value: &serde_json::Value,
    default_base_url: &str,
) -> Option<crate::agent::config::ModelEntryConfig> {
    let obj = value.as_object()?;
    let meta = obj.get("_meta").and_then(|v| v.as_object());
    let id = get_string(obj, "id");
    let model = get_string(obj, "model")
        .or_else(|| get_string(obj, "modelId"))
        .or_else(|| id.clone())
        .or_else(|| meta.and_then(|m| get_string(m, "model")))
        .or_else(|| meta.and_then(|m| get_string(m, "modelId")))?;
    let base_url = get_string(obj, "baseUrl")
        .or_else(|| get_string(obj, "base_url"))
        .unwrap_or_else(|| default_base_url.to_owned());
    let name = get_string(obj, "name").or_else(|| Some(model.clone()));
    let context_window = get_u64(obj, "contextWindow")
        .or_else(|| get_u64(obj, "context_window"))
        .or_else(|| meta.and_then(|m| get_u64(m, "contextWindow")))
        .or_else(|| meta.and_then(|m| get_u64(m, "totalContextTokens")))
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let context_window = std::num::NonZeroU64::new(context_window)?;
    let agent_type = get_string(obj, "systemPromptType")
        .or_else(|| get_string(obj, "system_prompt_type"))
        .or_else(|| get_string(obj, "agent_type"))
        .or_else(|| get_string(obj, "agentType"))
        .or_else(|| meta.and_then(|m| get_string(m, "agentType")))
        .or_else(|| meta.and_then(|m| get_string(m, "agent_type")))
        .unwrap_or_else(crate::agent::config::default_agent_type);
    let api_backend = get_string(obj, "apiBackend")
        .or_else(|| get_string(obj, "api_backend"))
        .and_then(|s| match s.as_str() {
            "responses" => Some(crate::sampling::ApiBackend::Responses),
            "chat_completions" => Some(crate::sampling::ApiBackend::ChatCompletions),
            "messages" => Some(crate::sampling::ApiBackend::Messages),
            _ => None,
        })
        .unwrap_or_default();
    Some(crate::agent::config::ModelEntryConfig {
        id,
        model,
        base_url,
        name,
        description: get_string(obj, "description"),
        max_completion_tokens: get_u64(obj, "maxCompletionTokens")
            .or_else(|| get_u64(obj, "max_completion_tokens"))
            .and_then(|v| u32::try_from(v).ok()),
        temperature: get_f64(obj, "temperature").map(|v| v as f32),
        top_p: get_f64(obj, "topP").or_else(|| get_f64(obj, "top_p")).map(|v| v as f32),
        api_key: get_string(obj, "apiKey").or_else(|| get_string(obj, "api_key")),
        env_key: get_env_keys(obj, "envKey").or_else(|| get_env_keys(obj, "env_key")),
        api_backend,
        context_window,
        auto_compact_threshold_percent: get_u64(obj, "autoCompactThresholdPercent")
            .or_else(|| get_u64(obj, "auto_compact_threshold_percent"))
            .and_then(|v| u8::try_from(v).ok()),
        system_prompt_label: get_string(obj, "systemPromptLabel")
            .or_else(|| get_string(obj, "system_prompt_label"))
            .filter(|s| !s.trim().is_empty()),
        extra_headers: get_string_map(obj, "extraHeaders"),
        api_base_url: get_string(obj, "apiBaseUrl")
            .or_else(|| get_string(obj, "api_base_url")),
        use_concise: obj
            .get("useConcise")
            .or_else(|| obj.get("use_concise"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        agent_type,
        inference_idle_timeout_secs: get_u64(obj, "inferenceIdleTimeoutSecs")
            .or_else(|| get_u64(obj, "inference_idle_timeout_secs")),
        max_retries: get_u64(obj, "maxRetries")
            .or_else(|| get_u64(obj, "max_retries"))
            .and_then(|v| u32::try_from(v).ok()),
        hidden: obj
            .get("hidden")
            .or_else(|| meta.and_then(|m| m.get("hidden")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        supported_in_api: obj
            .get("supportedInApi")
            .or_else(|| obj.get("supported_in_api"))
            .or_else(|| meta.and_then(|m| m.get("supportedInApi")))
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        auth_scheme: None,
        reasoning_effort: get_string(obj, "reasoningEffort")
            .or_else(|| get_string(obj, "reasoning_effort"))
            .or_else(|| meta.and_then(|m| get_string(m, "reasoningEffort")))
            .and_then(|s| s.parse().ok()),
        supports_reasoning_effort: obj
            .get("supportsReasoningEffort")
            .or_else(|| obj.get("supports_reasoning_effort"))
            .or_else(|| meta.and_then(|m| m.get("supportsReasoningEffort")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        reasoning_efforts: obj
            .get("reasoningEfforts")
            .or_else(|| obj.get("reasoning_efforts"))
            .or_else(|| meta.and_then(|m| m.get("reasoningEfforts")))
            .and_then(|v| v.as_array())
            .map(|arr| kigi_sampling_types::parse_reasoning_effort_options(arr))
            .unwrap_or_default(),
        capabilities: obj
            .get("capabilities")
            .and_then(|v| {
                serde_json::from_value::<Vec<kigi_models::ModelCapability>>(v.clone()).ok()
            })
            .unwrap_or_default(),
        supports_backend_search: obj
            .get("supportsBackendSearch")
            .or_else(|| obj.get("supports_backend_search"))
            .or_else(|| meta.and_then(|m| m.get("supportsBackendSearch")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        compactions_remaining: obj
            .get("compactionsRemaining")
            .or_else(|| obj.get("compactions_remaining"))
            .or_else(|| meta.and_then(|m| m.get("compactionsRemaining")))
            .and_then(parse_compactions_remaining)
            .or_else(|| {
                obj
                    .get("sendCompactionsRemaining")
                    .or_else(|| obj.get("send_compactions_remaining"))
                    .or_else(|| meta.and_then(|m| m.get("sendCompactionsRemaining")))
                    .and_then(|v| v.as_bool())
                    .map(kigi_sampling_types::CompactionsRemaining::Dynamic)
            }),
        compaction_at_tokens: obj
            .get("compactionAtTokens")
            .or_else(|| obj.get("compaction_at_tokens"))
            .or_else(|| meta.and_then(|m| m.get("compactionAtTokens")))
            .and_then(parse_compaction_at_tokens),
        show_model_fingerprint: obj
            .get("showModelFingerprint")
            .or_else(|| obj.get("show_model_fingerprint"))
            .or_else(|| meta.and_then(|m| m.get("showModelFingerprint")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        stream_tool_calls: obj
            .get("streamToolCalls")
            .or_else(|| obj.get("stream_tool_calls"))
            .and_then(|v| v.as_bool()),
        laziness_detector: get_object(obj, "lazinessDetector")
            .or_else(|| get_object(obj, "laziness_detector"))
            .or_else(|| meta.and_then(|m| get_object(m, "lazinessDetector")))
            .and_then(|v| match serde_json::from_value::<
                crate::agent::config::LazinessDetectorPerModelConfig,
            >(v.clone()) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    tracing::warn!(
                        error = % e,
                        "Failed to deserialize laziness_detector block from remote model; falling back to default"
                    );
                    None
                }
            })
            .unwrap_or_default(),
    })
}
fn get_string(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}
/// Parse `env_key` / `envKey` as a single string or a string array.
fn get_env_keys(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<crate::agent::config::EnvKeys> {
    let v = obj.get(key)?;
    if let Some(s) = v.as_str() {
        return Some(crate::agent::config::EnvKeys::single(s));
    }
    if let Some(arr) = v.as_array() {
        let mut names = Vec::with_capacity(arr.len());
        for item in arr {
            let Some(s) = item.as_str() else {
                tracing::warn!(
                    key,
                    "env_key array has a non-string element; ignoring env_key"
                );
                return None;
            };
            if !s.is_empty() {
                names.push(s.to_owned());
            }
        }
        if names.is_empty() {
            return None;
        }
        return Some(crate::agent::config::EnvKeys::new(names));
    }
    None
}
fn parse_compaction_at_tokens(
    v: &serde_json::Value,
) -> Option<kigi_sampling_types::CompactionAtTokens> {
    use kigi_sampling_types::CompactionAtTokens;
    v.as_bool()
        .map(CompactionAtTokens::Enabled)
        .or_else(|| v.as_u64().map(CompactionAtTokens::Fixed))
}
fn parse_compactions_remaining(
    v: &serde_json::Value,
) -> Option<kigi_sampling_types::CompactionsRemaining> {
    use kigi_sampling_types::CompactionsRemaining;
    v.as_bool().map(CompactionsRemaining::Dynamic).or_else(|| {
        v.as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .map(CompactionsRemaining::Fixed)
    })
}
fn get_u64(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(|v| v.as_u64())
}
fn get_f64(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<f64> {
    obj.get(key).and_then(|v| v.as_f64())
}
fn get_object<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    obj.get(key).filter(|v| v.is_object())
}
fn get_string_map(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> IndexMap<String, String> {
    obj.get(key)
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn get_env_keys_parses_strings_and_rejects_non_strings() {
        use crate::agent::config::EnvKeys;
        let parse = |v: serde_json::Value| {
            let obj = serde_json::json!({ "env_key" : v });
            get_env_keys(obj.as_object().unwrap(), "env_key")
        };
        assert_eq!(parse(serde_json::json!("A")), Some(EnvKeys::single("A")));
        assert_eq!(
            parse(serde_json::json!(["A", "B"])),
            Some(EnvKeys::new(["A", "B"]))
        );
        assert_eq!(parse(serde_json::json!(["A", 123])), None);
        assert_eq!(parse(serde_json::json!([])), None);
    }
    #[test]
    fn parse_openai_format_uses_id_field() {
        let value = serde_json::json!(
            { "id" : "kigi-3", "object" : "model", "owned_by" : "xai", "context_window" :
            131072 }
        );
        let result = parse_remote_model_value(&value, "https://byok.example/v1").unwrap();
        assert_eq!(result.model, "kigi-3");
        assert_eq!(result.base_url, "https://byok.example/v1");
        assert_eq!(result.name.as_deref(), Some("kigi-3"));
    }
    #[test]
    fn parse_model_field_takes_priority_over_id() {
        let value = serde_json::json!(
            { "id" : "display-key", "model" : "actual-model-id", "name" : "Display Name",
            "context_window" : 131072 }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(result.model, "actual-model-id");
        assert_eq!(result.name.as_deref(), Some("Display Name"));
    }
    /// Live-wire regression: the K3 `/models` entry (api.kimi.com, 2026-07)
    /// must land in the catalog with selectable low/high/max efforts and a
    /// max default — this is what feeds `/model <m> [effort]` and `/effort`.
    #[test]
    fn platform_entry_maps_live_k3_think_efforts() {
        use kigi_sampling_types::ReasoningEffort;
        let wire: kigi_models::WireModel = serde_json::from_value(serde_json::json!({
            "id": "k3",
            "display_name": "K3",
            "context_length": 1_048_576,
            "supports_reasoning": true,
            "supports_image_in": true,
            "supports_video_in": true,
            "supports_thinking_type": "only",
            "think_efforts": {
                "support": true,
                "valid_efforts": ["low", "high", "max"],
                "default_effort": "max"
            }
        }))
        .unwrap();
        let entry = platform_wire_model_to_entry(
            kigi_models::PlatformId::KimiCode,
            wire,
            "https://api.kimi.com/coding/v1",
        );
        assert!(entry.supports_reasoning_effort);
        // The wire token "max" is canonical Max since the Xhigh/Max split;
        // kimi_compat still spells it "max" on the inference wire.
        assert_eq!(entry.reasoning_effort, Some(ReasoningEffort::Max));
        let ids: Vec<&str> = entry
            .reasoning_efforts
            .iter()
            .map(|o| o.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["low", "high", "max"],
            "wire tokens stay the option ids"
        );
        assert_eq!(
            entry
                .reasoning_efforts
                .iter()
                .map(|o| o.value)
                .collect::<Vec<_>>(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ],
        );
        let max = entry
            .reasoning_efforts
            .iter()
            .find(|o| o.id == "max")
            .unwrap();
        assert!(max.default, "max is the server default for K3");
        assert_eq!(max.label, "Max");
        // K2.7-style entries (no think_efforts) stay effort-less.
        let plain: kigi_models::WireModel = serde_json::from_value(serde_json::json!({
            "id": "kimi-for-coding",
            "context_length": 262_144,
            "supports_reasoning": true,
            "supports_thinking_type": "only"
        }))
        .unwrap();
        let entry = platform_wire_model_to_entry(
            kigi_models::PlatformId::KimiCode,
            plain,
            "https://api.kimi.com/coding/v1",
        );
        assert!(!entry.supports_reasoning_effort);
        assert!(entry.reasoning_efforts.is_empty());
        assert!(entry.reasoning_effort.is_none());
    }
    #[test]
    fn parse_reads_reasoning_effort_fields() {
        use kigi_sampling_types::ReasoningEffort;
        let value = serde_json::json!(
            { "model" : "kigi-4.5", "context_window" : 1_000_000,
            "supports_reasoning_effort" : true, "reasoning_effort" : "high" }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.supports_reasoning_effort);
        assert_eq!(result.reasoning_effort, Some(ReasoningEffort::High));
        let value = serde_json::json!(
            { "model" : "kigi-4.5", "contextWindow" : 1_000_000,
            "supportsReasoningEffort" : true, "reasoningEffort" : "xhigh" }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.supports_reasoning_effort);
        assert_eq!(result.reasoning_effort, Some(ReasoningEffort::Xhigh));
        let value = serde_json::json!({ "model" : "x", "context_window" : 256_000 });
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(!result.supports_reasoning_effort);
        assert!(result.reasoning_effort.is_none());
    }
    #[test]
    fn parse_reads_reasoning_efforts_list() {
        use kigi_sampling_types::ReasoningEffort;
        let value = serde_json::json!(
            { "model" : "kigi-4.5", "context_window" : 1_000_000, "reasoning_efforts" :
            [{ "id" : "deep", "value" : "xhigh", "label" : "Deep" }, { "value" :
            "quantum" }, "low",] }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(result.reasoning_efforts.len(), 2);
        assert_eq!(result.reasoning_efforts[0].id, "deep");
        assert_eq!(result.reasoning_efforts[0].value, ReasoningEffort::Xhigh);
        assert_eq!(result.reasoning_efforts[1].value, ReasoningEffort::Low);
        for value in [
            serde_json::json!(
                { "model" : "m", "context_window" : 256_000, "reasoningEfforts" : [{
                "value" : "high" }] }
            ),
            serde_json::json!(
                { "model" : "m", "context_window" : 256_000, "_meta" : {
                "reasoningEfforts" : [{ "value" : "high" }] } }
            ),
        ] {
            let result = parse_remote_model_value(&value, "https://default.url").unwrap();
            assert_eq!(result.reasoning_efforts.len(), 1);
            assert_eq!(result.reasoning_efforts[0].value, ReasoningEffort::High);
        }
        let value = serde_json::json!({ "model" : "x", "context_window" : 256_000 });
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.reasoning_efforts.is_empty());
    }
    #[test]
    fn parse_reads_meta_fallback_fields() {
        let value = serde_json::json!(
            { "_meta" : { "model" : "meta-model-id", "contextWindow" : 131072,
            "agentType" : "concise" } }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(result.model, "meta-model-id");
        assert_eq!(
            result.context_window,
            std::num::NonZeroU64::new(131072).unwrap()
        );
        assert_eq!(result.agent_type, "concise");
    }
    #[test]
    fn parse_remote_model_value_no_laziness_detector_block_yields_default() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(
            result.laziness_detector,
            crate::agent::config::LazinessDetectorPerModelConfig::default()
        );
    }
    #[test]
    fn parse_remote_model_value_parses_camelcase_key() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "max_nudges_per_session" : 2, "idle_threshold_ms" : 12_000,
            "min_confidence" : 0.75, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 2,
            idle_threshold_ms: Some(12_000),
            min_confidence: Some(0.75),
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    #[test]
    fn parse_remote_model_value_parses_snake_case_laziness_detector() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "laziness_detector" : {
            "enabled" : true, "max_nudges_per_session" : 3, "idle_threshold_ms" : 8_000,
            "min_confidence" : 0.6, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 3,
            idle_threshold_ms: Some(8_000),
            min_confidence: Some(0.6),
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    #[test]
    fn parse_remote_model_value_parses_meta_laziness_detector() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "_meta" : {
            "lazinessDetector" : { "enabled" : true, "max_nudges_per_session" : 1,
            "idle_threshold_ms" : 15_000, "min_confidence" : 0.9, }, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 1,
            idle_threshold_ms: Some(15_000),
            min_confidence: Some(0.9),
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    #[test]
    fn parse_remote_model_value_partial_block_uses_field_defaults() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 0,
            idle_threshold_ms: None,
            min_confidence: None,
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    #[test]
    fn parse_remote_model_value_malformed_block_falls_back_to_default() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "max_nudges_per_session" : "abc", }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(
            result.laziness_detector,
            crate::agent::config::LazinessDetectorPerModelConfig::default()
        );
    }
    #[test]
    fn parse_remote_model_value_non_object_value_falls_back_to_default() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" :
            "not-an-object", }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(
            result.laziness_detector,
            crate::agent::config::LazinessDetectorPerModelConfig::default()
        );
    }
    #[test]
    fn parse_remote_model_value_top_level_camelcase_wins_over_snake_case() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "max_nudges_per_session" : 7, }, "laziness_detector" : {
            "enabled" : false, "max_nudges_per_session" : 99, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 7,
            idle_threshold_ms: None,
            min_confidence: None,
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    /// `include_reasoning: false` parses cleanly under the per-model
    /// `lazinessDetector` block (camelCase wrapper, snake_case inner —
    /// matching the existing field-naming convention used for the
    /// sibling `min_confidence`, `idle_threshold_ms`, etc.).
    #[test]
    fn parse_remote_model_value_parses_include_reasoning_under_camelcase_wrapper() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "include_reasoning" : false, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(result.laziness_detector.include_reasoning, Some(false));
    }
    #[test]
    fn parse_remote_model_value_parses_include_reasoning_under_snake_case_wrapper() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "laziness_detector" : {
            "enabled" : true, "include_reasoning" : true, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(result.laziness_detector.include_reasoning, Some(true));
    }
    #[test]
    fn parse_remote_model_value_omitted_include_reasoning_defaults_to_none() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "max_nudges_per_session" : 2, }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert_eq!(
            result.laziness_detector.include_reasoning, None,
            "absent include_reasoning defers to harness default via None",
        );
    }
    #[test]
    fn parse_remote_model_value_top_level_wins_over_meta() {
        let value = serde_json::json!(
            { "model" : "kigi-4", "context_window" : 256_000, "lazinessDetector" : {
            "enabled" : true, "max_nudges_per_session" : 5, }, "_meta" : {
            "lazinessDetector" : { "enabled" : false, "max_nudges_per_session" : 99, },
            }, }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        let expected = crate::agent::config::LazinessDetectorPerModelConfig {
            enabled: true,
            max_nudges_per_session: 5,
            idle_threshold_ms: None,
            min_confidence: None,
            include_reasoning: None,
        };
        assert_eq!(result.laziness_detector, expected);
    }
    #[test]
    fn parse_reads_show_model_fingerprint_field() {
        let value = serde_json::json!(
            { "model" : "kigi", "context_window" : 256_000,
            "show_model_fingerprint" : true }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.show_model_fingerprint);
        let value = serde_json::json!(
            { "model" : "kigi", "contextWindow" : 256_000, "showModelFingerprint" :
            true }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.show_model_fingerprint);
        let value = serde_json::json!(
            { "model" : "kigi", "context_window" : 256_000, "_meta" : {
            "showModelFingerprint" : true } }
        );
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(result.show_model_fingerprint);
        let value = serde_json::json!({ "model" : "x", "context_window" : 256_000 });
        let result = parse_remote_model_value(&value, "https://default.url").unwrap();
        assert!(!result.show_model_fingerprint);
    }
    #[test]
    fn get_object_returns_none_for_non_object_values() {
        let value = serde_json::json!(
            { "string" : "hello", "number" : 42, "bool" : true, "array" : [1, 2, 3],
            "null" : null, }
        );
        let obj = value.as_object().unwrap();
        assert!(get_object(obj, "string").is_none());
        assert!(get_object(obj, "number").is_none());
        assert!(get_object(obj, "bool").is_none());
        assert!(get_object(obj, "array").is_none());
        assert!(get_object(obj, "null").is_none());
        assert!(get_object(obj, "missing").is_none());
    }
    #[test]
    fn get_object_returns_some_for_actual_object() {
        let value = serde_json::json!({ "nested" : { "a" : 1, "b" : "two" }, });
        let obj = value.as_object().unwrap();
        let nested = get_object(obj, "nested").expect("nested key should resolve to object");
        assert!(nested.is_object());
        assert_eq!(nested["a"], serde_json::json!(1));
        assert_eq!(nested["b"], serde_json::json!("two"));
    }
    fn endpoints(
        proxy: &str,
        models_base_url: Option<&str>,
        models_list_url: Option<&str>,
    ) -> crate::agent::config::EndpointsConfig {
        crate::agent::config::EndpointsConfig {
            coding_api_base_url: Some(proxy.to_owned()),
            models_base_url: models_base_url.map(|s| s.to_owned()),
            models_list_url: models_list_url.map(|s| s.to_owned()),
            ..Default::default()
        }
    }
    #[test]
    fn inference_url_defaults_to_proxy() {
        let ep = endpoints("https://proxy.kigi.com/v1", None, None);
        assert_eq!(ep.resolve_inference_base_url(), "https://proxy.kigi.com/v1");
    }
    #[test]
    fn inference_url_uses_models_base_url() {
        let ep = endpoints(
            "https://proxy.kigi.com/v1",
            Some("https://enterprise.acme.com/v1"),
            None,
        );
        assert_eq!(
            ep.resolve_inference_base_url(),
            "https://enterprise.acme.com/v1"
        );
    }
    #[test]
    fn inference_url_base_url_wins_over_proxy() {
        let ep = endpoints(
            "https://proxy.kigi.com/v1",
            Some("https://inference.acme.com/v1"),
            Some("https://registry.acme.com/api/models"),
        );
        assert_eq!(
            ep.resolve_inference_base_url(),
            "https://inference.acme.com/v1"
        );
    }
    #[test]
    fn list_url_defaults_to_proxy_models() {
        let ep = endpoints("https://proxy.kigi.com/v1", None, None);
        assert_eq!(
            ep.resolve_models_list_url(),
            "https://proxy.kigi.com/v1/models"
        );
    }
    #[test]
    fn list_url_derived_from_base_url() {
        let ep = endpoints(
            "https://proxy.kigi.com/v1",
            Some("https://byok.example/v1"),
            None,
        );
        assert_eq!(
            ep.resolve_models_list_url(),
            "https://byok.example/v1/models"
        );
    }
    #[test]
    fn list_url_explicit_overrides_derivation() {
        let ep = endpoints(
            "https://proxy.kigi.com/v1",
            Some("https://inference.acme.com/v1"),
            Some("https://registry.acme.com/api/list-models"),
        );
        assert_eq!(
            ep.resolve_models_list_url(),
            "https://registry.acme.com/api/list-models"
        );
    }
    /// INVARIANT: each platform's `/models` URL matches its registry base —
    /// kimi-code → the subscription proxy (config override respected, else the
    /// kigi-env default), moonshot platforms → their fixed bases — and the
    /// cache-origin key encodes the enabled fetch plan without any secrets.
    #[test]
    #[serial_test::serial]
    fn platform_models_urls_and_fetch_origin() {
        use crate::agent::config::EndpointsConfig;
        use crate::agent::models::{ModelFetchAuth, PlatformApiKeys};
        for k in [
            "KIGI_CODE_BASE_URL",
            "KIGI_CODE_BASE_URL",
            "KIGI_MODELS_LIST_URL",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        let cfg = EndpointsConfig::from_config_value(&toml::Value::Table(Default::default()));
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::KimiCode, &cfg),
            "https://api.kimi.com/coding/v1/models"
        );
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::MoonshotCn, &cfg),
            "https://api.moonshot.cn/v1/models"
        );
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::MoonshotAi, &cfg),
            "https://api.moonshot.ai/v1/models"
        );
        // Proxy override re-points the subscription platform only.
        let proxied = EndpointsConfig::from_config_value(
            &toml::from_str(
                r#"[endpoints]
                coding_api_base_url = "https://proxy.acme.example/v1""#,
            )
            .unwrap(),
        );
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::KimiCode, &proxied),
            "https://proxy.acme.example/v1/models"
        );
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::MoonshotCn, &proxied),
            "https://api.moonshot.cn/v1/models"
        );

        // Origin key: OAuth-only plan lists kimi-code only; adding a moonshot
        // key changes the plan (→ cache miss); the key VALUE never appears.
        let oauth_only = models_fetch_origin(
            &cfg,
            ModelFetchAuth::Platforms,
            true,
            &PlatformApiKeys::default(),
        );
        assert_eq!(
            oauth_only,
            "platforms[kimi-code=https://api.kimi.com/coding/v1/models]"
        );
        let with_cn = models_fetch_origin(
            &cfg,
            ModelFetchAuth::Platforms,
            true,
            &crate::agent::models::PlatformApiKeys::test_keys(Some("sk-secret-cn"), None),
        );
        assert_ne!(
            oauth_only, with_cn,
            "enabling a platform must change the origin"
        );
        assert!(with_cn.contains("moonshot-cn=https://api.moonshot.cn/v1/models"));
        assert!(
            !with_cn.contains("sk-secret-cn"),
            "origin key must never embed credential values"
        );

        // Custom endpoint mode → the explicit list URL verbatim.
        let custom = EndpointsConfig::from_config_value(
            &toml::from_str(
                r#"[endpoints]
                models_base_url = "https://models.acme.com/v1""#,
            )
            .unwrap(),
        );
        assert_eq!(
            models_fetch_origin(
                &custom,
                ModelFetchAuth::CustomEndpoint,
                false,
                &PlatformApiKeys::default(),
            ),
            "https://models.acme.com/v1/models"
        );
    }
}
