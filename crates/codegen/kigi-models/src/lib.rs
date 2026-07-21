//! Kimi model catalog primitives (PRD F2/F4).
//!
//! This crate owns:
//! - the compiled-in platform registry ([`PlatformId`] + its spec rows): the
//!   Kimi Code subscription channel plus the API-key platforms;
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

pub mod enrichment;

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

/// Inference dialect a platform speaks. Leaf-safe mirror of the sampler's
/// `ApiBackend` (kigi-models must stay dependency-light); the shell maps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformWireApi {
    ChatCompletions,
    Responses,
    Messages,
}

/// Shape + headers of a platform's model-listing endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingDialect {
    /// `GET {base}/models`, `Authorization: Bearer`, `{data:[{id,...}]}`
    /// (the F4 wire contract; kimi extends it with think_efforts etc.).
    OpenAi,
    /// `GET {base}/models?limit=1000`, `x-api-key` + `anthropic-version`
    /// headers, Anthropic's response shape (parsed by
    /// [`parse_anthropic_listing`]).
    Anthropic,
}

/// ChatCompletions body-adaptation dialect (leaf-safe mirror of the
/// sampler's `ChatCompat`; the shell maps it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformChatCompat {
    Kimi,
    DeepSeek,
    Passthrough,
    /// Strict OpenAI-compatible validator (Mistral, Cerebras) — strips
    /// `stream_options` and private fields.
    StrictOpenAi,
}

/// How a platform's API key rides requests (listing, validation, inference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKeyHeader {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version` (Anthropic wire).
    XApiKey,
}

/// Where a platform's base URL is resolved from.
enum BaseUrlSource {
    /// The Kimi Code subscription base, owned by `kigi_env`
    /// (honors `KIGI_CODE_BASE_URL`).
    KigiEnvCoding,
    /// Fixed compiled default with a dev/test env-var override.
    EnvOr {
        env: &'static str,
        default: &'static str,
    },
}

/// One row of the platform registry. All per-platform data lives in these
/// rows; [`PlatformId`] methods only read fields. Adding a platform touches
/// exactly four sites in this file: the enum variant, the `ALL` entry, the
/// `spec()` arm, and the row (plus quirk code where a provider deviates).
/// The `spec()` arm is compiler-enforced; the `ALL` entry is enforced by the
/// `all_covers_every_variant` test — a variant missing from `ALL` would
/// otherwise be silently unparseable and excluded from model sync.
struct PlatformSpec {
    /// Wire id (auth method id, managed-model-key prefix, config key, and —
    /// for API-key platforms — the auth.json scope the key is stored under).
    id: &'static str,
    display_name: &'static str,
    base_url: BaseUrlSource,
    /// True for OAuth-bearer subscription channels.
    uses_oauth: bool,
    /// Model-id prefixes admitted from this platform's `/models` listing.
    /// `None` = no filtering (listing served pre-filtered).
    allowed_model_prefixes: Option<&'static [&'static str]>,
    /// Env var names holding this platform's API key, in precedence order
    /// (first set, non-blank value wins). Empty for OAuth channels.
    ///
    /// SECURITY: the *values* behind these names must never be logged.
    api_key_envs: &'static [&'static str],
    /// Short vendor word for login copy ("Paste your {vendor} API key").
    vendor: &'static str,
    /// Where the user gets an API key (login copy + key-validation errors).
    /// `None` for OAuth channels.
    console_host: Option<&'static str>,
    /// Interactive login-picker label. `None` = fall back to `display_name`.
    login_label: Option<&'static str>,
    /// This platform's provider id on models.dev, for metadata enrichment.
    /// `None` = not covered there (enrichment silently skips).
    models_dev_id: Option<&'static str>,
    /// True when the platform's `/models` listing itself serves context
    /// window / thinking metadata — enrichment (and its network refresh) is
    /// skipped entirely for such platforms.
    wire_serves_metadata: bool,
    /// Inference dialect (mapped to the sampler backend by the shell).
    wire_api: PlatformWireApi,
    /// Model-listing endpoint shape + headers.
    listing: ListingDialect,
    /// ChatCompletions body-adaptation dialect (ignored for other backends).
    chat_compat: PlatformChatCompat,
    /// Key header style for listing/validation/inference.
    key_header: PlatformKeyHeader,
    /// Restrict the live listing to models the enrichment catalog knows —
    /// for providers whose `/models` is polluted with non-chat entries
    /// (tts/embeddings/image). Availability still requires the LIVE listing;
    /// this only drops listing noise, never adds models.
    restrict_to_enriched: bool,
    /// Path (relative to base) to hit for API-key VALIDATION at login, when
    /// the listing endpoint can't validate. OpenRouter's `/models` is public
    /// (200 for any key), so a bad key would false-accept at login; its
    /// `/key` endpoint 401s properly. `None` = validate against `/models`.
    key_validation_path: Option<&'static str>,
    /// A prefix to strip from each live-listing model id before filtering,
    /// enrichment lookup, and managed-key formation. Google's OpenAI-compat
    /// `/models` returns `models/`-prefixed ids while its chat endpoint (and
    /// the models.dev snapshot) use the bare id — stripping canonicalizes to
    /// the bare form. `None` = no stripping (the id is used verbatim). The
    /// strip is a no-op when the prefix is absent, so it is safe even if a
    /// listing returns some ids already bare.
    strip_listing_id_prefix: Option<&'static str>,
}

