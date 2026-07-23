//! Sampler configuration types.
//!
//! [`SamplerConfig`] deliberately does **not** alias
//! `kigi_sampling_types::SamplingConfig`, which would drag shell-specific
//! types (`kigi-tools`, etc.) into this crate's dependency graph.

use indexmap::IndexMap;
use kigi_sampling_types::{
    ApiBackend, CompactionAtTokens, CompactionsRemaining, DoomLoopRecoveryPolicy, ReasoningEffort,
};
use serde::{Deserialize, Serialize};

use crate::attribution::SharedAttributionCallback;
use crate::retry::{DEFAULT_MAX_RETRIES, RATE_LIMIT_RETRY_THRESHOLD};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    #[default]
    Bearer,
    XApiKey,
}

/// All knobs that control a single sampling request. The session owns one per
/// active model and passes it — or a per-request override — to the actor on
/// every submit.
///
/// `kigi-shell` builds it by composing chat-state's
/// `kigi_sampling_types::SamplingConfig` with `Credentials`; see
/// `agent::config::sampling_config_for_model` and
/// `session::acp_session::SessionActor::reconstruct_full_config`. URL-derived
/// request headers are folded into [`Self::extra_headers`] by
/// `agent::config::inject_url_derived_headers` before the config reaches the
/// actor. Auth is selected separately via `auth_scheme`, while `api_backend`
/// controls only the request/response protocol shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplerConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub api_backend: ApiBackend,
    #[serde(default)]
    pub auth_scheme: AuthScheme,
    /// Claude Pro/Max OAuth adaptation (claude-pro-max only). When true the
    /// Messages request carries the OAuth identity headers (`anthropic-beta`
    /// oauth, `claude-cli` User-Agent, `x-app: cli`) and its system prompt is
    /// prefixed with the required "You are Claude Code…" line. Gated so the
    /// API-key `anthropic` + `minimax` Messages requests stay byte-identical.
    #[serde(default)]
    pub anthropic_oauth: bool,
    /// GitHub Copilot ChatCompletions adaptation (github-copilot only). When
    /// true the request carries the VS Code Copilot editor-identity headers
    /// (User-Agent `GitHubCopilotChat/…`, `Editor-Version`,
    /// `Editor-Plugin-Version`, `Copilot-Integration-Id`) plus `X-Initiator:
    /// user`. Gated so every other ChatCompletions provider (groq, …) stays
    /// byte-identical.
    #[serde(default)]
    pub github_copilot: bool,
    /// ChatGPT/Codex Responses adaptation (openai-codex only). When true the
    /// `/codex/responses` request carries the Codex identity headers
    /// (`chatgpt-account-id` derived per-request from the bearer JWT,
    /// `originator: codex_cli_rs`, `OpenAI-Beta: responses=experimental`, a codex
    /// `User-Agent`). Gated so the API-key `openai` Responses requests stay
    /// byte-identical (`store: false` is already the shared Responses default).
    #[serde(default)]
    pub openai_codex: bool,
    /// Extra request headers applied verbatim. The sampler never inspects
    /// the URL to derive headers; callers (the session) inject proxy auth
    /// and other access headers here before constructing the config.
    pub extra_headers: IndexMap<String, String>,
    /// Total context window size in tokens. The sampler does not enforce
    /// it; it is informational metadata used by the session for compaction
    /// decisions.
    pub context_window: u64,
    pub force_http1: bool,
    pub max_retries: Option<u32>,
    pub stream_tool_calls: bool,
    pub idle_timeout_secs: Option<u64>,

    pub reasoning_effort: Option<ReasoningEffort>,
    /// ChatCompletions body-adaptation dialect. BYOK and custom endpoints fall
    /// back to Kimi behavior; `serde(default)` keeps persisted configs that
    /// lack the key parseable.
    #[serde(default)]
    pub chat_compat: kigi_sampling_types::ChatCompat,

    /// Client identity for the User-Agent header (`kigi/{version}` plus an
    /// optional origin product). User-Agent and `extra_headers` are the only
    /// identity signals this crate puts on the wire.
    pub origin_client: Option<OriginClientInfo>,

    /// Hook invoked at every UNAUTHORIZED (401) response site, receiving the
    /// bearer that was actually sent on the wire — typically joined against a
    /// live credential source to tell a stale token apart from a live token the
    /// server rejected. `None` is a no-op and the 401 arm still yields
    /// `SamplingError::Auth`.
    ///
    /// `Arc<dyn Trait>` is not serializable, so a config round-tripped through
    /// serde comes back without the callback. Callers deserializing a
    /// `SamplerConfig` from disk must re-attach it before
    /// [`crate::SamplingClient::new`], or 401 attribution is silently disabled
    /// for the rebuilt client.
    #[serde(skip)]
    pub attribution_callback: Option<SharedAttributionCallback>,

    /// Live bearer resolve per request. `None` uses construction-time `api_key`.
    #[serde(skip)]
    pub bearer_resolver: Option<SharedBearerResolver>,

    #[serde(default)]
    pub supports_backend_search: bool,

    /// Per-model config for the `x-compactions-remaining` header; `None` disables it.
    #[serde(default)]
    pub compactions_remaining: Option<CompactionsRemaining>,

    /// Per-model config for the `x-compaction-at` header; `None` disables it.
    #[serde(default)]
    pub compaction_at_tokens: Option<CompactionAtTokens>,

    /// Server-side doom-loop check policy; `None` disables it. When set, the
    /// client sends the opt-in `x-kigi-doom-loop-check` header on streaming
    /// Responses API requests and absorbs the reported trigger events. Unlike
    /// the environment headers in [`Self::extra_headers`], this header gates
    /// the client's own decode behavior, so it lives with the decoder.
    #[serde(default)]
    pub doom_loop_recovery: Option<DoomLoopRecoveryPolicy>,

    /// Per-request header injector (e.g. OTel traceparent). Called in `post()`.
    #[serde(skip)]
    pub header_injector: Option<SharedHeaderInjector>,
}

