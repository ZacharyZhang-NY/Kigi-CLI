//! Kimi model catalog primitives (PRD F2/F4).
//!
//! This crate owns:
//! - the fixed three-platform registry ([`PlatformId`]): the Kimi Code
//!   subscription channel plus the two Moonshot open platforms;
//! - the `GET {base}/models` wire contract ([`WireModel`]) and the capability
//!   derivation ported from kimi-cli `auth/platforms.py`;
//! - the managed catalog key format `{platform_id}/{model_id}`;
//! - the bundled OFFLINE-LAST-RESORT fallback catalog
//!   (`default_models.json`), used only when the live `/models` sync fails
//!   AND no disk cache is usable. Every id in that file is sourced from
//!   kimi-cli 1.49.0 (see the module docs on [`DEFAULT_MODELS_JSON`]).
//!
//! At runtime each model is resolved via:
//!   CLI flag > ENV var > config.toml > server-delivered > these defaults

use std::sync::LazyLock;

// ── Platform registry (PRD F2) ──────────────────────────────────────────────

/// Env var holding the moonshot-cn API key (wins over the generic name).
pub const MOONSHOT_CN_API_KEY_ENV: &str = "KIGI_MOONSHOT_CN_API_KEY";
/// Env var holding the moonshot-ai API key (wins over the generic name).
pub const MOONSHOT_AI_API_KEY_ENV: &str = "KIGI_MOONSHOT_AI_API_KEY";
/// Generic moonshot API key env var, applied to BOTH open platforms when the
/// platform-scoped name is unset.
pub const MOONSHOT_API_KEY_ENV: &str = "KIGI_MOONSHOT_API_KEY";
/// Base-URL override for moonshot-cn (dev/test escape hatch mirroring
/// `KIGI_CODE_BASE_URL`; production uses the compiled default).
pub const MOONSHOT_CN_BASE_URL_ENV: &str = "KIGI_MOONSHOT_CN_BASE_URL";
/// Base-URL override for moonshot-ai (dev/test escape hatch mirroring
/// `KIGI_CODE_BASE_URL`; production uses the compiled default).
pub const MOONSHOT_AI_BASE_URL_ENV: &str = "KIGI_MOONSHOT_AI_BASE_URL";

/// Env override when set and non-blank, else the compiled default.
fn env_or(var: &str, compiled: &str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => compiled.to_string(),
    }
}

/// The fixed platform registry. Kigi talks to exactly these three model
/// providers; there is no dynamic provider registration (PRD F2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformId {
    /// Kimi Code subscription (OAuth bearer from the F1 device flow).
    KimiCode,
    /// Moonshot AI open platform, api.moonshot.cn (API key).
    MoonshotCn,
    /// Moonshot AI open platform, api.moonshot.ai (API key).
    MoonshotAi,
}

impl PlatformId {
    /// All platforms, in catalog precedence order: the subscription channel
    /// first so "default model = first list item" favors it when present.
    pub const ALL: [PlatformId; 3] = [Self::KimiCode, Self::MoonshotCn, Self::MoonshotAi];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::KimiCode => "kimi-code",
            Self::MoonshotCn => "moonshot-cn",
            Self::MoonshotAi => "moonshot-ai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kimi-code" => Some(Self::KimiCode),
            "moonshot-cn" => Some(Self::MoonshotCn),
            "moonshot-ai" => Some(Self::MoonshotAi),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::KimiCode => "Kimi Code",
            Self::MoonshotCn => "Moonshot AI Open Platform (moonshot.cn)",
            Self::MoonshotAi => "Moonshot AI Open Platform (moonshot.ai)",
        }
    }

    /// Inference/model-listing base URL. The subscription base honors the
    /// `KIGI_CODE_BASE_URL` override via [`kigi_env::coding_api_base_url`];
    /// the open-platform bases are fixed in production, with
    /// `KIGI_MOONSHOT_{CN,AI}_BASE_URL` as dev/test overrides.
    pub fn base_url(self) -> String {
        match self {
            Self::KimiCode => kigi_env::coding_api_base_url(),
            Self::MoonshotCn => env_or(MOONSHOT_CN_BASE_URL_ENV, "https://api.moonshot.cn/v1"),
            Self::MoonshotAi => env_or(MOONSHOT_AI_BASE_URL_ENV, "https://api.moonshot.ai/v1"),
        }
    }

    /// True for the OAuth-bearer subscription channel.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::KimiCode)
    }

    /// Model-id prefixes admitted from this platform's `/models` listing.
    /// `None` = no filtering (subscription listing is served pre-filtered).
    pub fn allowed_model_prefixes(self) -> Option<&'static [&'static str]> {
        match self {
            Self::KimiCode => None,
            Self::MoonshotCn | Self::MoonshotAi => Some(&["kimi-k"]),
        }
    }

    /// Env var names holding this platform's API key, in precedence order
    /// (first set, non-blank value wins). Empty for the OAuth channel.
    ///
    /// SECURITY: the *values* behind these names must never be logged.
    pub fn api_key_env_names(self) -> &'static [&'static str] {
        match self {
            Self::KimiCode => &[],
            Self::MoonshotCn => &[MOONSHOT_CN_API_KEY_ENV, MOONSHOT_API_KEY_ENV],
            Self::MoonshotAi => &[MOONSHOT_AI_API_KEY_ENV, MOONSHOT_API_KEY_ENV],
        }
    }

    /// Managed catalog key for a model served by this platform:
    /// `{platform_id}/{model_id}` (kimi-cli `managed_model_key`).
    pub fn managed_model_key(self, model_id: &str) -> String {
        format!("{}/{model_id}", self.as_str())
    }
}

