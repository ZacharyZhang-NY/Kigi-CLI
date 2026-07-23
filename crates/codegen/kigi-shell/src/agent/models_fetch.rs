//! Model catalog fetch (PRD F4).
//!
//! Fetches the model catalog with `GET {base}/models` per enabled platform
//! (the subscription platform via the OAuth session, the open platforms via
//! their API keys), plus the custom-endpoint OpenAI-compatible listing path.
//!
//! It talks only to the configured platform model endpoints (plus the
//! models.dev metadata refresh when an enabled platform needs enrichment —
//! see `enrichment_fetch`), never to a proxy backend.
use crate::auth::KimiAuth;
use indexmap::IndexMap;
use serde::Deserialize;

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

/// Session bearer tokens for the GENERIC device-code OAuth platforms
/// (`platform.oauth().is_some()` — xai-grok today), resolved per provider from
/// its own scope-keyed `AuthManager` (refreshed on expiry) before the blocking
/// fetch. kimi-code is NOT here — it still rides the single `auth: &KimiAuth`.
///
/// SECURITY: values are access tokens — never logged, never persisted here.
pub(crate) type OAuthSessionTokens = std::collections::BTreeMap<kigi_models::PlatformId, String>;

/// Resolve session bearers for every generic device-code OAuth platform whose
/// own `AuthManager` holds a usable (refreshed-on-expiry) session. Each
/// platform resolves from ITS OWN scope (`oauth/xai`, …) — independent of the
/// Kimi session. Providers without a stored session are simply absent. Only a
/// non-secret count is logged.
pub(crate) async fn resolve_generic_oauth_tokens(
    kigi_home: &std::path::Path,
) -> OAuthSessionTokens {
    let mut tokens = OAuthSessionTokens::new();
    for platform in kigi_models::PlatformId::ALL {
        let Some(oauth) = platform.oauth() else {
            continue;
        };
        let manager = std::sync::Arc::new(crate::auth::AuthManager::new_oauth_provider(
            kigi_home, oauth,
        ));
        manager.configure_refresher();
        if let Ok(auth) = manager.auth().await {
            tokens.insert(platform, auth.key);
        }
    }
    if !tokens.is_empty() {
        tracing::info!(
            count = tokens.len(),
            "resolved generic oauth platform session tokens"
        );
    }
    tokens
}

/// The generic device-code OAuth platforms with a STORED session scope in
/// auth.json — the cheap sync companion to [`resolve_generic_oauth_tokens`]
/// (no `AuthManager`, no refresh, no secrets read beyond scope presence).
///
/// Every place that reasons about the FETCH PLAN without resolving bearers
/// (the prefetch arming gate, `on_auth_changed`'s wipe guard, cache-origin
/// computation) must consult this, or a subscription-OAuth-only user (e.g.
/// claude-pro-max with no Kimi session and no API key) is treated as
/// credential-less: catalog wiped/never fetched, session stuck on the
/// bundled Kimi table with an empty picker.
pub(crate) fn stored_oauth_platforms(kigi_home: &std::path::Path) -> Vec<kigi_models::PlatformId> {
    let Ok(store) = crate::auth::read_auth_json(&kigi_home.join("auth.json")) else {
        return Vec::new();
    };
    kigi_models::PlatformId::ALL
        .into_iter()
        .filter(|p| p.oauth().is_some_and(|o| store.contains_key(o.scope_key)))
        .collect()
}