impl Default for SamplerConfig {
    /// Empty defaults so callers can spell `..Default::default()` and a new
    /// field does not ripple through every struct literal.
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: String::new(),
            model: String::new(),
            max_completion_tokens: None,
            temperature: None,
            chat_compat: kigi_sampling_types::ChatCompat::default(),
            top_p: None,
            api_backend: ApiBackend::default(),
            auth_scheme: AuthScheme::default(),
            anthropic_oauth: false,
            github_copilot: false,
            openai_codex: false,
            extra_headers: IndexMap::new(),
            context_window: 0,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            origin_client: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        }
    }
}

/// Cheap sync read of the current bearer for [`SamplerConfig::bearer_resolver`].
pub trait BearerResolver: Send + Sync + std::fmt::Debug {
    fn current_bearer(&self) -> Option<String>;
}

pub type SharedBearerResolver = std::sync::Arc<dyn BearerResolver>;

/// Per-request header injection (e.g. OTel `traceparent`).
pub trait HeaderInjector: Send + Sync + std::fmt::Debug {
    fn inject(&self, headers: &mut reqwest::header::HeaderMap);
}

pub type SharedHeaderInjector = std::sync::Arc<dyn HeaderInjector>;

/// Retry knobs for the sampler's internal transport-error retry loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_retries: u32,
    /// After this many rate-limit (429) retries, escalate to the caller.
    /// Lower than `max_retries` because rate-limit waits can be long.
    pub rate_limit_retry_threshold: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            rate_limit_retry_threshold: RATE_LIMIT_RETRY_THRESHOLD,
        }
    }
}

/// Identity of the client that originated the request, used for
/// User-Agent rendering. The shell layer composes this with platform
/// info into a final UA string.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OriginClientInfo {
    pub product: String,
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(
            policy.rate_limit_retry_threshold,
            RATE_LIMIT_RETRY_THRESHOLD
        );
    }

    #[test]
    fn config_without_doom_loop_recovery_deserializes_to_none() {
        let mut stripped = serde_json::to_value(SamplerConfig::default()).unwrap();
        stripped
            .as_object_mut()
            .unwrap()
            .remove("doom_loop_recovery");
        let config: SamplerConfig = serde_json::from_value(stripped).unwrap();
        assert!(config.doom_loop_recovery.is_none());

        let with_policy = SamplerConfig {
            doom_loop_recovery: Some(DoomLoopRecoveryPolicy {
                max_threshold: 8,
                max_retries: 2,
            }),
            ..Default::default()
        };
        let round_tripped: SamplerConfig =
            serde_json::from_value(serde_json::to_value(&with_policy).unwrap()).unwrap();
        assert_eq!(
            round_tripped.doom_loop_recovery,
            with_policy.doom_loop_recovery
        );
    }
}