/// Split a managed catalog key `{platform_id}/{model_id}` back into its
/// platform and bare model id. `None` when the key carries no known platform
/// prefix (e.g. a user-defined `[model.*]` entry).
pub fn parse_managed_model_key(key: &str) -> Option<(PlatformId, &str)> {
    let (platform, model_id) = key.split_once('/')?;
    let platform = PlatformId::parse(platform)?;
    if model_id.is_empty() {
        return None;
    }
    Some((platform, model_id))
}

// ── Wire contract + capability derivation (PRD F4) ──────────────────────────

/// Model capabilities derived from the `/models` listing
/// (port of kimi-cli `ModelCapability` + `ModelInfo.capabilities`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    /// Supports reasoning ("thinking" mode toggleable on/off).
    Thinking,
    /// Thinking cannot be disabled (id contains "thinking").
    AlwaysThinking,
    ImageIn,
    VideoIn,
}

impl ModelCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::AlwaysThinking => "always_thinking",
            Self::ImageIn => "image_in",
            Self::VideoIn => "video_in",
        }
    }
}

impl std::fmt::Display for ModelCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the `GET {base}/models` response `data` array (PRD F4).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireModel {
    pub id: String,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub supports_image_in: bool,
    #[serde(default)]
    pub supports_video_in: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    /// `"only"` marks always-thinking models (thinking cannot be disabled).
    /// Verified against the live `api.kimi.com/coding/v1/models` response.
    #[serde(default)]
    pub supports_thinking_type: Option<String>,
    /// Selectable thinking-effort levels, present only on models that offer
    /// them (e.g. K3). Verified against the live `/models` response.
    #[serde(default)]
    pub think_efforts: Option<WireThinkEfforts>,
}

/// The `think_efforts` object of a `/models` entry. Live wire shape
/// (api.kimi.com, 2026-07):
/// `{"support": true, "valid_efforts": ["low", "high", "max"],
///   "default_effort": "max"}`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct WireThinkEfforts {
    #[serde(default)]
    pub support: bool,
    #[serde(default)]
    pub valid_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
}

/// `GET {base}/models` response envelope.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireModelsResponse {
    pub data: Vec<WireModel>,
}