/// Presence-only stand-in for [`OAuthSessionTokens`] in fetch-plan/origin
/// computations. Values are EMPTY strings: cache origins encode enabled
/// platform NAMES and URLs only ([`models_fetch_origin`]), and this map must
/// never reach a request builder — real bearers come from
/// [`resolve_generic_oauth_tokens`] on the fetch path itself.
pub(crate) fn stored_oauth_token_stubs(kigi_home: &std::path::Path) -> OAuthSessionTokens {
    stored_oauth_platforms(kigi_home)
        .into_iter()
        .map(|p| (p, String::new()))
        .collect()
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
    oauth_tokens: &OAuthSessionTokens,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> String {
    match fetch_auth {
        crate::agent::models::ModelFetchAuth::CustomEndpoint => endpoints.resolve_models_list_url(),
        crate::agent::models::ModelFetchAuth::Platforms => {
            let parts: Vec<String> = enabled_platforms(has_oauth, oauth_tokens, platform_keys)
                .into_iter()
                .map(|p| format!("{}={}", p.as_str(), platform_models_url(p, endpoints)))
                .collect();
            format!("platforms[{}]", parts.join(";"))
        }
    }
}
/// The platforms with usable credentials, in registry order (kimi-code first
/// so "default model = first list item" favors the subscription).
///
/// - kimi-code (`uses_oauth`, no `OAuthConfig`) is gated on the single Kimi
///   session (`has_oauth`);
/// - a generic device-code OAuth platform (`oauth().is_some()`, e.g. xai-grok)
///   is gated on ITS OWN resolved session token (`oauth_tokens`);
/// - an API-key platform is gated on a stored key.
fn enabled_platforms(
    has_oauth: bool,
    oauth_tokens: &OAuthSessionTokens,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Vec<kigi_models::PlatformId> {
    kigi_models::PlatformId::ALL
        .into_iter()
        .filter(|p| {
            if p.oauth().is_some() {
                oauth_tokens.contains_key(p)
            } else if p.uses_oauth() {
                has_oauth
            } else {
                platform_keys.key_for(*p).is_some()
            }
        })
        .collect()
}
/// The inference/listing base for one platform's `/models` fetch:
/// - kimi-code (`uses_oauth`, no `OAuthConfig`) → the Kimi subscription base
///   via the endpoints config (`proxy_url`);
/// - every other platform — API-key AND generic OAuth (xai-grok) — → its own
///   registry `base_url()` (e.g. api.x.ai/v1).
fn platform_fetch_base(
    platform: kigi_models::PlatformId,
    endpoints: &crate::agent::config::EndpointsConfig,
) -> String {
    if platform.uses_oauth() && platform.oauth().is_none() {
        endpoints.proxy_url()
    } else {
        platform.base_url()
    }
}
/// `{base}/models` for one platform (see [`platform_fetch_base`]).
fn platform_models_url(
    platform: kigi_models::PlatformId,
    endpoints: &crate::agent::config::EndpointsConfig,
) -> String {
    format!(
        "{}/models",
        platform_fetch_base(platform, endpoints).trim_end_matches('/')
    )
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
    oauth_tokens: &OAuthSessionTokens,
    fetch_auth: crate::agent::models::ModelFetchAuth,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Result<FetchModelsResult, BackendError> {
    match fetch_auth {
        crate::agent::models::ModelFetchAuth::CustomEndpoint => {
            fetch_custom_endpoint_models_blocking(endpoints, auth)
        }
        crate::agent::models::ModelFetchAuth::Platforms => {
            fetch_platform_models_blocking(endpoints, auth, oauth_tokens, platform_keys)
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
            BackendError::Auth("No API key for custom models endpoint. Set KIGI_API_KEY.".into())
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
    oauth_tokens: &OAuthSessionTokens,
    platform_keys: &crate::agent::models::PlatformApiKeys,
) -> Result<FetchModelsResult, BackendError> {
    let enabled = enabled_platforms(auth.is_some(), oauth_tokens, platform_keys);
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
        let bearer = if platform.oauth().is_some() {
            // Generic device-code OAuth platform (xai-grok): its OWN resolved
            // session token — never the Kimi session.
            oauth_tokens
                .get(platform)
                .expect("enabled_platforms gated on generic-oauth token presence")
                .clone()
        } else if platform.uses_oauth() {
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
                // The catalog etag tracks the Kimi subscription listing only
                // (kimi-code: uses_oauth with no generic OAuthConfig).
                if platform.uses_oauth() && platform.oauth().is_none() {
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
    // A platform that serves NO live `/models` listing (openai-codex) delivers a
    // HARDCODED catalog: short-circuit BEFORE any HTTP, mapping the compiled-in
    // `WireModel`s through the SAME `platform_wire_model_to_entry` output a live
    // listing produces (context window + per-model reasoning efforts). The
    // bearer is unused here (login gates availability; no request is made).
    if let Some(wire_models) = platform.hardcoded_catalog() {
        let base_url = platform_fetch_base(platform, endpoints);
        let models = wire_models
            .into_iter()
            .map(|wire| platform_wire_model_to_entry(platform, wire, &base_url))
            .collect();
        return Ok((models, None));
    }
    let client = crate::http::shared_blocking_client();
    let url = match platform.listing() {
        kigi_models::ListingDialect::OpenAi => platform_models_url(platform, endpoints),
        // Anthropic paginates (default 20); limit=1000 is the documented max
        // and far above the catalog size (the adapter warns on has_more).
        kigi_models::ListingDialect::Anthropic => {
            format!("{}?limit=1000", platform_models_url(platform, endpoints))
        }
    };
    tracing::info!(platform = platform.as_str(), url = %url, "fetching platform models");
    let request = match platform.key_header() {
        kigi_models::PlatformKeyHeader::Bearer => {
            let mut req = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", bearer));
            // An Anthropic listing reached with a Bearer key is the OAuth
            // channel (claude-pro-max): the /v1/models endpoint requires
            // anthropic-version, and the OAuth bearer needs the oauth beta.
            // The OpenAI-listing Bearer platforms (xai-grok, api-key OpenAI
            // rows) add neither, so their requests stay byte-identical.
            if platform.listing() == kigi_models::ListingDialect::Anthropic {
                req = req
                    .header("anthropic-version", kigi_sampling_types::ANTHROPIC_VERSION)
                    .header("anthropic-beta", kigi_sampling_types::ANTHROPIC_OAUTH_BETA);
            }
            // GitHub Copilot /models needs the VS Code editor identity + the
            // Copilot API version. github-copilot-GATED, so every other Bearer
            // OpenAI-listing platform (xai-grok, api-key OpenAI rows) is
            // byte-identical.
            if platform.sends_copilot_editor_headers() {
                req = req
                    .header("User-Agent", kigi_sampling_types::COPILOT_USER_AGENT)
                    .header(
                        "Editor-Version",
                        kigi_sampling_types::COPILOT_EDITOR_VERSION,
                    )
                    .header(
                        "Editor-Plugin-Version",
                        kigi_sampling_types::COPILOT_EDITOR_PLUGIN_VERSION,
                    )
                    .header(
                        "Copilot-Integration-Id",
                        kigi_sampling_types::COPILOT_INTEGRATION_ID,
                    )
                    .header(
                        "X-GitHub-Api-Version",
                        kigi_sampling_types::COPILOT_API_VERSION,
                    );
            }
            req
        }
        kigi_models::PlatformKeyHeader::XApiKey => client
            .get(&url)
            .header("x-api-key", bearer)
            .header("anthropic-version", kigi_sampling_types::ANTHROPIC_VERSION),
    };
    let response = request.send()?;
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
    let data = match platform.listing() {
        // GitHub Copilot serves an OpenAI-shape listing with extra availability
        // fields (model_picker_enabled/policy/tool_calls). Parse + filter it
        // with the Copilot-specific adapter (keep only selectable, tool-calling,
        // openai-completions-served ids). Platform-gated; every other OpenAi
        // listing takes the plain path.
        kigi_models::ListingDialect::OpenAi if platform.sends_copilot_editor_headers() => {
            let body = response.text()?;
            kigi_models::parse_github_copilot_listing(&body).map_err(|e| {
                BackendError::RequestFailed {
                    status: 200,
                    body: format!("copilot listing parse failed: {e}"),
                }
            })?
        }
        kigi_models::ListingDialect::OpenAi => {
            // Tolerant of both the {data:[...]} envelope and a bare array
            // (Together AI serves the bare form).
            let body = response.text()?;
            kigi_models::parse_openai_listing(&body).map_err(|e| BackendError::RequestFailed {
                status: 200,
                body: format!("openai listing parse failed: {e}"),
            })?
        }
        kigi_models::ListingDialect::Anthropic => {
            let body = response.text()?;
            kigi_models::parse_anthropic_listing(&body).map_err(|e| {
                BackendError::RequestFailed {
                    status: 200,
                    body: format!("anthropic listing parse failed: {e}"),
                }
            })?
        }
    };
    // Canonicalize listing ids before filtering/enrichment/keying. Google's
    // OpenAI-compat `/models` returns `models/`-prefixed ids while its chat
    // endpoint and the models.dev snapshot use the bare id — without this the
    // enrichment lookup misses and `restrict_to_enriched` would empty the
    // catalog. No-op for platforms with no configured prefix.
    let mut data = data;
    if let Some(prefix) = platform.strip_listing_id_prefix() {
        for wire in &mut data {
            if let Some(bare) = wire.id.strip_prefix(prefix) {
                wire.id = bare.to_string();
            }
        }
    }
    let total = data.len();
    let mut filtered = kigi_models::filter_allowed_models(platform, data);
    if filtered.len() != total {
        tracing::info!(
            platform = platform.as_str(),
            total,
            kept = filtered.len(),
            "applied platform model-prefix filter"
        );
    }
    // Polluted listings (tts/embeddings/image entries) are restricted to
    // models the enrichment catalog knows. FAIL-SAFE: if enrichment has no
    // data for this provider at all (refresh broken AND snapshot gap), keep
    // the full listing with a warning — a noisy picker beats an empty one.
    if platform.restrict_to_enriched()
        && let Some(dev_id) = platform.models_dev_id()
    {
        let provider_known = enrichment.get(dev_id).is_some_and(|m| !m.is_empty());
        if provider_known {
            let before = filtered.len();
            let mut dropped: Vec<String> = Vec::new();
            // Keep only tool-calling chat models: membership alone would
            // admit models.dev-known embeddings/moderation entries, which
            // would 400 on every agentic request (EnrichmentModel.tool_call
            // exists exactly for this cut).
            filtered.retain(|wire| {
                let keep = kigi_models::enrichment::lookup(enrichment, dev_id, &wire.id)
                    .is_some_and(|meta| meta.tool_call);
                if !keep {
                    dropped.push(wire.id.clone());
                }
                keep
            });
            if filtered.len() != before {
                tracing::info!(
                    platform = platform.as_str(),
                    before,
                    kept = filtered.len(),
                    "restricted listing to tool-calling enrichment-known models"
                );
                // A launch-day model missing from enrichment lands here for
                // up to models.dev lag + cache TTL — keep the ids traceable.
                tracing::debug!(
                    platform = platform.as_str(),
                    dropped = ?dropped,
                    "listing ids dropped by the enrichment restriction"
                );
            }
        } else {
            tracing::warn!(
                platform = platform.as_str(),
                "no enrichment data for provider; keeping full listing"
            );
        }
    }
    let base_url = platform_fetch_base(platform, endpoints);
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
/// Map a live `think_efforts` block to catalog effort options. The wire
/// token stays the option id/label (`"max"` → label `"Max"`) so the UI
/// mirrors the server's vocabulary, while the canonical value maps through
/// the [`kigi_sampling_types::ReasoningEffort`] parser (`"max"` → `Max`
/// since the Xhigh/Max split). Unknown tokens are dropped with a warning
/// rather than inventing a level.
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

/// Map one F4 wire model to a catalog entry config.
///
/// SECURITY: the entry carries only env-var NAMES (`env_key`) for the open
/// platforms — never key values — because raw fetched entries are persisted
/// to the models disk cache. Config-file keys are stamped in-memory later by
/// `resolve_model_list`'s platform-credentials layer.
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
    let api_backend = match platform.wire_api() {
        kigi_models::PlatformWireApi::ChatCompletions => {
            crate::sampling::ApiBackend::ChatCompletions
        }
        kigi_models::PlatformWireApi::Responses => crate::sampling::ApiBackend::Responses,
        kigi_models::PlatformWireApi::Messages => crate::sampling::ApiBackend::Messages,
    };
    let auth_scheme = match platform.key_header() {
        kigi_models::PlatformKeyHeader::Bearer => None,
        kigi_models::PlatformKeyHeader::XApiKey => Some(kigi_sampler::AuthScheme::XApiKey),
    };
    crate::agent::config::ModelEntryConfig {
        id: Some(platform.managed_model_key(&wire.id)),
        name: Some(wire.display_name.clone().unwrap_or_else(|| wire.id.clone())),
        model: wire.id,
        base_url: base_url.to_owned(),
        description: None,
        // The wire/enrichment output cap; the sampler otherwise defaults to
        // 128K, which Anthropic rejects on smaller-cap models (400 on every
        // request for e.g. a 64K haiku).
        max_completion_tokens: (wire.max_output_tokens > 0)
            .then(|| u32::try_from(wire.max_output_tokens).unwrap_or(u32::MAX)),
        temperature: None,
        top_p: None,
        api_key: None,
        env_key,
        api_backend,
        auth_scheme,
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
        // `supported_in_api: false` hides a model unless the PRIMARY session is
        // an OAuth session (`ModelInfo::visible_for_auth`). Only `kimi-code`
        // rides that primary session, so only it may be gated on it. Every
        // other OAuth platform (claude-pro-max, openai-codex, github-copilot,
        // xai-grok) carries its OWN pooled credential, and its models only
        // enter the catalog once THAT provider is signed in — gating them on
        // the Kimi session would hide every model from a user who signed in
        // with only a Claude/ChatGPT/Copilot/Grok subscription.
        supported_in_api: platform != kigi_models::PlatformId::KimiCode,
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

    /// Whether a freshly-fetched platform entry shows up in the picker for a
    /// user whose PRIMARY session is NOT an OAuth session (`is_session_auth ==
    /// false`): an API-key user, or — the case that made this a ship-blocker —
    /// someone who signed in with ONLY a Claude Pro/Max, ChatGPT, Copilot, or
    /// Grok subscription. `ModelInfo::visible_for_auth` is the picker's real
    /// predicate (`agent/models.rs` → `available()`).
    fn visible_to_non_primary_session_user(entry: &crate::agent::config::ModelEntryConfig) -> bool {
        crate::agent::config::ModelEntry::from_config_entry(entry).visible_for_auth(false)
    }

    /// OpenAI-cycle e2e (mock wire): a polluted bare-id `/models` listing +
    /// a models.dev refresh produce a catalog with ONLY chat models, enriched
    /// context windows / efforts, and the Responses backend — the full
    /// "live list + documented metadata" contract.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn openai_listing_is_enriched_filtered_and_responses_backed() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer sk-oai"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "gpt-5-test", "object": "model", "owned_by": "openai" },
                    { "id": "whisper-1", "object": "model", "owned_by": "openai" },
                    { "id": "text-embedding-tiny", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "openai": { "models": {
                    "gpt-5-test": {
                        "name": "GPT-5 Test",
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "medium", "high"]}
                        ],
                        "limit": {"context": 400000, "output": 128000},
                        "modalities": {"input": ["text", "image"]},
                        "tool_call": true
                    },
                    // models.dev KNOWS embeddings models — membership alone
                    // must not admit them; the tool_call cut does.
                    "text-embedding-tiny": {
                        "limit": {"context": 8191}
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::OPENAI_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );

        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::OpenAi,
            "sk-oai",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");

        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["openai/gpt-5-test"],
            "pollution must be filtered: whisper (enrichment-unknown) AND \
             text-embedding-tiny (enrichment-known but not tool-calling)"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            400_000,
            "context window must come from enrichment (wire had none)"
        );
        assert_eq!(
            entry.api_backend,
            crate::sampling::ApiBackend::Responses,
            "OpenAI entries must use the Responses backend"
        );
        assert_eq!(entry.name.as_deref(), Some("GPT-5 Test"));
        assert!(entry.supports_reasoning_effort, "efforts must be filled");
        assert_eq!(
            entry
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high"]
        );
        assert!(
            entry
                .capabilities
                .contains(&kigi_models::ModelCapability::Thinking),
            "enrichment reasoning flag must derive the thinking capability"
        );
        assert_eq!(
            entry.env_key,
            Some(crate::agent::config::EnvKeys::single("OPENAI_API_KEY")),
            "entries carry the env NAME (never key values)"
        );
        assert!(
            cache_dir.path().join("models_dev_cache.json").exists(),
            "the refresh must be cached in the overridden dir"
        );
    }

    /// Anthropic-cycle e2e (mock wire): the Anthropic listing dialect —
    /// x-api-key + anthropic-version headers, ?limit=1000 — maps
    /// wire-served metadata (max_input_tokens, per-level effort
    /// capabilities) onto Messages-backed XApiKey entries, and enrichment
    /// fills a zero max_input_tokens without touching wire-served values.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn anthropic_listing_maps_wire_metadata_and_enrichment_fills_gaps() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::query_param("limit", "1000"))
            .and(wiremock::matchers::header("x-api-key", "sk-ant"))
            .and(wiremock::matchers::header(
                "anthropic-version",
                "2023-06-01",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    {
                        "id": "claude-opus-4-8",
                        "display_name": "Claude Opus 4.8",
                        "type": "model",
                        "max_input_tokens": 1_000_000,
                        "capabilities": {
                            "effort": {
                                "supported": true,
                                "low": {"supported": true},
                                "medium": {"supported": true},
                                "high": {"supported": true},
                                "xhigh": {"supported": true},
                                "max": {"supported": true}
                            },
                            "thinking": {"supported": true},
                            "image_input": {"supported": true}
                        }
                    },
                    {
                        "id": "claude-gap-test",
                        "type": "model",
                        "max_input_tokens": 0,
                        "capabilities": {
                            "effort": {"supported": false},
                            "thinking": {"supported": true},
                            "image_input": {"supported": false}
                        }
                    }
                ], "has_more": false }),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "anthropic": { "models": {
                    "claude-gap-test": {
                        "limit": {"context": 200000, "output": 64000},
                        "tool_call": true,
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "high"]}
                        ]
                    },
                    "claude-opus-4-8": {
                        "limit": {"context": 555},
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::ANTHROPIC_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );

        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Anthropic,
            "sk-ant",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");

        assert_eq!(result.models.len(), 2);
        let opus = &result.models[0];
        assert_eq!(opus.id.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(
            opus.context_window.get(),
            1_000_000,
            "wire max_input_tokens must WIN over enrichment (555)"
        );
        assert_eq!(opus.api_backend, crate::sampling::ApiBackend::Messages);
        assert_eq!(
            opus.auth_scheme,
            Some(kigi_sampler::AuthScheme::XApiKey),
            "anthropic entries must ride x-api-key at inference"
        );
        assert_eq!(
            opus.reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max"],
            "wire effort capabilities become the menu"
        );
        let gap = &result.models[1];
        assert_eq!(
            gap.context_window.get(),
            200_000,
            "a zero wire context must be filled by enrichment"
        );
        assert_eq!(
            gap.max_completion_tokens,
            Some(64_000),
            "the enrichment output cap must reach max_completion_tokens"
        );
        assert!(
            gap.reasoning_efforts.is_empty() && !gap.supports_reasoning_effort,
            "the wire's explicit effort decline must block enrichment's menu \
             (pre-4.6 models 400 on adaptive thinking); efforts={:?} supports={}",
            gap.reasoning_efforts,
            gap.supports_reasoning_effort,
        );
        let opus = &result.models[0];
        assert_eq!(
            opus.max_completion_tokens, None,
            "no wire/enrichment cap on this fixture entry — sampler default applies"
        );
        assert!(
            gap.capabilities
                .contains(&kigi_models::ModelCapability::Thinking),
            "wire thinking capability must survive"
        );
    }

    /// Claude Pro/Max OAuth fetch e2e (mock wire): `GET /v1/models?limit=1000`
    /// gated on the OAuth `Authorization: Bearer` + the oauth `anthropic-beta`
    /// (the OAuth listing contract) → anthropic listing → enriched from
    /// models.dev "anthropic" → keyed `claude-pro-max/<id>` on the Messages
    /// backend. The token is drawn from the claude-pro-max OAuth-session map,
    /// NOT an x-api-key.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn claude_pro_max_oauth_listing_is_bearer_gated_and_keyed() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::query_param("limit", "1000"))
            // OAuth listing: Bearer token + the oauth beta + anthropic-version.
            // NO x-api-key header (that is the API-key `anthropic` path).
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer sk-ant-oat-session",
            ))
            // The oauth beta is comma-joined; wiremock's exact `header` matcher
            // splits on commas, so assert both tokens via the multi-valued form.
            .and(wiremock::matchers::headers(
                "anthropic-beta",
                vec!["claude-code-20250219", "oauth-2025-04-20"],
            ))
            .and(wiremock::matchers::header(
                "anthropic-version",
                "2023-06-01",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    {
                        "id": "claude-opus-4-8",
                        "display_name": "Claude Opus 4.8",
                        "type": "model"
                    }
                ], "has_more": false }),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "anthropic": { "models": {
                    "claude-opus-4-8": {
                        "limit": {"context": 1000000, "output": 128000},
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::CLAUDE_OAUTH_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );

        let endpoints = crate::agent::config::EndpointsConfig::default();
        // The listing bearer comes from the claude-pro-max OAuth-session map,
        // never a Kimi session (auth=None) or an API key (keys empty).
        let mut oauth_tokens = OAuthSessionTokens::new();
        oauth_tokens.insert(
            kigi_models::PlatformId::ClaudeProMax,
            "sk-ant-oat-session".to_string(),
        );
        let keys = crate::agent::models::PlatformApiKeys::default();
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &oauth_tokens, &keys)
        })
        .await
        .unwrap()
        .expect("claude-pro-max oauth fetch must succeed");

        assert_eq!(result.models.len(), 1);
        let opus = &result.models[0];
        assert_eq!(
            opus.id.as_deref(),
            Some("claude-pro-max/claude-opus-4-8"),
            "the entry must key under the claude-pro-max platform"
        );
        assert_eq!(
            opus.api_backend,
            crate::sampling::ApiBackend::Messages,
            "claude-pro-max speaks the Messages wire"
        );
        assert_eq!(
            opus.auth_scheme, None,
            "OAuth Bearer entries carry no XApiKey auth scheme"
        );
        assert_eq!(
            opus.context_window.get(),
            1_000_000,
            "enrichment fills the context window from models.dev anthropic"
        );
        assert!(
            opus.supported_in_api,
            "claude-pro-max carries its OWN pooled credential — it must NOT be \
             gated on the primary (Kimi) session"
        );
        assert!(
            visible_to_non_primary_session_user(opus),
            "a Claude-Pro/Max-only user has no primary OAuth session; their \
             models must still appear in the picker"
        );
    }

    /// GitHub Copilot OAuth fetch e2e (mock wire): `GET /models` gated on the
    /// Bearer COPILOT token + the VS Code editor headers + X-GitHub-Api-Version
    /// returns a mix (a good completions model, a claude-4.x messages model, a
    /// gpt-5 responses-only model, and a disabled model). The catalog keeps ONLY
    /// the completions-served enabled tool-calling model, keyed `github-copilot/
    /// <id>` on the ChatCompletions backend, enriched from models.dev
    /// "github-copilot". The bearer is drawn from the copilot OAuth-session map,
    /// never a Kimi session or an API key.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn github_copilot_oauth_listing_filters_and_keys_completions_models() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            // Bearer COPILOT token + the editor identity + the Copilot API
            // version — the request MUST carry all of them or the mock 404s.
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer copilot-session-tok",
            ))
            .and(wiremock::matchers::header(
                "User-Agent",
                "GitHubCopilotChat/0.35.0",
            ))
            .and(wiremock::matchers::header(
                "Editor-Version",
                "vscode/1.107.0",
            ))
            .and(wiremock::matchers::header(
                "Copilot-Integration-Id",
                "vscode-chat",
            ))
            .and(wiremock::matchers::header(
                "X-GitHub-Api-Version",
                "2026-06-01",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    // KEPT: completions-served, selectable, tool-calling.
                    { "id": "gpt-4.1", "name": "GPT-4.1", "model_picker_enabled": true,
                      "policy": {"state": "enabled"},
                      "capabilities": {"supports": {"tool_calls": true}} },
                    // DROPPED: claude-4.x → anthropic-messages wire (excluded).
                    { "id": "claude-opus-4-8", "model_picker_enabled": true,
                      "capabilities": {"supports": {"tool_calls": true}} },
                    // DROPPED: gpt-5* → responses-only (excluded).
                    { "id": "gpt-5.2", "model_picker_enabled": true,
                      "capabilities": {"supports": {"tool_calls": true}} },
                    // DROPPED: policy disabled.
                    { "id": "gemini-3-flash-preview", "model_picker_enabled": true,
                      "policy": {"state": "disabled"} }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "github-copilot": { "models": {
                    "gpt-4.1": {
                        "limit": {"context": 128000, "output": 16384},
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::COPILOT_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );

        let endpoints = crate::agent::config::EndpointsConfig::default();
        // The listing bearer comes from the github-copilot OAuth-session map,
        // never a Kimi session (auth=None) or an API key (keys empty).
        let mut oauth_tokens = OAuthSessionTokens::new();
        oauth_tokens.insert(
            kigi_models::PlatformId::GithubCopilot,
            "copilot-session-tok".to_string(),
        );
        let keys = crate::agent::models::PlatformApiKeys::default();
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &oauth_tokens, &keys)
        })
        .await
        .unwrap()
        .expect("github-copilot oauth fetch must succeed");

        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["github-copilot/gpt-4.1"],
            "only the completions-served, enabled, tool-calling model survives \
             (claude-4.x, gpt-5, and the disabled model are dropped)"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.api_backend,
            crate::sampling::ApiBackend::ChatCompletions,
            "github-copilot speaks the ChatCompletions wire"
        );
        assert_eq!(
            entry.auth_scheme, None,
            "OAuth Bearer entries carry no XApiKey auth scheme"
        );
        assert_eq!(entry.name.as_deref(), Some("GPT-4.1"));
        assert_eq!(
            entry.context_window.get(),
            128_000,
            "context window comes from models.dev github-copilot enrichment"
        );
        assert!(
            entry.supported_in_api,
            "github-copilot carries its OWN pooled credential — it must NOT be \
             gated on the primary (Kimi) session"
        );
        assert!(
            visible_to_non_primary_session_user(entry),
            "a Copilot-only user has no primary OAuth session; their models \
             must still appear in the picker"
        );
    }

    /// openai-codex fetch: the catalog is HARDCODED, so the fetch path
    /// short-circuits BEFORE any HTTP — there is NO mock `/models` server, yet
    /// the fetch returns exactly the 4 compiled-in models keyed
    /// `openai-codex/<slug>` on the Responses backend, ctx 272000, each exposing
    /// its exact reasoning efforts (incl. the codex-only `xhigh`/`max`/`ultra`).
    /// A BOGUS base URL confirms no live `/models` request is attempted (it would
    /// otherwise fail against an unroutable host).
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn openai_codex_catalog_is_hardcoded_with_no_http_fetch() {
        // Unroutable base: if the fetch path tried a live `/models` request it
        // would error here; the hardcoded short-circuit ignores it for fetching.
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::CODEX_BASE_URL_ENV,
            "http://127.0.0.1:1/codex",
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        // The session token merely marks openai-codex "enabled"; it is unused by
        // the hardcoded path (no request rides it).
        let mut oauth_tokens = OAuthSessionTokens::new();
        oauth_tokens.insert(
            kigi_models::PlatformId::OpenaiCodex,
            "codex-session-tok".to_string(),
        );
        let keys = crate::agent::models::PlatformApiKeys::default();
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &oauth_tokens, &keys)
        })
        .await
        .unwrap()
        .expect("openai-codex hardcoded catalog fetch must succeed with no HTTP");

        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![
                "openai-codex/gpt-5.6-sol",
                "openai-codex/gpt-5.6-terra",
                "openai-codex/gpt-5.6-luna",
                "openai-codex/gpt-5.5",
            ],
            "exactly the 4 hardcoded models, keyed openai-codex/<slug>"
        );
        // Excluded models never appear.
        for absent in [
            "openai-codex/gpt-5.3-codex-spark",
            "openai-codex/gpt-5.4",
            "openai-codex/gpt-5.4-mini",
            "openai-codex/codex-auto-review",
        ] {
            assert!(
                !result
                    .models
                    .iter()
                    .any(|m| m.id.as_deref() == Some(absent)),
                "{absent} must be absent from the hardcoded catalog"
            );
        }
        let sol = &result.models[0];
        assert_eq!(
            sol.api_backend,
            crate::sampling::ApiBackend::Responses,
            "openai-codex speaks the Responses wire"
        );
        assert_eq!(sol.context_window.get(), 272_000);
        assert_eq!(sol.name.as_deref(), Some("GPT-5.6-Sol"));
        assert!(
            sol.supported_in_api,
            "openai-codex carries its OWN pooled credential — it must NOT be \
             gated on the primary (Kimi) session"
        );
        assert!(
            visible_to_non_primary_session_user(sol),
            "a ChatGPT/Codex-only user has no primary OAuth session; their \
             models must still appear in the picker"
        );
        assert!(sol.supports_reasoning_effort);
        assert_eq!(
            sol.reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max", "ultra"],
            "sol exposes the full codex effort menu incl. ultra"
        );
        // gpt-5.5 tops out at xhigh (no max/ultra).
        let five_five = result
            .models
            .iter()
            .find(|m| m.id.as_deref() == Some("openai-codex/gpt-5.5"))
            .unwrap();
        assert_eq!(
            five_five
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    /// DeepSeek-cycle e2e: bare OpenAI-shape listing + enrichment efforts
    /// (high/max) produce ChatCompletions entries whose sampler config
    /// speaks the DeepSeek thinking dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn deepseek_listing_enriches_and_maps_dialect() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer sk-ds"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "deepseek-v4-pro", "object": "model", "owned_by": "deepseek" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "deepseek": { "models": { "deepseek-v4-pro": {
                    "reasoning": true,
                    "reasoning_options": [
                        {"type": "toggle"},
                        {"type": "effort", "values": ["high", "max"]}
                    ],
                    "limit": {"context": 1000000, "output": 384000},
                    "tool_call": true
                }}}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::DEEPSEEK_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::DeepSeek,
            "sk-ds",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(result.models.len(), 1);
        let entry = &result.models[0];
        assert_eq!(entry.id.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(entry.context_window.get(), 1_000_000);
        assert_eq!(entry.max_completion_tokens, Some(384_000));
        assert_eq!(
            entry.api_backend,
            crate::sampling::ApiBackend::ChatCompletions
        );
        assert_eq!(
            entry
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["high", "max"]
        );

        // The managed id maps to the DeepSeek chat dialect; a BYOK entry
        // (no managed key) keeps the historical Kimi adaptation.
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("sk-ds".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(cfg.chat_compat, kigi_sampling_types::ChatCompat::DeepSeek);
        let mut byok = entry.clone();
        byok.id = Some("my-custom".into());
        let byok_entry = crate::agent::config::ModelEntry::from_config_entry(&byok);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("sk-x".into()),
            base_url: byok.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&byok_entry, creds, None);
        assert_eq!(cfg.chat_compat, kigi_sampling_types::ChatCompat::Kimi);
    }

    /// Groq-cycle e2e: pure pattern — polluted listing restricted to
    /// tool-calling enrichment models, Passthrough dialect (OpenAI-style
    /// reasoning_effort untouched on this wire).
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn groq_listing_restricts_and_maps_passthrough_dialect() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer gsk-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "llama-3.3-70b-versatile", "object": "model" },
                    { "id": "whisper-large-v3", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "groq": { "models": {
                    "llama-3.3-70b-versatile": {
                        "limit": {"context": 131072, "output": 32768},
                        "tool_call": true
                    },
                    "whisper-large-v3": { "limit": {"context": 448} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base =
            kigi_test_support::EnvGuard::set(kigi_models::GROQ_BASE_URL_ENV, platform_server.uri());
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Groq,
            "gsk-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["groq/llama-3.3-70b-versatile"],
            "whisper (enrichment-known, not tool-calling) must be dropped"
        );
        let entry = &result.models[0];
        assert_eq!(entry.context_window.get(), 131_072);
        assert_eq!(entry.max_completion_tokens, Some(32_768));
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("gsk-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough,
            "groq entries must leave OpenAI-style bodies untouched"
        );
    }

    /// Mistral-cycle e2e: embed pollution restricted away, and the Mistral
    /// dialect (strips stream_options, handles reasoning arrays) is mapped.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn mistral_listing_restricts_and_maps_mistral_dialect() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer msk-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "devstral-latest", "object": "model" },
                    { "id": "mistral-embed", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "mistral": { "models": {
                    "devstral-latest": {
                        "limit": {"context": 262144, "output": 65536},
                        "tool_call": true
                    },
                    "mistral-embed": { "limit": {"context": 8000} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::MISTRAL_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Mistral,
            "msk-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["mistral/devstral-latest"],
            "embed (enrichment-known, not tool-calling) must be dropped"
        );
        let entry = &result.models[0];
        assert_eq!(entry.context_window.get(), 262_144);
        assert_eq!(entry.max_completion_tokens, Some(65_536));
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("msk-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Mistral,
            "mistral entries use the Mistral dialect (StrictOpenAi behavior \
             plus the exactly-nine-alphanumeric tool-call id contract)"
        );
    }

    /// Fireworks-cycle e2e (Groq pattern): embedding pollution restricted
    /// away, Passthrough dialect, and Fireworks' deeply-slashed native ids
    /// (`accounts/fireworks/models/…`) round-trip through the managed key
    /// (`fireworks/accounts/fireworks/models/…`, first-slash split).
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn fireworks_listing_restricts_maps_dialect_and_keeps_slashed_ids() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer fw-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "accounts/fireworks/models/glm-5p2", "object": "model" },
                    { "id": "nomic-ai/nomic-embed-text-v1.5", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "fireworks-ai": { "models": {
                    "accounts/fireworks/models/glm-5p2": {
                        "limit": {"context": 1048575, "output": 65536},
                        "tool_call": true
                    },
                    "nomic-ai/nomic-embed-text-v1.5": { "limit": {"context": 8192} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::FIREWORKS_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Fireworks,
            "fw-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["fireworks/accounts/fireworks/models/glm-5p2"],
            "embedding model (enrichment-known, not tool-calling) must be dropped; \
             the slashed native id survives in the managed key"
        );
        let entry = &result.models[0];
        assert_eq!(entry.context_window.get(), 1_048_575);
        assert_eq!(entry.max_completion_tokens, Some(65_536));
        // The NATIVE slashed id rides the inference wire (`model` field);
        // the `fireworks/` managed-key prefix is internal routing only. A
        // regression here would 404 every Fireworks request.
        assert_eq!(
            entry.model, "accounts/fireworks/models/glm-5p2",
            "wire model must be the native id, not the managed key"
        );
        // The managed key parses back to (Fireworks, native-slashed-id).
        assert_eq!(
            kigi_models::parse_managed_model_key(entry.id.as_deref().unwrap()),
            Some((
                kigi_models::PlatformId::Fireworks,
                "accounts/fireworks/models/glm-5p2"
            ))
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("fw-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
        assert_eq!(
            cfg.model, "accounts/fireworks/models/glm-5p2",
            "the sampler wire model is the native slashed id end-to-end"
        );
    }

    /// Google/Gemini-cycle e2e: the OpenAI-compat listing returns
    /// `models/`-PREFIXED ids (Google's real shape), which the Google spec's
    /// `strip_listing_id_prefix` canonicalizes to the bare form the models.dev
    /// snapshot + chat endpoint use — WITHOUT the strip, restrict_to_enriched
    /// would silently drop every Gemini model. Embedding pollution is
    /// restricted away; Passthrough dialect; bare id on the wire.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn google_compat_listing_strips_prefix_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer gk-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    // Real Gemini compat shape: `models/`-prefixed ids.
                    { "id": "models/gemini-2.5-pro", "object": "model" },
                    { "id": "models/gemini-embedding-001", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "google": { "models": {
                    "gemini-2.5-pro": {
                        "limit": {"context": 1048576, "output": 65536},
                        "tool_call": true
                    },
                    "gemini-embedding-001": { "limit": {"context": 2048} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::GOOGLE_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Google,
            "gk-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["google/gemini-2.5-pro"],
            "prefix stripped → matches bare snapshot → kept as the bare \
             managed key; embedding (not tool-calling) dropped"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            1_048_576,
            "enrichment matched the bare id"
        );
        assert_eq!(entry.max_completion_tokens, Some(65_536));
        assert_eq!(
            entry.model, "gemini-2.5-pro",
            "the BARE Gemini id rides the wire (chat rejects the models/ prefix)"
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("gk-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// OpenRouter-cycle e2e: wire_serves_metadata=true — the listing itself
    /// carries `context_length`, so context comes from the WIRE with NO
    /// enrichment fetch and NO restriction (all models kept). The models.dev
    /// refresh is disabled to prove it is never consulted. Slashed ids
    /// round-trip; Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn openrouter_wire_metadata_needs_no_enrichment_and_keeps_all() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer or-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "anthropic/claude-opus-4.8", "context_length": 1000000,
                      "supported_parameters": ["reasoning_effort", "tools"] },
                    { "id": "openai/gpt-5.5", "context_length": 400000,
                      "supported_parameters": ["tools"] }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::OPENROUTER_BASE_URL_ENV,
            platform_server.uri(),
        );
        // Kill switch: proves enrichment is never fetched for a wire-served
        // platform (any attempt would need this URL).
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            "0",
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::OpenRouter,
            "or-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![
                "openrouter/anthropic/claude-opus-4.8",
                "openrouter/openai/gpt-5.5"
            ],
            "no restriction — all listed models kept; slashed ids in the key"
        );
        let opus = &result.models[0];
        assert_eq!(
            opus.context_window.get(),
            1_000_000,
            "context window comes from the wire listing, not enrichment"
        );
        assert_eq!(
            opus.model, "anthropic/claude-opus-4.8",
            "the native slashed id rides the wire"
        );
        assert_eq!(
            kigi_models::parse_managed_model_key(opus.id.as_deref().unwrap()),
            Some((
                kigi_models::PlatformId::OpenRouter,
                "anthropic/claude-opus-4.8"
            ))
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(opus);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("or-1".into()),
            base_url: opus.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// Together-cycle e2e: the listing is a BARE JSON ARRAY (no {data:[]}
    /// envelope) — parse_openai_listing tolerates it. models.dev enrichment
    /// (matching org/Model keys) supplies context + the tool-calling
    /// restriction drops non-chat models; Passthrough dialect; slashed id.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn together_bare_array_listing_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer tg-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // BARE ARRAY, not {object:list,data:[]}.
                serde_json::json!([
                    { "id": "Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8", "object": "model",
                      "type": "chat", "context_length": 262144 },
                    { "id": "togethercomputer/m2-bert-80M-8k-retrieval", "object": "model",
                      "type": "embedding" }
                ]),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "togetherai": { "models": {
                    "Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8": {
                        "limit": {"context": 262144, "output": 32768},
                        "tool_call": true
                    },
                    "togethercomputer/m2-bert-80M-8k-retrieval": { "limit": {"context": 8192} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::TOGETHER_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Together,
            "tg-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["together/Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8"],
            "bare array parsed; embedding (not tool-calling) dropped; slashed id in key"
        );
        let entry = &result.models[0];
        assert_eq!(entry.context_window.get(), 262_144);
        assert_eq!(entry.max_completion_tokens, Some(32_768));
        assert_eq!(
            entry.model, "Qwen/Qwen3-Coder-480B-A35B-Instruct-FP8",
            "the native slashed id rides the wire"
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("tg-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// Cerebras-cycle e2e: enrich WITHOUT restrict (the unpolluted-catalog
    /// path). A minimal listing (bare ids, no context) → every live model is
    /// kept; the enrichment-known one gains context + an effort menu, the
    /// unknown one keeps the default context. Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn cerebras_enriches_without_restrict_keeping_all_models() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header("Authorization", "Bearer cb-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // Minimal Cerebras listing: ids only, no context.
                serde_json::json!({ "data": [
                    { "id": "gpt-oss-120b", "object": "model" },
                    { "id": "brand-new-cerebras-model", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "cerebras": { "models": {
                    "gpt-oss-120b": {
                        "limit": {"context": 131072, "output": 40960},
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "medium", "high"]}
                        ],
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::CEREBRAS_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Cerebras,
            "cb-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        // restrict=false → BOTH the known and the unknown model are kept.
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["cerebras/gpt-oss-120b", "cerebras/brand-new-cerebras-model"],
            "enrich-without-restrict keeps every live model"
        );
        let known = &result.models[0];
        assert_eq!(known.context_window.get(), 131_072, "known model enriched");
        assert_eq!(known.max_completion_tokens, Some(40_960));
        assert!(
            known.supports_reasoning_effort,
            "enrichment effort menu → selectable levels"
        );
        assert_eq!(
            known
                .reasoning_efforts
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high"],
            "effort menu comes from enrichment"
        );
        let unknown = &result.models[1];
        assert_eq!(
            unknown.context_window.get(),
            DEFAULT_CONTEXT_WINDOW,
            "an enrichment-unknown model keeps the default context (not dropped)"
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(known);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("cb-1".into()),
            base_url: known.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::StrictOpenAi,
            "Cerebras uses the StrictOpenAi dialect (strict validator strips stream_options)"
        );
    }

    /// NVIDIA-cycle e2e: polluted listing (embedding/image models) restricted
    /// to tool-calling enrichment-known chat models; slashed org/model ids;
    /// StrictOpenAi dialect (strips stream_options — some NIM models reject it).
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn nvidia_restricts_slashed_ids_and_maps_strict_dialect() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer nvapi-1",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "meta/llama-3.3-70b-instruct", "object": "model" },
                    { "id": "baai/bge-m3", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "nvidia": { "models": {
                    "meta/llama-3.3-70b-instruct": {
                        "limit": {"context": 128000, "output": 32768},
                        "tool_call": true
                    },
                    "baai/bge-m3": { "limit": {"context": 8192} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::NVIDIA_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Nvidia,
            "nvapi-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["nvidia/meta/llama-3.3-70b-instruct"],
            "bge-m3 embedding (not tool-calling) dropped; slashed org/model id kept"
        );
        let entry = &result.models[0];
        assert_eq!(entry.context_window.get(), 128_000);
        assert_eq!(entry.max_completion_tokens, Some(32_768));
        assert_eq!(
            entry.model, "meta/llama-3.3-70b-instruct",
            "the native slashed id rides the wire"
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("nvapi-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::StrictOpenAi,
            "NVIDIA uses StrictOpenAi (strips stream_options for NIM models that reject it)"
        );
    }

    /// Vercel-cycle e2e: the gateway lists creator/model ids (matching the
    /// models.dev "vercel" keys); enrichment supplies context (the wire uses
    /// `context_window`, not the WireModel `context_length`); the restriction
    /// drops non-chat types; slashed id; Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn vercel_gateway_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // Vercel serves context under `context_window` (ignored by
                // WireModel) — enrichment supplies the real context.
                serde_json::json!({ "data": [
                    // A DISTINCT (wrong) context_window that WireModel ignores —
                    // so asserting the enrichment value below proves the source.
                    { "id": "openai/gpt-5.5", "object": "model",
                      "type": "language", "context_window": 999 },
                    { "id": "voyage/rerank-2.5", "object": "model", "type": "embedding" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "vercel": { "models": {
                    "openai/gpt-5.5": {
                        "limit": {"context": 400000, "output": 128000},
                        "tool_call": true
                    },
                    "voyage/rerank-2.5": { "limit": {"context": 32000} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::VERCEL_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Vercel,
            "vg-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["vercel-ai-gateway/openai/gpt-5.5"],
            "rerank (not tool-calling) dropped; creator/model id kept under the platform key"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            400_000,
            "context comes from enrichment (wire used context_window, not context_length)"
        );
        assert_eq!(entry.max_completion_tokens, Some(128_000));
        assert_eq!(
            entry.model, "openai/gpt-5.5",
            "the creator/model id rides the wire"
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("vg-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// xAI-cycle e2e: /v1/models is minimal (bare grok ids, no context), so
    /// enrichment supplies context/limits; the restriction drops the
    /// non-tool-calling grok-imagine-* generators; bare id round-trips under
    /// the `xai/` key; Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn xai_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // xAI /v1/models is minimal: id/object only, NO context field —
                // so a non-zero context below can only come from enrichment.
                serde_json::json!({ "data": [
                    { "id": "grok-4.5", "object": "model" },
                    { "id": "grok-imagine-image", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "xai": { "models": {
                    "grok-4.5": {
                        "limit": {"context": 500000, "output": 128000},
                        "tool_call": true
                    },
                    // present in enrichment too, but not tool-calling → dropped.
                    "grok-imagine-image": { "limit": {"context": 8000} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base =
            kigi_test_support::EnvGuard::set(kigi_models::XAI_BASE_URL_ENV, platform_server.uri());
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Xai,
            "xai-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["xai/grok-4.5"],
            "grok-imagine-image (not tool-calling) dropped; bare id kept under the xai key"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            500_000,
            "context comes from enrichment (the wire listing carries none)"
        );
        assert_eq!(entry.max_completion_tokens, Some(128_000));
        assert_eq!(entry.model, "grok-4.5", "the bare grok id rides the wire");
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("xai-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// Qwen-Token-Plan e2e: DashScope compatible-mode /models is auth-gated and
    /// minimal (ids only), so enrichment supplies context; the restriction drops
    /// the non-tool-calling qwen-image / wan generators the token plan lists;
    /// bare id round-trips under the platform key; Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn qwen_token_plan_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // DashScope /models is minimal: id/object only, NO context.
                serde_json::json!({ "data": [
                    { "id": "qwen3.7-max", "object": "model" },
                    { "id": "qwen-image-2.0", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "alibaba-token-plan": { "models": {
                    "qwen3.7-max": {
                        "limit": {"context": 1000000, "output": 32768},
                        "tool_call": true
                    },
                    // present in enrichment too, but not tool-calling → dropped.
                    "qwen-image-2.0": { "limit": {"context": 8192} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::QWEN_TOKEN_PLAN_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::QwenTokenPlan,
            "qtp-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["qwen-token-plan/qwen3.7-max"],
            "qwen-image (not tool-calling) dropped; bare id kept under the platform key"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            1_000_000,
            "context comes from enrichment (the wire listing carries none)"
        );
        assert_eq!(entry.max_completion_tokens, Some(32_768));
        assert_eq!(entry.model, "qwen3.7-max", "the bare id rides the wire");
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("qtp-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// Kimi-For-Coding static-key e2e: same endpoint + Kimi dialect as the OAuth
    /// kimi-code platform, keyed by KIMI_API_KEY. Kimi's /models serves its own
    /// metadata (wire_serves_metadata), so context comes from the WIRE and the
    /// models.dev fetch is skipped entirely (all enabled platforms self-serve);
    /// no restriction; Kimi dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn kimi_coding_static_key_uses_wire_metadata_and_kimi_dialect() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "k3", "object": "model", "context_length": 1_048_576,
                      "supports_reasoning": true }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        // Point models.dev at an ALWAYS-500 server: enrichment must NOT be
        // fetched for an all-wire-metadata provider, so this is never hit.
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(0)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base =
            kigi_test_support::EnvGuard::set(kigi_env::CODE_BASE_URL_ENV, platform_server.uri());
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::KimiCoding,
            "kc-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["kimi-coding/k3"],
            "wire model kept under the platform key (no restriction)"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            1_048_576,
            "context comes from the WIRE (wire_serves_metadata); enrichment was skipped"
        );
        assert_eq!(entry.model, "k3");
        // The managed key must parse back to KimiCoding — so the Kimi dialect
        // below is a real attribution, not the default-dialect fallback that a
        // failed parse would also yield.
        assert_eq!(
            kigi_models::parse_managed_model_key(entry.id.as_deref().unwrap()),
            Some((kigi_models::PlatformId::KimiCoding, "k3")),
        );
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("kc-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(cfg.chat_compat, kigi_sampling_types::ChatCompat::Kimi);
    }

    /// Z.AI-cycle e2e: OpenAI-compatible GLM coding plan (per Pi, plain
    /// completions — no thinking dialect). /models is auth-gated + minimal, so
    /// enrichment supplies context; restrict drops any wire model absent from
    /// the models.dev "zai-coding-plan" snapshot; bare id round-trips under the
    /// zai key; Passthrough dialect.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn zai_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "glm-5.2", "object": "model" },
                    // not in the enrichment snapshot → dropped by restrict.
                    { "id": "glm-experimental-unlisted", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "zai-coding-plan": { "models": {
                    "glm-5.2": {
                        "limit": {"context": 200000, "output": 128000},
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base =
            kigi_test_support::EnvGuard::set(kigi_models::ZAI_BASE_URL_ENV, platform_server.uri());
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Zai,
            "zai-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["zai/glm-5.2"],
            "the non-enriched wire model is dropped by restrict_to_enriched"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            200_000,
            "context comes from enrichment (the wire listing carries none)"
        );
        assert_eq!(entry.max_completion_tokens, Some(128_000));
        assert_eq!(entry.model, "glm-5.2");
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("zai-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// Xiaomi-cycle e2e: OpenAI-compatible MiMo (per Pi, plain completions).
    /// /models is auth-gated + minimal; enrichment supplies context; restrict's
    /// tool_call cut drops the mimo TTS models (tool_call=false) the token plan
    /// lists; bare id round-trips under the xiaomi key; Passthrough.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn xiaomi_enriches_restricts_and_maps_passthrough() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "mimo-v2.5-pro", "object": "model" },
                    // a TTS model (tool_call=false in enrichment) → dropped.
                    { "id": "mimo-v2-tts", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "xiaomi": { "models": {
                    "mimo-v2.5-pro": {
                        "limit": {"context": 256000, "output": 32768},
                        "tool_call": true
                    },
                    "mimo-v2-tts": { "limit": {"context": 8192}, "tool_call": false }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::XIAOMI_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Xiaomi,
            "xm-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["xiaomi/mimo-v2.5-pro"],
            "the TTS model (tool_call=false) is dropped by restrict_to_enriched"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            256_000,
            "context comes from enrichment (the wire listing carries none)"
        );
        assert_eq!(entry.max_completion_tokens, Some(32_768));
        assert_eq!(entry.model, "mimo-v2.5-pro");
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("xm-1".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::ApiKey,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
        );
    }

    /// MiniMax e2e: Pi drives MiniMax through its Anthropic-compatible surface,
    /// so Kigi uses the Anthropic listing (x-api-key + anthropic-version,
    /// ?limit=1000) + Messages wire. /anthropic/v1/models is minimal, so
    /// enrichment supplies context; restrict=false keeps the clean MiniMax-M*
    /// catalog; id round-trips under the minimax key.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn minimax_anthropic_listing_enriches_and_keys_under_platform() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            // x-api-key auth (Anthropic key header) must be present.
            .and(wiremock::matchers::header("x-api-key", "mm-1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // Anthropic listing envelope; minimal (id only) → enrichment fills.
                serde_json::json!({ "data": [
                    { "id": "MiniMax-M2.5", "type": "model" }
                ], "has_more": false }),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "minimax": { "models": {
                    "MiniMax-M2.5": {
                        "limit": {"context": 204800, "output": 131072},
                        "tool_call": true
                    }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::MINIMAX_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );
        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::test_single(
            kigi_models::PlatformId::Minimax,
            "mm-1",
        );
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &Default::default(), &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");
        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["minimax/MiniMax-M2.5"],
            "the Anthropic-listed model is keyed under the minimax platform"
        );
        let entry = &result.models[0];
        assert_eq!(
            entry.context_window.get(),
            204_800,
            "context comes from enrichment (the wire listing carries none)"
        );
        assert_eq!(entry.max_completion_tokens, Some(131_072));
        assert_eq!(entry.model, "MiniMax-M2.5");
        assert_eq!(
            kigi_models::parse_managed_model_key(entry.id.as_deref().unwrap()),
            Some((kigi_models::PlatformId::Minimax, "MiniMax-M2.5")),
        );
    }

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
    /// xai-grok OAuth-cycle e2e (mock wire): the generic device-code OAuth
    /// bearer (`grok-oauth-tok`, resolved from a stored `oauth/xai` session —
    /// mocked here as the token map) fetches `GET {base}/models` against the
    /// platform's OWN base (api.x.ai/v1 via its env), enriches from models.dev
    /// "xai", restricts to tool-calling chat models, and keys each entry under
    /// `xai-grok/<id>` with the Passthrough dialect — NOT the API-key `xai/`.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn xai_grok_oauth_listing_is_enriched_restricted_and_keyed() {
        let platform_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer grok-oauth-tok",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [
                    { "id": "grok-4.5", "object": "model" },
                    // enrichment-known but NOT tool-calling → dropped by restrict.
                    { "id": "grok-2-image", "object": "model" }
                ]}),
            ))
            .expect(1)
            .mount(&platform_server)
            .await;
        let modelsdev_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "xai": { "models": {
                    "grok-4.5": {
                        "name": "Grok 4.5",
                        "reasoning": true,
                        "limit": {"context": 256000, "output": 64000},
                        "tool_call": true
                    },
                    "grok-2-image": { "limit": {"context": 8192} }
                }}}),
            ))
            .expect(1)
            .mount(&modelsdev_server)
            .await;
        let cache_dir = tempfile::tempdir().unwrap();
        let _base = kigi_test_support::EnvGuard::set(
            kigi_models::XAI_GROK_BASE_URL_ENV,
            platform_server.uri(),
        );
        let _mdev = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_URL_ENV,
            format!("{}/api.json", modelsdev_server.uri()),
        );
        let _mdev_cache = kigi_test_support::EnvGuard::set(
            crate::agent::enrichment_fetch::MODELS_DEV_CACHE_DIR_ENV,
            cache_dir.path(),
        );

        let endpoints = crate::agent::config::EndpointsConfig::default();
        let keys = crate::agent::models::PlatformApiKeys::default();
        let mut tokens = OAuthSessionTokens::new();
        tokens.insert(kigi_models::PlatformId::XaiGrok, "grok-oauth-tok".into());
        let result = tokio::task::spawn_blocking(move || {
            fetch_platform_models_blocking(&endpoints, None, &tokens, &keys)
        })
        .await
        .unwrap()
        .expect("fetch must succeed");

        assert_eq!(
            result
                .models
                .iter()
                .map(|m| m.id.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["xai-grok/grok-4.5"],
            "grok-2-image (enrichment-known, not tool-calling) must be dropped; \
             the surviving model is keyed under xai-grok/ (not xai/)"
        );
        let entry = &result.models[0];
        assert_eq!(entry.model, "grok-4.5");
        assert_eq!(
            entry.base_url,
            platform_server.uri(),
            "xai-grok fetches against its OWN base (its base env), not proxy_url"
        );
        assert_eq!(
            entry.context_window.get(),
            256_000,
            "context window must come from models.dev \"xai\" enrichment"
        );
        assert_eq!(entry.max_completion_tokens, Some(64_000));
        assert_eq!(
            entry.api_backend,
            crate::sampling::ApiBackend::ChatCompletions
        );
        assert_eq!(entry.name.as_deref(), Some("Grok 4.5"));
        assert!(
            entry.env_key.is_none(),
            "an OAuth channel carries no api-key env"
        );
        assert!(
            entry.supported_in_api,
            "xai-grok carries its OWN pooled credential — it must NOT be gated \
             on the primary (Kimi) session"
        );
        assert!(
            visible_to_non_primary_session_user(entry),
            "a Grok-only user has no primary OAuth session; their models must \
             still appear in the picker"
        );
        // Passthrough dialect (identical to the API-key xai wire).
        let model_entry = crate::agent::config::ModelEntry::from_config_entry(entry);
        let creds = crate::agent::config::ResolvedCredentials {
            api_key: Some("grok-oauth-tok".into()),
            base_url: entry.base_url.clone(),
            auth_type: kigi_chat_state::AuthType::SessionToken,
            auth_scheme: Default::default(),
        };
        let cfg = crate::agent::config::sampling_config_for_model(&model_entry, creds, None);
        assert_eq!(
            cfg.chat_compat,
            kigi_sampling_types::ChatCompat::Passthrough
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
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::OpenAi, &cfg),
            "https://api.openai.com/v1/models"
        );
        // xai-grok is uses_oauth but carries an OAuthConfig → its OWN base
        // (api.x.ai/v1), NOT the Kimi subscription proxy.
        assert_eq!(
            platform_models_url(kigi_models::PlatformId::XaiGrok, &cfg),
            "https://api.x.ai/v1/models"
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
            &Default::default(),
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
            &Default::default(),
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
                &Default::default(),
                &PlatformApiKeys::default(),
            ),
            "https://models.acme.com/v1/models"
        );
    }

    /// `stored_oauth_platforms` maps auth.json `oauth/<provider>` scopes to
    /// their platforms — presence only, no AuthManager, no refresh. Missing
    /// or empty auth.json → empty (self-cleaning TempDir).
    #[test]
    fn stored_oauth_platforms_reads_scopes_from_auth_json() {
        let home = tempfile::tempdir().expect("tempdir");
        assert!(
            stored_oauth_platforms(home.path()).is_empty(),
            "no auth.json → no stored oauth platforms"
        );

        let mut store = std::collections::BTreeMap::new();
        store.insert(
            "oauth/claude-pro-max".to_string(),
            crate::auth::KimiAuth::test_default(),
        );
        // A platform API-key scope must NOT count as a stored OAuth session.
        store.insert(
            "deepseek".to_string(),
            crate::auth::KimiAuth::test_default(),
        );
        std::fs::write(
            home.path().join("auth.json"),
            serde_json::to_string(&store).expect("serialize store"),
        )
        .expect("write auth.json");

        assert_eq!(
            stored_oauth_platforms(home.path()),
            vec![kigi_models::PlatformId::ClaudeProMax],
        );

        // The stub map carries the same platform set with EMPTY values.
        let stubs = stored_oauth_token_stubs(home.path());
        assert_eq!(stubs.len(), 1);
        assert_eq!(stubs[&kigi_models::PlatformId::ClaudeProMax], "");
    }

    /// The origin computed from presence-only stubs equals the origin the
    /// fetch path computes from REAL resolved tokens — the whole point of
    /// the stubs (a claude-inclusive cached catalog must load at startup).
    #[test]
    fn stub_origin_matches_real_token_origin() {
        use crate::agent::config::EndpointsConfig;
        use crate::agent::models::{ModelFetchAuth, PlatformApiKeys};
        let cfg = EndpointsConfig::default();
        let mut real = OAuthSessionTokens::new();
        real.insert(
            kigi_models::PlatformId::ClaudeProMax,
            "live-bearer".to_string(),
        );
        let mut stubs = OAuthSessionTokens::new();
        stubs.insert(kigi_models::PlatformId::ClaudeProMax, String::new());
        let with_real = models_fetch_origin(
            &cfg,
            ModelFetchAuth::Platforms,
            false,
            &real,
            &PlatformApiKeys::default(),
        );
        let with_stubs = models_fetch_origin(
            &cfg,
            ModelFetchAuth::Platforms,
            false,
            &stubs,
            &PlatformApiKeys::default(),
        );
        assert_eq!(with_real, with_stubs);
        assert!(with_real.contains("claude-pro-max="));
        assert!(
            !with_real.contains("live-bearer"),
            "origin never embeds tokens"
        );
    }
}