const KIMI_CODE_SPEC: PlatformSpec = PlatformSpec {
    id: "kimi-code",
    display_name: "Kimi Code",
    base_url: BaseUrlSource::KigiEnvCoding,
    uses_oauth: true,
    allowed_model_prefixes: None,
    api_key_envs: &[],
    vendor: "Kimi",
    console_host: None,
    login_label: None,
    models_dev_id: Some("kimi-for-coding"),
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Kimi,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

const MOONSHOT_CN_SPEC: PlatformSpec = PlatformSpec {
    id: "moonshot-cn",
    display_name: "Moonshot AI Open Platform (moonshot.cn)",
    base_url: BaseUrlSource::EnvOr {
        env: MOONSHOT_CN_BASE_URL_ENV,
        default: "https://api.moonshot.cn/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: Some(&["kimi-k"]),
    api_key_envs: &[MOONSHOT_CN_API_KEY_ENV, MOONSHOT_API_KEY_ENV],
    vendor: "Moonshot",
    console_host: Some("platform.moonshot.cn"),
    login_label: Some("Moonshot Open Platform (API key \u{b7} moonshot.cn)"),
    models_dev_id: Some("moonshotai-cn"),
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Kimi,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

const MOONSHOT_AI_SPEC: PlatformSpec = PlatformSpec {
    id: "moonshot-ai",
    display_name: "Moonshot AI Open Platform (moonshot.ai)",
    base_url: BaseUrlSource::EnvOr {
        env: MOONSHOT_AI_BASE_URL_ENV,
        default: "https://api.moonshot.ai/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: Some(&["kimi-k"]),
    api_key_envs: &[MOONSHOT_AI_API_KEY_ENV, MOONSHOT_API_KEY_ENV],
    vendor: "Moonshot",
    console_host: Some("platform.moonshot.ai"),
    login_label: Some("Moonshot Open Platform (API key \u{b7} moonshot.ai)"),
    models_dev_id: Some("moonshotai"),
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Kimi,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for OpenAI (dev/test escape hatch).
pub const OPENAI_BASE_URL_ENV: &str = "KIGI_OPENAI_BASE_URL";

const OPENAI_SPEC: PlatformSpec = PlatformSpec {
    id: "openai",
    display_name: "OpenAI",
    base_url: BaseUrlSource::EnvOr {
        env: OPENAI_BASE_URL_ENV,
        default: "https://api.openai.com/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["OPENAI_API_KEY"],
    vendor: "OpenAI",
    console_host: Some("platform.openai.com"),
    login_label: Some("OpenAI (API key)"),
    models_dev_id: Some("openai"),
    // GET /v1/models returns bare ids only (no context/thinking metadata)
    // and is polluted with tts/embeddings/image entries.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::Responses,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: true,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for Anthropic (dev/test escape hatch).
pub const ANTHROPIC_BASE_URL_ENV: &str = "KIGI_ANTHROPIC_BASE_URL";

const ANTHROPIC_SPEC: PlatformSpec = PlatformSpec {
    id: "anthropic",
    display_name: "Anthropic",
    base_url: BaseUrlSource::EnvOr {
        env: ANTHROPIC_BASE_URL_ENV,
        default: "https://api.anthropic.com/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["ANTHROPIC_API_KEY"],
    vendor: "Anthropic",
    console_host: Some("console.anthropic.com"),
    login_label: Some("Anthropic (API key)"),
    models_dev_id: Some("anthropic"),
    // The 2026 /v1/models serves capabilities + max_input_tokens, but the
    // adapter maps only what's present — enrichment fills gaps (wire wins).
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::Messages,
    listing: ListingDialect::Anthropic,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::XApiKey,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for DeepSeek (dev/test escape hatch).
pub const DEEPSEEK_BASE_URL_ENV: &str = "KIGI_DEEPSEEK_BASE_URL";

const DEEPSEEK_SPEC: PlatformSpec = PlatformSpec {
    id: "deepseek",
    display_name: "DeepSeek",
    base_url: BaseUrlSource::EnvOr {
        env: DEEPSEEK_BASE_URL_ENV,
        // No /v1: chat rides {base}/chat/completions, listing {base}/models
        // (official docs).
        default: "https://api.deepseek.com",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["DEEPSEEK_API_KEY"],
    vendor: "DeepSeek",
    console_host: Some("platform.deepseek.com"),
    login_label: Some("DeepSeek (API key)"),
    models_dev_id: Some("deepseek"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::DeepSeek,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for Groq (dev/test escape hatch).
pub const GROQ_BASE_URL_ENV: &str = "KIGI_GROQ_BASE_URL";

const GROQ_SPEC: PlatformSpec = PlatformSpec {
    id: "groq",
    display_name: "Groq",
    base_url: BaseUrlSource::EnvOr {
        env: GROQ_BASE_URL_ENV,
        default: "https://api.groq.com/openai/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["GROQ_API_KEY"],
    vendor: "Groq",
    console_host: Some("console.groq.com"),
    login_label: Some("Groq (API key)"),
    models_dev_id: Some("groq"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // The listing carries whisper/tts entries; keep tool-calling chat
    // models only.
    restrict_to_enriched: true,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for Mistral (dev/test escape hatch).
pub const MISTRAL_BASE_URL_ENV: &str = "KIGI_MISTRAL_BASE_URL";

const MISTRAL_SPEC: PlatformSpec = PlatformSpec {
    id: "mistral",
    display_name: "Mistral",
    base_url: BaseUrlSource::EnvOr {
        env: MISTRAL_BASE_URL_ENV,
        default: "https://api.mistral.ai/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["MISTRAL_API_KEY"],
    vendor: "Mistral",
    console_host: Some("console.mistral.ai"),
    login_label: Some("Mistral (API key)"),
    models_dev_id: Some("mistral"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    // Mistral's strict validator 422s on `stream_options`, and its reasoning
    // models return array content — the StrictOpenAi dialect strips
    // stream_options; the response deserializer handles arrays universally.
    chat_compat: PlatformChatCompat::StrictOpenAi,
    key_header: PlatformKeyHeader::Bearer,
    // The listing carries embed/moderation/OCR entries; keep tool-calling
    // chat models only.
    restrict_to_enriched: true,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for Fireworks (dev/test escape hatch).
pub const FIREWORKS_BASE_URL_ENV: &str = "KIGI_FIREWORKS_BASE_URL";

const FIREWORKS_SPEC: PlatformSpec = PlatformSpec {
    id: "fireworks",
    display_name: "Fireworks AI",
    base_url: BaseUrlSource::EnvOr {
        env: FIREWORKS_BASE_URL_ENV,
        // Inference plane (note the /inference/v1 path, not /v1).
        default: "https://api.fireworks.ai/inference/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["FIREWORKS_API_KEY"],
    vendor: "Fireworks",
    console_host: Some("fireworks.ai"),
    login_label: Some("Fireworks AI (API key)"),
    models_dev_id: Some("fireworks-ai"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // The inference /models listing can include embedding/non-chat models;
    // keep tool-calling enrichment-known models only.
    restrict_to_enriched: true,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

/// Base-URL override for Google Gemini (dev/test escape hatch).
pub const GOOGLE_BASE_URL_ENV: &str = "KIGI_GOOGLE_BASE_URL";

const GOOGLE_SPEC: PlatformSpec = PlatformSpec {
    id: "google",
    display_name: "Google Gemini",
    base_url: BaseUrlSource::EnvOr {
        env: GOOGLE_BASE_URL_ENV,
        // Gemini's OpenAI-compatibility shim (bare model ids, Bearer key).
        default: "https://generativelanguage.googleapis.com/v1beta/openai",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["GEMINI_API_KEY"],
    vendor: "Google",
    console_host: Some("aistudio.google.com"),
    login_label: Some("Google Gemini (API key)"),
    models_dev_id: Some("google"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // The compat listing carries embedding/tts/image models; keep
    // tool-calling enrichment-known chat models only.
    restrict_to_enriched: true,
    key_validation_path: None,
    strip_listing_id_prefix: Some("models/"),
};

/// Base-URL override for OpenRouter (dev/test escape hatch).
pub const OPENROUTER_BASE_URL_ENV: &str = "KIGI_OPENROUTER_BASE_URL";

const OPENROUTER_SPEC: PlatformSpec = PlatformSpec {
    id: "openrouter",
    display_name: "OpenRouter",
    base_url: BaseUrlSource::EnvOr {
        env: OPENROUTER_BASE_URL_ENV,
        // Note /api/v1, not /v1.
        default: "https://openrouter.ai/api/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["OPENROUTER_API_KEY"],
    vendor: "OpenRouter",
    console_host: Some("openrouter.ai"),
    login_label: Some("OpenRouter (API key)"),
    // OpenRouter's public /models serves context_length for every model, so
    // enrichment is neither needed nor fetched (verified live: 340/340 carry
    // a top-level context_length). Not a models.dev provider here.
    models_dev_id: None,
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: Some("/key"),
    strip_listing_id_prefix: None,
};

/// Base-URL override for Together AI (dev/test escape hatch).
pub const TOGETHER_BASE_URL_ENV: &str = "KIGI_TOGETHER_BASE_URL";

const TOGETHER_SPEC: PlatformSpec = PlatformSpec {
    id: "together",
    display_name: "Together AI",
    base_url: BaseUrlSource::EnvOr {
        env: TOGETHER_BASE_URL_ENV,
        default: "https://api.together.xyz/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["TOGETHER_API_KEY"],
    vendor: "Together",
    console_host: Some("api.together.xyz"),
    login_label: Some("Together AI (API key)"),
    models_dev_id: Some("togetherai"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    // Together's /v1/models is a BARE JSON array (parse_openai_listing is
    // tolerant), and it mixes chat/embedding/rerank/image types; keep
    // tool-calling enrichment-known chat models only.
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

/// Base-URL override for Cerebras (dev/test escape hatch).
pub const CEREBRAS_BASE_URL_ENV: &str = "KIGI_CEREBRAS_BASE_URL";

const CEREBRAS_SPEC: PlatformSpec = PlatformSpec {
    id: "cerebras",
    display_name: "Cerebras",
    base_url: BaseUrlSource::EnvOr {
        env: CEREBRAS_BASE_URL_ENV,
        default: "https://api.cerebras.ai/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["CEREBRAS_API_KEY"],
    vendor: "Cerebras",
    console_host: Some("cloud.cerebras.ai"),
    login_label: Some("Cerebras (API key)"),
    models_dev_id: Some("cerebras"),
    // /models is minimal (id only, no context) → enrichment supplies context
    // + effort menus. The catalog is all chat LLMs (no embedding/tts
    // pollution), so keep every live model and enrich the known ones.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    // Cerebras uses strict additionalProperties:false validation (400s on
    // out-of-schema fields like store/thinking); strip stream_options.
    chat_compat: PlatformChatCompat::StrictOpenAi,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: false,
};

/// Base-URL override for NVIDIA NIM (dev/test escape hatch).
pub const NVIDIA_BASE_URL_ENV: &str = "KIGI_NVIDIA_BASE_URL";

const NVIDIA_SPEC: PlatformSpec = PlatformSpec {
    id: "nvidia",
    display_name: "NVIDIA NIM",
    base_url: BaseUrlSource::EnvOr {
        env: NVIDIA_BASE_URL_ENV,
        default: "https://integrate.api.nvidia.com/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["NVIDIA_API_KEY"],
    vendor: "NVIDIA",
    console_host: Some("build.nvidia.com"),
    login_label: Some("NVIDIA NIM (API key)"),
    models_dev_id: Some("nvidia"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    // NIM exposes raw vLLM behavior; stream_options support varies per model
    // and some 4xx on it, so strip it (StrictOpenAi) to keep streaming
    // working across the fleet.
    chat_compat: PlatformChatCompat::StrictOpenAi,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    // The listing mixes chat/embedding/rerank/vision/image models; keep
    // tool-calling enrichment-known chat models only.
    restrict_to_enriched: true,
};

/// Base-URL override for Vercel AI Gateway (dev/test escape hatch).
pub const VERCEL_BASE_URL_ENV: &str = "KIGI_VERCEL_BASE_URL";

const VERCEL_SPEC: PlatformSpec = PlatformSpec {
    id: "vercel-ai-gateway",
    display_name: "Vercel AI Gateway",
    base_url: BaseUrlSource::EnvOr {
        env: VERCEL_BASE_URL_ENV,
        default: "https://ai-gateway.vercel.sh/v1",
    },
    uses_oauth: false,
    allowed_model_prefixes: None,
    api_key_envs: &["AI_GATEWAY_API_KEY"],
    vendor: "Vercel",
    console_host: Some("vercel.com"),
    login_label: Some("Vercel AI Gateway (API key)"),
    models_dev_id: Some("vercel"),
    // Vercel's /models serves rich metadata but under `context_window` (not
    // the WireModel `context_length`), so take context from models.dev
    // enrichment instead; restrict to tool-calling chat models (the gateway
    // lists embedding/image/rerank types too). Ids are creator/model,
    // byte-matching the models.dev "vercel" keys.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // /models is PUBLIC (200 for any key), so validate against /credits,
    // which 401s for a bad key.
    key_validation_path: Some("/credits"),
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

/// The platform registry. Platforms are compiled-in spec rows; there is no
/// dynamic provider registration (PRD F2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformId {
    /// Kimi Code subscription (OAuth bearer from the F1 device flow).
    KimiCode,
    /// Moonshot AI open platform, api.moonshot.cn (API key).
    MoonshotCn,
    /// Moonshot AI open platform, api.moonshot.ai (API key).
    MoonshotAi,
    /// OpenAI platform API (API key, Responses dialect).
    OpenAi,
    /// Anthropic platform API (API key, Messages dialect).
    Anthropic,
    /// DeepSeek platform API (API key, ChatCompletions dialect).
    DeepSeek,
    /// Groq platform API (API key, OpenAI-compatible ChatCompletions).
    Groq,
    /// Mistral platform API (API key, OpenAI-compatible ChatCompletions).
    Mistral,
    /// Fireworks AI platform API (API key, OpenAI-compatible ChatCompletions).
    Fireworks,
    /// Google Gemini platform API (API key, OpenAI-compatibility shim).
    Google,
    /// OpenRouter meta-provider (API key, wire-served metadata).
    OpenRouter,
    /// Together AI platform API (API key, bare-array listing).
    Together,
    /// Cerebras platform API (API key, OpenAI-compatible ChatCompletions).
    Cerebras,
    /// NVIDIA NIM platform API (API key, OpenAI-compatible ChatCompletions).
    Nvidia,
    /// Vercel AI Gateway (API key, wire-listed with models.dev enrichment).
    Vercel,
}

impl PlatformId {
    /// All platforms, in catalog precedence order: the subscription channel
    /// first so "default model = first list item" favors it when present.
    pub const ALL: [PlatformId; 15] = [
        Self::KimiCode,
        Self::MoonshotCn,
        Self::MoonshotAi,
        Self::OpenAi,
        Self::Anthropic,
        Self::DeepSeek,
        Self::Groq,
        Self::Mistral,
        Self::Fireworks,
        Self::Google,
        Self::OpenRouter,
        Self::Together,
        Self::Cerebras,
        Self::Nvidia,
        Self::Vercel,
    ];

    /// The registry row backing this platform (single source of per-platform
    /// data; every accessor below reads it).
    const fn spec(self) -> &'static PlatformSpec {
        match self {
            Self::KimiCode => &KIMI_CODE_SPEC,
            Self::MoonshotCn => &MOONSHOT_CN_SPEC,
            Self::MoonshotAi => &MOONSHOT_AI_SPEC,
            Self::OpenAi => &OPENAI_SPEC,
            Self::Anthropic => &ANTHROPIC_SPEC,
            Self::DeepSeek => &DEEPSEEK_SPEC,
            Self::Groq => &GROQ_SPEC,
            Self::Mistral => &MISTRAL_SPEC,
            Self::Fireworks => &FIREWORKS_SPEC,
            Self::Google => &GOOGLE_SPEC,
            Self::OpenRouter => &OPENROUTER_SPEC,
            Self::Together => &TOGETHER_SPEC,
            Self::Cerebras => &CEREBRAS_SPEC,
            Self::Nvidia => &NVIDIA_SPEC,
            Self::Vercel => &VERCEL_SPEC,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.spec().id
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.spec().id == s)
    }

    pub fn display_name(self) -> &'static str {
        self.spec().display_name
    }

    /// Inference/model-listing base URL. The subscription base honors the
    /// `KIGI_CODE_BASE_URL` override via [`kigi_env::coding_api_base_url`];
    /// API-key platform bases are fixed in production, with a per-platform
    /// env var (e.g. `KIGI_MOONSHOT_{CN,AI}_BASE_URL`) as dev/test override.
    pub fn base_url(self) -> String {
        match self.spec().base_url {
            BaseUrlSource::KigiEnvCoding => kigi_env::coding_api_base_url(),
            BaseUrlSource::EnvOr { env, default } => env_or(env, default),
        }
    }

    /// True for OAuth-bearer subscription channels.
    pub fn uses_oauth(self) -> bool {
        self.spec().uses_oauth
    }

    /// Model-id prefixes admitted from this platform's `/models` listing.
    /// `None` = no filtering (subscription listing is served pre-filtered).
    pub fn allowed_model_prefixes(self) -> Option<&'static [&'static str]> {
        self.spec().allowed_model_prefixes
    }

    /// Env var names holding this platform's API key, in precedence order
    /// (first set, non-blank value wins). Empty for the OAuth channel.
    ///
    /// SECURITY: the *values* behind these names must never be logged.
    pub fn api_key_env_names(self) -> &'static [&'static str] {
        self.spec().api_key_envs
    }

    /// Managed catalog key for a model served by this platform:
    /// `{platform_id}/{model_id}` (kimi-cli `managed_model_key`).
    pub fn managed_model_key(self, model_id: &str) -> String {
        format!("{}/{model_id}", self.as_str())
    }

    /// Short vendor word for login copy ("Paste your {vendor} API key").
    pub fn vendor(self) -> &'static str {
        self.spec().vendor
    }

    /// Console host where the user obtains an API key, for login copy and
    /// key-validation errors. `None` for OAuth channels.
    pub fn console_host(self) -> Option<&'static str> {
        self.spec().console_host
    }

    /// Label for the interactive login picker (falls back to the display
    /// name when the row doesn't override it).
    pub fn login_label(self) -> &'static str {
        self.spec().login_label.unwrap_or(self.spec().display_name)
    }

    /// This platform's provider id on models.dev (metadata enrichment).
    pub fn models_dev_id(self) -> Option<&'static str> {
        self.spec().models_dev_id
    }

    /// True when the live `/models` wire serves metadata itself — enrichment
    /// and its network refresh are skipped for such platforms.
    pub fn wire_serves_metadata(self) -> bool {
        self.spec().wire_serves_metadata
    }

    /// Inference dialect this platform speaks (shell maps to `ApiBackend`).
    pub fn wire_api(self) -> PlatformWireApi {
        self.spec().wire_api
    }

    /// Restrict the live listing to enrichment-known models (drops non-chat
    /// listing noise on polluted providers). Never adds models.
    pub fn restrict_to_enriched(self) -> bool {
        self.spec().restrict_to_enriched
    }

    /// Prefix to strip from live-listing model ids before filter/enrich/key
    /// (e.g. Google's `models/`). `None` = use the id verbatim.
    pub fn strip_listing_id_prefix(self) -> Option<&'static str> {
        self.spec().strip_listing_id_prefix
    }

    /// Path (relative to base) for API-key validation at login. Defaults to
    /// `/models`; a platform whose listing is public (OpenRouter) overrides
    /// it with an auth-requiring endpoint so a bad key can't false-accept.
    pub fn key_validation_path(self) -> &'static str {
        self.spec().key_validation_path.unwrap_or("/models")
    }

    /// Model-listing endpoint shape + headers.
    pub fn listing(self) -> ListingDialect {
        self.spec().listing
    }

    /// Key header style for listing/validation/inference requests.
    pub fn key_header(self) -> PlatformKeyHeader {
        self.spec().key_header
    }

    /// ChatCompletions body-adaptation dialect (shell maps to the sampler's
    /// `ChatCompat`; meaningless for other backends).
    pub fn chat_compat(self) -> PlatformChatCompat {
        self.spec().chat_compat
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
    /// Max output tokens (`max_tokens` on the Anthropic listing; absent on
    /// the Kimi/OpenAI-shape wires). 0 = unserved → enrichment may fill.
    #[serde(default)]
    pub max_output_tokens: u64,
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

/// Parse an OpenAI-shape `/models` listing, tolerant of BOTH the standard
/// envelope `{object:"list", data:[...]}` and a bare top-level array `[...]`
/// (Together AI serves the bare-array form).
pub fn parse_openai_listing(json: &str) -> Result<Vec<WireModel>, serde_json::Error> {
    // Sniff the top-level shape so a malformed body yields the diagnostic for
    // the shape it actually is (a broken bare-array element reports the
    // element error, not a misleading "expected the envelope object").
    if json.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<WireModel>>(json)
    } else {
        Ok(serde_json::from_str::<WireModelsResponse>(json)?.data)
    }
}

// ── Anthropic listing adapter (ListingDialect::Anthropic) ───────────────────

#[derive(serde::Deserialize)]
struct AnthropicListing {
    /// No default: a 200 body without `data` is a contract violation and
    /// must error like the OpenAI-shape branch, not yield an empty catalog.
    data: Vec<AnthropicModel>,
    #[serde(default)]
    has_more: bool,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AnthropicModel {
    id: String,
    display_name: Option<String>,
    max_input_tokens: u64,
    /// The model's output cap (Anthropic REQUIRES `max_tokens` on
    /// /v1/messages and 400s when it exceeds this).
    max_tokens: u64,
    capabilities: AnthropicCapabilities,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AnthropicCapabilities {
    effort: AnthropicEffort,
    thinking: AnthropicSupported,
    image_input: AnthropicSupported,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AnthropicEffort {
    supported: bool,
    low: AnthropicSupported,
    medium: AnthropicSupported,
    high: AnthropicSupported,
    xhigh: AnthropicSupported,
    max: AnthropicSupported,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AnthropicSupported {
    supported: bool,
}

/// Parse the 2026 Anthropic `GET /v1/models` response into the F4
/// [`WireModel`] shape: `max_input_tokens` → context (0 stays 0 so
/// enrichment can fill it), `capabilities.thinking/image_input` → flags,
/// `capabilities.effort.{level}.supported` → `think_efforts` in canonical
/// order. `has_more: true` (impossible under `?limit=1000` for Anthropic's
/// catalog size) warns rather than silently truncating.
pub fn parse_anthropic_listing(json: &str) -> Result<Vec<WireModel>, serde_json::Error> {
    let listing: AnthropicListing = serde_json::from_str(json)?;
    if listing.has_more {
        tracing::warn!(
            fetched = listing.data.len(),
            "anthropic /models reports more pages beyond limit=1000; \
             listing may be incomplete"
        );
    }
    Ok(listing
        .data
        .into_iter()
        .filter(|m| {
            let keep = !m.id.is_empty();
            if !keep {
                tracing::warn!("anthropic listing entry without id; dropping");
            }
            keep
        })
        .map(|m| {
            let e = &m.capabilities.effort;
            let valid_efforts: Vec<String> = [
                ("low", e.low.supported),
                ("medium", e.medium.supported),
                ("high", e.high.supported),
                ("xhigh", e.xhigh.supported),
                ("max", e.max.supported),
            ]
            .into_iter()
            .filter(|(_, supported)| *supported)
            .map(|(level, _)| level.to_string())
            .collect();
            // The wire has no default marker; the provider's implicit
            // default applies until the user picks a level. An explicit
            // `supported: false` becomes a DECLINE sentinel (support=false)
            // — distinguishable from "wire silent", so enrichment can never
            // inject a menu the server rejects (pre-4.6 models 400 on
            // adaptive thinking).
            let think_efforts = if e.supported && !valid_efforts.is_empty() {
                Some(WireThinkEfforts {
                    support: true,
                    valid_efforts,
                    default_effort: None,
                })
            } else {
                Some(WireThinkEfforts {
                    support: false,
                    valid_efforts: Vec::new(),
                    default_effort: None,
                })
            };
            WireModel {
                id: m.id,
                context_length: m.max_input_tokens,
                supports_reasoning: m.capabilities.thinking.supported,
                supports_image_in: m.capabilities.image_input.supported,
                supports_video_in: false,
                display_name: m.display_name,
                max_output_tokens: m.max_tokens,
                supports_thinking_type: None,
                think_efforts,
            }
        })
        .collect())
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

    /// The Anthropic listing adapter maps the documented 2026 response shape
    /// (platform.claude.com/docs/en/api/models-list) onto WireModel: effort
    /// capability levels become think_efforts in canonical order, a zero
    /// max_input_tokens stays zero (enrichment fills it), unknown fields
    /// tolerated, effort.supported=false yields no menu.
    #[test]
    fn anthropic_listing_maps_documented_shape() {
        let json = serde_json::json!({
            "data": [
                {
                    "id": "claude-opus-4-6",
                    "display_name": "Claude Opus 4.6",
                    "created_at": "2026-02-04T00:00:00Z",
                    "type": "model",
                    "max_input_tokens": 1_000_000,
                    "max_tokens": 128_000,
                    "created_at_is_ignored": true,
                    "capabilities": {
                        "batch": { "supported": true },
                        "effort": {
                            "supported": true,
                            "low": { "supported": true },
                            "medium": { "supported": true },
                            "high": { "supported": true },
                            "xhigh": { "supported": true },
                            "max": { "supported": true }
                        },
                        "thinking": {
                            "supported": true,
                            "types": {
                                "adaptive": { "supported": true },
                                "enabled": { "supported": true }
                            }
                        },
                        "image_input": { "supported": true },
                        "structured_outputs": { "supported": true }
                    }
                },
                {
                    "id": "claude-legacy",
                    "max_input_tokens": 0,
                    "capabilities": {
                        "effort": { "supported": false },
                        "thinking": { "supported": false },
                        "image_input": { "supported": false }
                    }
                }
            ],
            "first_id": "claude-opus-4-6",
            "last_id": "claude-legacy",
            "has_more": false
        })
        .to_string();
        let models = parse_anthropic_listing(&json).expect("documented shape parses");
        assert_eq!(models.len(), 2);
        let opus = &models[0];
        assert_eq!(opus.id, "claude-opus-4-6");
        assert_eq!(opus.context_length, 1_000_000);
        assert_eq!(
            opus.max_output_tokens, 128_000,
            "wire max_tokens is the output cap (Anthropic 400s above it)"
        );
        assert!(opus.supports_reasoning && opus.supports_image_in);
        assert_eq!(opus.display_name.as_deref(), Some("Claude Opus 4.6"));
        let efforts = opus.think_efforts.as_ref().expect("effort menu");
        assert_eq!(
            efforts.valid_efforts,
            ["low", "medium", "high", "xhigh", "max"],
            "levels in canonical order from per-level supported flags"
        );
        assert_eq!(efforts.default_effort, None);
        let legacy = &models[1];
        assert_eq!(legacy.context_length, 0, "zero stays zero for enrichment");
        let decline = legacy
            .think_efforts
            .as_ref()
            .expect("explicit wire decline is a sentinel, not absence");
        assert!(
            !decline.support && decline.valid_efforts.is_empty(),
            "effort.supported=false must block enrichment menu injection"
        );
        assert!(!legacy.supports_reasoning);

        // A 200 body without `data` is a contract violation, not an empty
        // catalog; entries without an id are dropped with a warning.
        assert!(parse_anthropic_listing("{}").is_err());
        let ghosts = serde_json::json!({ "data": [ {}, { "id": "real" } ] }).to_string();
        let models = parse_anthropic_listing(&ghosts).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["real"]
        );
    }

    /// The OpenAI listing parser accepts both the standard envelope and a
    /// bare top-level array (Together AI serves the bare form).
    #[test]
    fn openai_listing_parse_accepts_envelope_and_bare_array() {
        let envelope = r#"{"object":"list","data":[
            {"id":"a","context_length":1000},{"id":"b"}]}"#;
        let bare = r#"[{"id":"a","context_length":1000},{"id":"b"}]"#;
        for (label, json) in [("envelope", envelope), ("bare array", bare)] {
            let models =
                parse_openai_listing(json).unwrap_or_else(|e| panic!("{label} must parse: {e}"));
            assert_eq!(
                models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                vec!["a", "b"],
                "{label}"
            );
            assert_eq!(models[0].context_length, 1000);
        }
        // Neither shape → error (not a silent empty list).
        assert!(parse_openai_listing("\"not a list\"").is_err());
        assert!(parse_openai_listing("{").is_err());
    }

    /// Providers whose `/models` listing is public must validate keys
    /// against an auth-requiring endpoint; every other platform validates
    /// against the default `/models`.
    #[test]
    fn public_listing_providers_override_the_validation_path() {
        assert_eq!(PlatformId::OpenRouter.key_validation_path(), "/key");
        assert_eq!(PlatformId::Vercel.key_validation_path(), "/credits");
        let overrides = [PlatformId::OpenRouter, PlatformId::Vercel];
        for p in PlatformId::ALL {
            if !overrides.contains(&p) {
                assert_eq!(
                    p.key_validation_path(),
                    "/models",
                    "{} must validate against /models",
                    p.as_str()
                );
            }
        }
    }

    /// Google's compat listing returns `models/`-prefixed ids; the spec
    /// must declare the strip so they canonicalize to the bare snapshot
    /// form. Every other platform uses ids verbatim.
    #[test]
    fn only_google_strips_a_listing_id_prefix() {
        assert_eq!(
            PlatformId::Google.strip_listing_id_prefix(),
            Some("models/")
        );
        for p in PlatformId::ALL {
            if p != PlatformId::Google {
                assert_eq!(
                    p.strip_listing_id_prefix(),
                    None,
                    "{} must use listing ids verbatim",
                    p.as_str()
                );
            }
        }
    }

    #[test]
    fn platform_ids_round_trip() {
        for p in PlatformId::ALL {
            assert_eq!(PlatformId::parse(p.as_str()), Some(p));
        }
        assert_eq!(PlatformId::parse("not-a-platform"), None);
    }

    /// A variant missing from `ALL` compiles fine (`ALL`'s length is a plain
    /// literal) but is silently unparseable and excluded from model sync.
    /// The exhaustive match below fails compilation when a variant is added,
    /// forcing this test — and with it the `ALL` entry — to be updated.
    #[test]
    fn all_covers_every_variant() {
        fn ordinal(p: PlatformId) -> usize {
            match p {
                PlatformId::KimiCode => 0,
                PlatformId::MoonshotCn => 1,
                PlatformId::MoonshotAi => 2,
                PlatformId::OpenAi => 3,
                PlatformId::Anthropic => 4,
                PlatformId::DeepSeek => 5,
                PlatformId::Groq => 6,
                PlatformId::Mistral => 7,
                PlatformId::Fireworks => 8,
                PlatformId::Google => 9,
                PlatformId::OpenRouter => 10,
                PlatformId::Together => 11,
                PlatformId::Cerebras => 12,
                PlatformId::Nvidia => 13,
                PlatformId::Vercel => 14,
            }
        }
        const VARIANT_COUNT: usize = 15; // update together with `ordinal`
        let mut seen: Vec<usize> = PlatformId::ALL.iter().map(|&p| ordinal(p)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            PlatformId::ALL.len(),
            "duplicate variant in ALL"
        );
        assert_eq!(
            seen.len(),
            VARIANT_COUNT,
            "PlatformId::ALL must contain every variant"
        );
    }

    /// Row-shape invariants the login UI and key resolution rely on:
    /// API-key platforms carry a console host (paste-box copy) and at least
    /// one key env var (missing-key error names it); OAuth channels carry
    /// neither key envs nor a console host requirement.
    #[test]
    fn api_key_rows_carry_console_host_and_env_names() {
        for p in PlatformId::ALL {
            if p.uses_oauth() {
                assert!(
                    p.api_key_env_names().is_empty(),
                    "{}: OAuth platforms take no key envs",
                    p.as_str()
                );
            } else {
                assert!(
                    p.console_host().is_some(),
                    "{}: API-key platforms must name their console host",
                    p.as_str()
                );
                assert!(
                    !p.api_key_env_names().is_empty(),
                    "{}: API-key platforms must name at least one key env",
                    p.as_str()
                );
                assert!(
                    !p.vendor().is_empty(),
                    "{}: API-key platforms must set a vendor word",
                    p.as_str()
                );
            }
        }
    }

    /// `parse` resolves by scanning spec rows, so duplicate ids would
    /// silently shadow a platform. Pin uniqueness as rows are added.
    #[test]
    fn platform_spec_ids_unique() {
        let mut ids: Vec<&str> = PlatformId::ALL.iter().map(|p| p.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate platform spec id");
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
        // FIRST-slash split: provider-native slashed ids (11 of 15 groq
        // models, e.g. openai/gpt-oss-120b) must survive the round trip.
        assert_eq!(
            parse_managed_model_key("groq/openai/gpt-oss-120b"),
            Some((PlatformId::Groq, "openai/gpt-oss-120b"))
        );
        // No prefix / unknown platform / empty model id → None.
        assert_eq!(parse_managed_model_key("kimi-for-coding"), None);
        assert_eq!(parse_managed_model_key("not-a-platform/gpt"), None);
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
                max_output_tokens: 0,
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
                max_output_tokens: 0,
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
        assert_eq!(default_image_description_model(), "kimi-for-coding");
        assert_eq!(default_session_summary_model(), "kimi-for-coding");
        // No kigi remnants in the embedded fallback.
        assert!(!DEFAULT_MODELS_JSON.contains("kigi"));
    }
}