impl WireModel {
    /// Capability derivation ported verbatim from kimi-cli
    /// `auth/platforms.py::ModelInfo.capabilities`:
    /// - `supports_reasoning` → thinking
    /// - `"thinking"` in id → thinking + always_thinking
    /// - `supports_image_in` → image_in; `supports_video_in` → video_in
    /// - id starts with `kimi-k2` → thinking + image_in + video_in
    ///
    /// On top of that, the live wire's `supports_thinking_type: "only"`
    /// marks a model whose thinking cannot be disabled → always_thinking.
    ///
    /// Returned sorted + deduplicated ([`ModelCapability`]'s `Ord`).
    pub fn capabilities(&self) -> Vec<ModelCapability> {
        let mut caps = derive_capabilities(
            &self.id,
            self.supports_reasoning,
            self.supports_image_in,
            self.supports_video_in,
        );
        if self.supports_thinking_type.as_deref() == Some("only") {
            for cap in [ModelCapability::Thinking, ModelCapability::AlwaysThinking] {
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
            caps.sort();
        }
        caps
    }
}

/// See [`WireModel::capabilities`]; split out so fallback/bundled entries can
/// run the same derivation from an id alone.
pub fn derive_capabilities(
    id: &str,
    supports_reasoning: bool,
    supports_image_in: bool,
    supports_video_in: bool,
) -> Vec<ModelCapability> {
    let id_lower = id.to_lowercase();
    let mut caps = std::collections::BTreeSet::new();
    if supports_reasoning {
        caps.insert(ModelCapability::Thinking);
    }
    if id_lower.contains("thinking") {
        caps.insert(ModelCapability::Thinking);
        caps.insert(ModelCapability::AlwaysThinking);
    }
    if supports_image_in {
        caps.insert(ModelCapability::ImageIn);
    }
    if supports_video_in {
        caps.insert(ModelCapability::VideoIn);
    }
    if id_lower.starts_with("kimi-k2") {
        caps.insert(ModelCapability::Thinking);
        caps.insert(ModelCapability::ImageIn);
        caps.insert(ModelCapability::VideoIn);
    }
    caps.into_iter().collect()
}

/// Whether thinking should default ON for a model with these capabilities
/// (PRD F4: `thinking` or `always_thinking` present).
pub fn default_thinking_enabled(capabilities: &[ModelCapability]) -> bool {
    capabilities.iter().any(|c| {
        matches!(
            c,
            ModelCapability::Thinking | ModelCapability::AlwaysThinking
        )
    })
}

/// Apply a platform's `allowed_model_prefixes` filter to a `/models` listing
/// (kimi-cli `list_models`). No-op for platforms without a filter.
pub fn filter_allowed_models(platform: PlatformId, models: Vec<WireModel>) -> Vec<WireModel> {
    let Some(prefixes) = platform.allowed_model_prefixes() else {
        return models;
    };
    models
        .into_iter()
        .filter(|m| prefixes.iter().any(|p| m.id.starts_with(p)))
        .collect()
}

// ── Bundled offline fallback catalog ────────────────────────────────────────

/// The raw JSON, embedded at compile time. OFFLINE LAST RESORT: consulted only
/// when the live `/models` sync fails and no disk cache is usable.
///
/// Sources for every id (do not add ids that cannot be sourced):
/// - `kimi-for-coding`: kimi-cli `src/kimi_cli/llm.py` (`model_display_name`,
///   `derive_model_capabilities`) — the Kimi Code subscription coding model.
///   Its capabilities {thinking, image_in, video_in} come from
///   `derive_model_capabilities` in the same file.
/// - `kimi-k2-turbo-preview` / `kimi-k2-thinking-turbo`: kimi-cli
///   `tests/core/test_create_llm.py` (`_make_kimi_plain_model`,
///   `_make_kimi_thinking_model`) — Moonshot open-platform models. Their
///   capabilities follow the `auth/platforms.py` derivation rules
///   ([`derive_capabilities`]).
/// - context_window 262144: the canonical Kimi context size used by kimi-cli's
///   own budget tests (`tests/core/test_create_llm.py`).
///
/// Re-exported through the `kigi_shell::models` facade and consumed by
/// `agent::config`, so it must be `pub`.
pub const DEFAULT_MODELS_JSON: &str = include_str!("../default_models.json");

#[derive(serde::Deserialize)]
struct DefaultModels {
    default: String,
    /// Falls back to `default` if not specified in JSON.
    web_search: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    image_description: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    session_summary: Option<String>,
    models: Vec<DefaultModelEntry>,
}

#[derive(serde::Deserialize)]
struct DefaultModelEntry {
    model: String,
}

static DEFAULTS: LazyLock<DefaultModels> = LazyLock::new(|| {
    let defaults: DefaultModels = serde_json::from_str(DEFAULT_MODELS_JSON)
        .expect("default_models.json: invalid JSON or missing 'default' field");

    // Baked-in JSON — a mismatch here is a developer error, not a runtime condition.
    let model_ids: Vec<&str> = defaults.models.iter().map(|m| m.model.as_str()).collect();
    assert!(
        model_ids.contains(&defaults.default.as_str()),
        "default_models.json: 'default' is '{}' but 'models' array only has {model_ids:?}",
        defaults.default,
    );

    defaults
});

/// Primary model for coding tasks and general fallback.
pub fn default_model() -> &'static str {
    &DEFAULTS.default
}

/// Model for web search tool synthesis. Falls back to default model.
pub fn default_web_search_model() -> &'static str {
    DEFAULTS.web_search.as_deref().unwrap_or(&DEFAULTS.default)
}

/// Model for image describe. Falls back to default model.
pub fn default_image_description_model() -> &'static str {
    DEFAULTS
        .image_description
        .as_deref()
        .unwrap_or(&DEFAULTS.default)
}

/// Model for session title generation. Falls back to default model.
pub fn default_session_summary_model() -> &'static str {
    DEFAULTS
        .session_summary
        .as_deref()
        .unwrap_or(&DEFAULTS.default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror of the live `api.kimi.com/coding/v1/models` K3 entry
    /// (fetched 2026-07-17): `supports_thinking_type: "only"` plus a
    /// `think_efforts` block with low/high/max and a max default.
    #[test]
    fn wire_model_parses_live_k3_think_efforts() {
        let json = serde_json::json!({
            "id": "k3",
            "created": 1_761_264_000,
            "object": "model",
            "display_name": "K3",
            "type": "model",
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
        });
        let wire: WireModel = serde_json::from_value(json).unwrap();
        let efforts = wire.think_efforts.as_ref().unwrap();
        assert!(efforts.support);
        assert_eq!(efforts.valid_efforts, ["low", "high", "max"]);
        assert_eq!(efforts.default_effort.as_deref(), Some("max"));
        // "only" thinking type forces always_thinking on top of the
        // supports_reasoning-derived thinking capability.
        let caps = wire.capabilities();
        assert!(caps.contains(&ModelCapability::Thinking));
        assert!(caps.contains(&ModelCapability::AlwaysThinking));
    }

    /// The K2.7 entries carry `supports_thinking_type: "only"` but no
    /// `think_efforts` — always-thinking without selectable levels.
    #[test]
    fn wire_model_without_think_efforts_still_always_thinking() {
        let json = serde_json::json!({
            "id": "kimi-for-coding",
            "context_length": 262_144,
            "supports_reasoning": true,
            "supports_image_in": true,
            "supports_video_in": true,
            "supports_thinking_type": "only"
        });
        let wire: WireModel = serde_json::from_value(json).unwrap();
        assert!(wire.think_efforts.is_none());
        let caps = wire.capabilities();
        assert!(caps.contains(&ModelCapability::AlwaysThinking));
        // Sorted + deduplicated invariant holds after the "only" injection.
        let mut sorted = caps.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(caps, sorted);
    }

    #[test]
    fn platform_ids_round_trip() {
        for p in PlatformId::ALL {
            assert_eq!(PlatformId::parse(p.as_str()), Some(p));
        }
        assert_eq!(PlatformId::parse("openai"), None);
    }

    #[test]
    fn platform_base_urls() {
        assert_eq!(
            PlatformId::MoonshotCn.base_url(),
            "https://api.moonshot.cn/v1"
        );
        assert_eq!(
            PlatformId::MoonshotAi.base_url(),
            "https://api.moonshot.ai/v1"
        );
        // Subscription base honors the env override.
        let _g = kigi_env::EnvVarGuard::set(kigi_env::CODE_BASE_URL_ENV, "https://mock.test/v1");
        assert_eq!(PlatformId::KimiCode.base_url(), "https://mock.test/v1");
    }

    #[test]
    fn managed_model_key_format_and_parse() {
        let key = PlatformId::MoonshotCn.managed_model_key("kimi-k2-turbo-preview");
        assert_eq!(key, "moonshot-cn/kimi-k2-turbo-preview");
        assert_eq!(
            parse_managed_model_key(&key),
            Some((PlatformId::MoonshotCn, "kimi-k2-turbo-preview"))
        );
        assert_eq!(
            parse_managed_model_key("kimi-code/kimi-for-coding"),
            Some((PlatformId::KimiCode, "kimi-for-coding"))
        );
        // No prefix / unknown platform / empty model id → None.
        assert_eq!(parse_managed_model_key("kimi-for-coding"), None);
        assert_eq!(parse_managed_model_key("openai/gpt"), None);
        assert_eq!(parse_managed_model_key("moonshot-cn/"), None);
    }

    /// Capability derivation table ported from kimi-cli platforms.py.
    #[test]
    fn capability_derivation_table() {
        use ModelCapability::*;
        let cases: &[(&str, bool, bool, bool, &[ModelCapability])] = &[
            // supports_reasoning only → thinking
            ("some-model", true, false, false, &[Thinking]),
            // no flags, no name rules → empty
            ("some-model", false, false, false, &[]),
            // "thinking" in id → thinking + always_thinking
            (
                "kimi-latest-thinking",
                false,
                false,
                false,
                &[Thinking, AlwaysThinking],
            ),
            // image/video flags map directly
            ("some-model", false, true, true, &[ImageIn, VideoIn]),
            // kimi-k2 prefix → thinking + image_in + video_in
            (
                "kimi-k2-turbo-preview",
                false,
                false,
                false,
                &[Thinking, ImageIn, VideoIn],
            ),
            // kimi-k2 prefix + "thinking" in id → all four
            (
                "kimi-k2-thinking-turbo",
                false,
                false,
                false,
                &[Thinking, AlwaysThinking, ImageIn, VideoIn],
            ),
            // Case-insensitive id rules (mirrors `.lower()` in platforms.py)
            (
                "Kimi-K2-Thinking",
                false,
                false,
                false,
                &[Thinking, AlwaysThinking, ImageIn, VideoIn],
            ),
        ];
        for (id, reasoning, image, video, want) in cases {
            let got = derive_capabilities(id, *reasoning, *image, *video);
            assert_eq!(&got, want, "capabilities for {id}");
        }
    }

    #[test]
    fn default_thinking_from_capabilities() {
        use ModelCapability::*;
        assert!(default_thinking_enabled(&[Thinking]));
        assert!(default_thinking_enabled(&[AlwaysThinking]));
        assert!(default_thinking_enabled(&[Thinking, ImageIn]));
        assert!(!default_thinking_enabled(&[ImageIn, VideoIn]));
        assert!(!default_thinking_enabled(&[]));
    }

    #[test]
    fn moonshot_prefix_filter_applies_only_to_open_platforms() {
        let listing = vec![
            WireModel {
                id: "kimi-k2-turbo-preview".into(),
                context_length: 262_144,
                supports_reasoning: false,
                supports_image_in: false,
                supports_video_in: false,
                display_name: None,
                supports_thinking_type: None,
                think_efforts: None,
            },
            WireModel {
                id: "moonshot-v1-8k".into(),
                context_length: 8_192,
                supports_reasoning: false,
                supports_image_in: false,
                supports_video_in: false,
                display_name: None,
                supports_thinking_type: None,
                think_efforts: None,
            },
        ];
        let filtered = filter_allowed_models(PlatformId::MoonshotCn, listing.clone());
        assert_eq!(
            filtered.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["kimi-k2-turbo-preview"],
            "moonshot listing must be filtered to the kimi-k prefix"
        );
        let unfiltered = filter_allowed_models(PlatformId::KimiCode, listing);
        assert_eq!(unfiltered.len(), 2, "subscription listing is not filtered");
    }

    #[test]
    fn wire_response_parses_f4_shape() {
        let raw = r#"{
            "data": [
                {
                    "id": "kimi-for-coding",
                    "context_length": 262144,
                    "supports_reasoning": true,
                    "supports_image_in": true,
                    "supports_video_in": false,
                    "display_name": "k2.6-code-preview"
                },
                { "id": "kimi-k2-turbo-preview" }
            ]
        }"#;
        let resp: WireModelsResponse = serde_json::from_str(raw).expect("F4 shape must parse");
        assert_eq!(resp.data.len(), 2);
        let first = &resp.data[0];
        assert_eq!(first.id, "kimi-for-coding");
        assert_eq!(first.context_length, 262_144);
        assert_eq!(first.display_name.as_deref(), Some("k2.6-code-preview"));
        assert_eq!(
            first.capabilities(),
            vec![ModelCapability::Thinking, ModelCapability::ImageIn]
        );
        // Missing optional fields default off/0.
        let second = &resp.data[1];
        assert_eq!(second.context_length, 0);
        assert!(!second.supports_reasoning);
    }

    #[test]
    fn bundled_fallback_is_kimi_catalog() {
        assert_eq!(default_model(), "kimi-for-coding");
        // Aux models fall back to the default (no dedicated entries).
        assert_eq!(default_web_search_model(), "kimi-for-coding");
        assert_eq!(default_image_description_model(), "kimi-for-coding");
        assert_eq!(default_session_summary_model(), "kimi-for-coding");
        // No grok remnants in the embedded fallback.
        assert!(!DEFAULT_MODELS_JSON.contains("grok"));
    }
}
