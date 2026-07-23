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
    /// Strict OpenAI-compatible validator (Cerebras, NVIDIA) — strips
    /// `stream_options` and private fields.
    StrictOpenAi,
    /// Mistral: StrictOpenAi plus its exactly-9-alphanumeric tool-call id
    /// contract (foreign/OpenAI-style ids are deterministically remapped).
    Mistral,
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

/// The interactive login mechanism a `uses_oauth` [`OAuthConfig`] provider
/// drives. `DeviceCode` is the RFC-8628 device flow (xai-grok); `PkceLocalhost`
/// is the authorization-code + PKCE (S256) flow with a loopback callback
/// (Claude Pro/Max). Kimi Code carries neither (its bespoke flow lives in
/// kigi-shell with `oauth: None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlow {
    /// RFC-8628 device-code: POST `device_path`, poll `token_path`.
    DeviceCode,
    /// Authorization-code + PKCE (S256): browser hits `auth_host`+`device_path`
    /// (the authorize endpoint); the code returns to a `127.0.0.1:redirect_port`
    /// loopback listener answering `redirect_path` (claude `/callback`, codex
    /// `/auth/callback`), then is exchanged at `token_host`+`token_path`.
    PkceLocalhost {
        redirect_port: u16,
        redirect_path: &'static str,
    },
    /// GitHub Copilot two-stage flow (github-copilot): an RFC-8628 device flow
    /// on `auth_host` (github.com) mints the DURABLE GitHub token, which is then
    /// exchanged at `copilot_exchange` for the SHORT-LIVED copilot session
    /// token. The github token is persisted as `refresh_token`; the copilot
    /// token as `key`. GitHub's poll returns errors in a `200` body (not `4xx`),
    /// so it drives a Copilot-specific poll, not the generic device wire.
    GithubDeviceCopilot,
}

/// Body encoding a provider's token endpoint expects for the code-exchange and
/// refresh POSTs. xAI's `/oauth2/token` is form-encoded; Claude's
/// `/v1/oauth/token` is JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthTokenBody {
    /// `application/x-www-form-urlencoded` (xai-grok device wire).
    Form,
    /// `application/json` (Claude PKCE wire).
    Json,
    /// GitHub Copilot copilot-token re-mint (github-copilot): "refresh" is NOT
    /// a `refresh_token` grant — it is a `GET copilot_internal/v2/token` bearing
    /// the durable GitHub token (`refresh_token` field) + editor headers, which
    /// re-mints the short-lived copilot token. Dispatched to the Copilot wire.
    GithubCopilotExchange,
}

/// Generic OAuth configuration carried by a `uses_oauth` platform whose login
/// is the GENERIC device-code path (xai-grok) or the PKCE-localhost path
/// (claude-pro-max).
///
/// Kimi Code keeps its bespoke device flow (client id, `/api/oauth/*` paths,
/// X-Msh device headers, `kigi_env::oauth_host()`); its `oauth` field stays
/// `None`. All fields here are non-secret wire constants — the access/refresh
/// tokens they mint are NEVER stored in this struct and never logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthConfig {
    /// OAuth client id sent on every authorize / token call.
    pub client_id: &'static str,
    /// Authorization-server origin (no trailing slash), e.g. `https://auth.x.ai`
    /// (device) or `https://claude.ai` (PKCE authorize host).
    pub auth_host: &'static str,
    /// Start-endpoint path (POST for `DeviceCode` device-authorization; the
    /// browser authorize path for `PkceLocalhost`), relative to `auth_host`.
    pub device_path: &'static str,
    /// Token-endpoint origin (no trailing slash). Equals `auth_host` for the
    /// device wire; for Claude the token host (`https://platform.claude.com`)
    /// differs from the authorize host (`https://claude.ai`).
    pub token_host: &'static str,
    /// Token path (POST) — used for BOTH the initial grant and refresh,
    /// relative to `token_host`.
    pub token_path: &'static str,
    /// OAuth scope string requested at authorization.
    pub scope: &'static str,
    /// auth.json map key + keyring entry name for this provider's persisted
    /// session (e.g. `oauth/xai`).
    pub scope_key: &'static str,
    /// A non-standard extra form field sent ONLY on the device-authorization
    /// request (e.g. `("referrer", "kigi")`). `None` = no extra field.
    pub extra_device_field: Option<(&'static str, &'static str)>,
    /// Extra query params appended ONLY to the `PkceLocalhost` browser authorize
    /// URL (openai-codex's `id_token_add_organizations`, `codex_cli_simplified_
    /// flow`, `originator`). Empty for every other config, so their authorize
    /// URLs stay byte-identical.
    pub authorize_extra: &'static [(&'static str, &'static str)],
    /// Interactive login mechanism (device-code vs PKCE-localhost).
    pub flow: OAuthFlow,
    /// Body encoding the token endpoint expects (form vs JSON).
    pub token_body: OAuthTokenBody,
    /// Second-stage token-exchange endpoint `(host, path)` for the GitHub
    /// Copilot two-stage flow (github-copilot): `("https://api.github.com",
    /// "/copilot_internal/v2/token")`. The durable GitHub token is exchanged
    /// here for the short-lived copilot token, at BOTH login and every re-mint
    /// "refresh". `None` for the standard flows (xai-grok, claude-pro-max),
    /// whose refresh is a plain `refresh_token` grant against `token_host`.
    pub copilot_exchange: Option<(&'static str, &'static str)>,
    /// The access token MUST carry a `chatgpt_account_id` claim (openai-codex):
    /// it becomes the `chatgpt-account-id` inference header, so a token without
    /// it cannot authorize a request. Login AND every refresh fail fast when the
    /// claim is absent. `false` everywhere else — this is an explicit provider
    /// fact, NEVER inferred from the token-body encoding (a plain form-encoded
    /// token endpoint is the OAuth norm and must not inherit this requirement).
    pub requires_chatgpt_account_id: bool,
}

/// xAI / Grok subscription device-code OAuth (ported from Pi
/// `earendil-works/pi` `auth/oauth/xai.ts`).
pub const XAI_OAUTH_CONFIG: OAuthConfig = OAuthConfig {
    client_id: "b1a00492-073a-47ea-816f-4c329264a828",
    auth_host: "https://auth.x.ai",
    device_path: "/oauth2/device/code",
    token_host: "https://auth.x.ai",
    token_path: "/oauth2/token",
    scope: "openid profile email offline_access grok-cli:access api:access",
    scope_key: "oauth/xai",
    extra_device_field: Some(("referrer", "kigi")),
    authorize_extra: &[],
    flow: OAuthFlow::DeviceCode,
    token_body: OAuthTokenBody::Form,
    copilot_exchange: None,
    requires_chatgpt_account_id: false,
};

/// Base-URL override for the Claude Pro/Max OAuth channel (dev/test escape
/// hatch). Production defaults to `https://api.anthropic.com/v1`.
pub const CLAUDE_OAUTH_BASE_URL_ENV: &str = "KIGI_CLAUDE_OAUTH_BASE_URL";

/// Claude Pro/Max subscription OAuth (authorization-code + PKCE S256, loopback
/// callback). Authoritative constants from Pi `earendil-works/pi`
/// `auth/oauth/anthropic.ts`: authorize host `https://claude.ai`, token host
/// `https://platform.claude.com` (JSON body), public Claude Code client id.
pub const CLAUDE_OAUTH_CONFIG: OAuthConfig = OAuthConfig {
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    auth_host: "https://claude.ai",
    device_path: "/oauth/authorize",
    token_host: "https://platform.claude.com",
    token_path: "/v1/oauth/token",
    scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    scope_key: "oauth/claude-pro-max",
    extra_device_field: None,
    authorize_extra: &[],
    flow: OAuthFlow::PkceLocalhost {
        redirect_port: 53692,
        redirect_path: "/callback",
    },
    token_body: OAuthTokenBody::Json,
    copilot_exchange: None,
    requires_chatgpt_account_id: false,
};

/// Base-URL override for the GitHub Copilot inference/listing channel
/// (dev/test escape hatch). Production defaults to the individual-subscription
/// endpoint `https://api.individual.githubcopilot.com`.
pub const COPILOT_BASE_URL_ENV: &str = "KIGI_COPILOT_BASE_URL";

/// GitHub Copilot subscription OAuth (two-stage device flow → copilot-token
/// exchange). Authoritative constants from Pi `earendil-works/pi`
/// `auth/oauth/github-copilot.ts`: the VS Code Copilot Chat public client id,
/// the github.com device endpoints, and the `api.github.com/copilot_internal/
/// v2/token` copilot-token exchange (Stage 2). The device flow mints the
/// durable GitHub token; the exchange re-mints the short-lived copilot token.
pub const COPILOT_OAUTH_CONFIG: OAuthConfig = OAuthConfig {
    client_id: "Iv1.b507a08c87ecfe98",
    auth_host: "https://github.com",
    device_path: "/login/device/code",
    token_host: "https://github.com",
    token_path: "/login/oauth/access_token",
    scope: "read:user",
    scope_key: "oauth/github-copilot",
    extra_device_field: None,
    authorize_extra: &[],
    flow: OAuthFlow::GithubDeviceCopilot,
    token_body: OAuthTokenBody::GithubCopilotExchange,
    copilot_exchange: Some(("https://api.github.com", "/copilot_internal/v2/token")),
    requires_chatgpt_account_id: false,
};

/// Base-URL override for the ChatGPT/Codex OAuth inference channel (dev/test
/// escape hatch). Production defaults to the ChatGPT Codex backend
/// `https://chatgpt.com/backend-api/codex`; Kigi's Responses path posts to
/// `{base}/responses`.
pub const CODEX_BASE_URL_ENV: &str = "KIGI_CODEX_BASE_URL";

/// ChatGPT/Codex subscription OAuth (authorization-code + PKCE S256, loopback
/// callback on port 1455 path `/auth/callback`, FORM token body). Authoritative
/// constants from the official Codex CLI + Pi `earendil-works/pi`
/// `auth/oauth/openai-codex.ts`: authorize + token host `https://auth.openai.com`,
/// the `codex_cli_simplified_flow` login client id, and the three authorize-only
/// extra params. Unlike claude the `state` is fresh-random (NOT the verifier),
/// and refresh is a plain `refresh_token` FORM grant (the generic device
/// refresher's `Form` path). The minted `access_token` is a JWT carrying the
/// `chatgpt_account_id` claim consumed at inference time.
pub const CODEX_OAUTH_CONFIG: OAuthConfig = OAuthConfig {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    auth_host: "https://auth.openai.com",
    device_path: "/oauth/authorize",
    token_host: "https://auth.openai.com",
    token_path: "/oauth/token",
    scope: "openid profile email offline_access",
    scope_key: "oauth/openai-codex",
    extra_device_field: None,
    authorize_extra: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ],
    flow: OAuthFlow::PkceLocalhost {
        redirect_port: 1455,
        redirect_path: "/auth/callback",
    },
    token_body: OAuthTokenBody::Form,
    copilot_exchange: None,
    requires_chatgpt_account_id: true,
};

/// The generic device-code OAuth config for a platform, or `None` for API-key
/// platforms and for Kimi Code (whose bespoke flow uses no generic config).
pub fn oauth_config_for_scope_key(scope_key: &str) -> Option<&'static OAuthConfig> {
    PlatformId::ALL
        .into_iter()
        .find_map(|p| p.oauth().filter(|c| c.scope_key == scope_key))
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
    /// Generic device-code OAuth config, for a `uses_oauth` platform whose
    /// login is the RFC-8628 device-code path (xai-grok). `None` for API-key
    /// platforms AND for Kimi Code (whose bespoke device flow lives in
    /// kigi-shell, not this generic config).
    oauth: Option<&'static OAuthConfig>,
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
    // Kimi Code keeps its bespoke device flow (kigi-shell), not the generic
    // OAuthConfig path — see architecture decision #1.
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["MISTRAL_API_KEY"],
    vendor: "Mistral",
    console_host: Some("console.mistral.ai"),
    login_label: Some("Mistral (API key)"),
    models_dev_id: Some("mistral"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    // Mistral's strict validator 422s on `stream_options`, its reasoning
    // models return array content, and tool-call ids must be EXACTLY nine
    // `[a-zA-Z0-9]` chars — the Mistral dialect strips stream_options and
    // deterministically remaps non-conforming (foreign/OpenAI-style) ids;
    // the response deserializer handles arrays universally.
    chat_compat: PlatformChatCompat::Mistral,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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
    oauth: None,
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

pub const XAI_BASE_URL_ENV: &str = "KIGI_XAI_BASE_URL";
const XAI_SPEC: PlatformSpec = PlatformSpec {
    id: "xai",
    display_name: "xAI (Grok)",
    base_url: BaseUrlSource::EnvOr {
        env: XAI_BASE_URL_ENV,
        default: "https://api.x.ai/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    // NOTE: `XAI_API_KEY` is ALSO read as a legacy fallback by the house BYOK
    // path (`read_xai_api_key_env`), whose primary env is now `KIGI_API_KEY`.
    // Here it is the x.ai/Grok provider key (its canonical ecosystem name).
    api_key_envs: &["XAI_API_KEY"],
    vendor: "xAI",
    console_host: Some("console.x.ai"),
    login_label: Some("xAI (Grok) (API key)"),
    models_dev_id: Some("xai"),
    // /v1/models is minimal (ids only; rich metadata lives on the non-standard
    // /v1/language-models), so take context/limits from models.dev enrichment.
    // The /v1/models ids match the models.dev "xai" keys byte-for-byte
    // (grok-4.5, grok-4.20-0309-reasoning, ...); restrict to tool-calling chat
    // models to drop the grok-imagine-* image/video generators.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // /v1/models requires auth (401 without a key), so it doubles as the key
    // validator; no separate public endpoint needed.
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

/// Base-URL override for the xAI Grok subscription (OAuth) platform. Its own
/// env (distinct from `KIGI_XAI_BASE_URL`) so a mock can target it
/// independently of the API-key `xai` row.
pub const XAI_GROK_BASE_URL_ENV: &str = "KIGI_XAI_GROK_BASE_URL";
const XAI_GROK_SPEC: PlatformSpec = PlatformSpec {
    id: "xai-grok",
    display_name: "xAI (Grok subscription)",
    // Same wire as the API-key `xai` row (api.x.ai/v1), but reached with an
    // OAuth subscription bearer instead of an API key.
    base_url: BaseUrlSource::EnvOr {
        env: XAI_GROK_BASE_URL_ENV,
        default: "https://api.x.ai/v1",
    },
    uses_oauth: true,
    oauth: Some(&XAI_OAUTH_CONFIG),
    allowed_model_prefixes: None,
    // OAuth channel: no API key envs (the device-flow session is the bearer).
    api_key_envs: &[],
    vendor: "xAI",
    console_host: Some("x.ai"),
    login_label: Some("xAI Grok (subscription)"),
    models_dev_id: Some("xai"),
    // Same as XAI_SPEC: bare-id /v1/models, enrich from models.dev "xai",
    // restrict to tool-calling chat models (drops the grok-imagine-* media
    // generators).
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const QWEN_TOKEN_PLAN_BASE_URL_ENV: &str = "KIGI_QWEN_TOKEN_PLAN_BASE_URL";
const QWEN_TOKEN_PLAN_SPEC: PlatformSpec = PlatformSpec {
    id: "qwen-token-plan",
    display_name: "Qwen Token Plan",
    base_url: BaseUrlSource::EnvOr {
        env: QWEN_TOKEN_PLAN_BASE_URL_ENV,
        default: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["QWEN_TOKEN_PLAN_API_KEY"],
    vendor: "Alibaba",
    console_host: Some("modelstudio.console.alibabacloud.com"),
    login_label: Some("Qwen Token Plan (API key)"),
    models_dev_id: Some("alibaba-token-plan"),
    // DashScope compatible-mode /models is auth-gated (no wire metadata), so
    // take context/limits from the models.dev "alibaba-token-plan" snapshot;
    // restrict to tool-calling chat models to drop the qwen-image / wan image
    // generators the token plan also lists.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // /models requires auth (401 without a key), so it doubles as the key
    // validator.
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const QWEN_TOKEN_PLAN_CN_BASE_URL_ENV: &str = "KIGI_QWEN_TOKEN_PLAN_CN_BASE_URL";
const QWEN_TOKEN_PLAN_CN_SPEC: PlatformSpec = PlatformSpec {
    id: "qwen-token-plan-cn",
    display_name: "Qwen Token Plan (China)",
    base_url: BaseUrlSource::EnvOr {
        env: QWEN_TOKEN_PLAN_CN_BASE_URL_ENV,
        default: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["QWEN_TOKEN_PLAN_CN_API_KEY"],
    vendor: "Alibaba",
    console_host: Some("bailian.console.aliyun.com"),
    login_label: Some("Qwen Token Plan China (API key)"),
    models_dev_id: Some("alibaba-token-plan-cn"),
    // China endpoint; same DashScope compatible-mode shape as the global plan.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

const KIMI_CODING_SPEC: PlatformSpec = PlatformSpec {
    id: "kimi-coding",
    display_name: "Kimi For Coding",
    // Same endpoint as the OAuth `kimi-code` platform (KIGI_CODE_BASE_URL
    // override), but authenticated with a static KIMI_API_KEY instead of the
    // device flow. Kimi's /coding/v1/models serves its own metadata and the
    // Kimi thinking dialect applies verbatim.
    base_url: BaseUrlSource::KigiEnvCoding,
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["KIMI_API_KEY"],
    vendor: "Kimi",
    console_host: Some("www.kimi.com"),
    login_label: Some("Kimi For Coding (API key)"),
    models_dev_id: Some("kimi-for-coding"),
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Kimi,
    key_header: PlatformKeyHeader::Bearer,
    // /coding/v1/models requires auth (401 without a key), so it doubles as
    // the key validator.
    key_validation_path: None,
    restrict_to_enriched: false,
    strip_listing_id_prefix: None,
};

pub const ZAI_BASE_URL_ENV: &str = "KIGI_ZAI_BASE_URL";
const ZAI_SPEC: PlatformSpec = PlatformSpec {
    id: "zai",
    display_name: "Z.AI",
    // Z.AI coding plan (GLM). Per Pi (earendil-works/pi) this is plain
    // OpenAI-compatible chat completions — no special thinking dialect.
    base_url: BaseUrlSource::EnvOr {
        env: ZAI_BASE_URL_ENV,
        default: "https://api.z.ai/api/coding/paas/v4",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["ZAI_API_KEY"],
    vendor: "Z.AI",
    console_host: Some("z.ai"),
    login_label: Some("Z.AI (API key)"),
    models_dev_id: Some("zai-coding-plan"),
    // /models auth-gated → validator; enrichment from models.dev; restrict to
    // tool-calling GLM chat models. Live ids match the snapshot keys
    // (glm-4.7, glm-5-turbo, glm-5.2, ...) byte-for-byte (verified vs Pi).
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const ZAI_CODING_CN_BASE_URL_ENV: &str = "KIGI_ZAI_CODING_CN_BASE_URL";
const ZAI_CODING_CN_SPEC: PlatformSpec = PlatformSpec {
    id: "zai-coding-cn",
    display_name: "Z.AI Coding (China)",
    // Zhipu/BigModel-hosted CN coding plan; same OpenAI-compatible shape.
    base_url: BaseUrlSource::EnvOr {
        env: ZAI_CODING_CN_BASE_URL_ENV,
        default: "https://open.bigmodel.cn/api/coding/paas/v4",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["ZAI_CODING_CN_API_KEY"],
    vendor: "Z.AI",
    console_host: Some("open.bigmodel.cn"),
    login_label: Some("Z.AI Coding China (API key)"),
    models_dev_id: Some("zhipuai-coding-plan"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const XIAOMI_BASE_URL_ENV: &str = "KIGI_XIAOMI_BASE_URL";
const XIAOMI_SPEC: PlatformSpec = PlatformSpec {
    id: "xiaomi",
    display_name: "Xiaomi MiMo",
    // Xiaomi MiMo. Per Pi this is plain OpenAI-compatible chat completions.
    base_url: BaseUrlSource::EnvOr {
        env: XIAOMI_BASE_URL_ENV,
        default: "https://api.xiaomimimo.com/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["XIAOMI_API_KEY"],
    vendor: "Xiaomi",
    console_host: Some("xiaomimimo.com"),
    login_label: Some("Xiaomi MiMo (API key)"),
    models_dev_id: Some("xiaomi"),
    // /models auth-gated → validator; enrichment from models.dev; restrict to
    // tool-calling chat models. Live ids match the snapshot keys (mimo-v2.5,
    // mimo-v2-pro, ...) byte-for-byte (verified vs Pi).
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const XIAOMI_TOKEN_PLAN_CN_BASE_URL_ENV: &str = "KIGI_XIAOMI_TOKEN_PLAN_CN_BASE_URL";
const XIAOMI_TOKEN_PLAN_CN_SPEC: PlatformSpec = PlatformSpec {
    id: "xiaomi-token-plan-cn",
    display_name: "Xiaomi Token Plan (China)",
    base_url: BaseUrlSource::EnvOr {
        env: XIAOMI_TOKEN_PLAN_CN_BASE_URL_ENV,
        default: "https://token-plan-cn.xiaomimimo.com/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
    vendor: "Xiaomi",
    console_host: Some("xiaomimimo.com"),
    login_label: Some("Xiaomi Token Plan China (API key)"),
    models_dev_id: Some("xiaomi-token-plan-cn"),
    // The CN token plan listing also carries mimo TTS models (tool_call=false)
    // → restrict drops them, keeping only the tool-calling chat models.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::ChatCompletions,
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: true,
};

pub const MINIMAX_BASE_URL_ENV: &str = "KIGI_MINIMAX_BASE_URL";
const MINIMAX_SPEC: PlatformSpec = PlatformSpec {
    id: "minimax",
    display_name: "MiniMax",
    // Per Pi, MiniMax is driven through its Anthropic-compatible surface
    // (baseUrl .../anthropic). Kigi appends bare paths, so the base carries
    // the /v1 suffix: listing → .../anthropic/v1/models?limit=1000, inference
    // → .../anthropic/v1/messages. Reuses the Anthropic Messages machinery.
    base_url: BaseUrlSource::EnvOr {
        env: MINIMAX_BASE_URL_ENV,
        default: "https://api.minimax.io/anthropic/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["MINIMAX_API_KEY"],
    vendor: "MiniMax",
    console_host: Some("platform.minimax.io"),
    login_label: Some("MiniMax (API key)"),
    models_dev_id: Some("minimax"),
    // Anthropic listing (x-api-key + anthropic-version) serves the id list;
    // enrichment fills context. Catalog is clean (7 MiniMax-M* chat models),
    // so no restriction — and restrict=false keeps launch-day models that
    // models.dev has not indexed yet.
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::Messages,
    listing: ListingDialect::Anthropic,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::XApiKey,
    // /anthropic/v1/models requires x-api-key (401 without), so it validates.
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: false,
};

pub const MINIMAX_CN_BASE_URL_ENV: &str = "KIGI_MINIMAX_CN_BASE_URL";
const MINIMAX_CN_SPEC: PlatformSpec = PlatformSpec {
    id: "minimax-cn",
    display_name: "MiniMax (China)",
    base_url: BaseUrlSource::EnvOr {
        env: MINIMAX_CN_BASE_URL_ENV,
        default: "https://api.minimaxi.com/anthropic/v1",
    },
    uses_oauth: false,
    oauth: None,
    allowed_model_prefixes: None,
    api_key_envs: &["MINIMAX_CN_API_KEY"],
    vendor: "MiniMax",
    console_host: Some("platform.minimaxi.com"),
    login_label: Some("MiniMax China (API key)"),
    models_dev_id: Some("minimax-cn"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::Messages,
    listing: ListingDialect::Anthropic,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::XApiKey,
    key_validation_path: None,
    strip_listing_id_prefix: None,
    restrict_to_enriched: false,
};

/// Claude Pro/Max subscription via PKCE-localhost OAuth. Reaches
/// `api.anthropic.com` with an OAuth `sk-ant-oat…` bearer (NOT an API key) —
/// same Anthropic Messages + listing wire as the API-key `anthropic` row, plus
/// the OAuth identity headers + "You are Claude Code" system prefix (gated on
/// this platform's OAuth path in the sampler/fetch, so the API-key rows stay
/// byte-identical).
const CLAUDE_PRO_MAX_SPEC: PlatformSpec = PlatformSpec {
    id: "claude-pro-max",
    display_name: "Claude Pro/Max",
    // WITH /v1 so listing → /v1/models and inference → /v1/messages, matching
    // ANTHROPIC_SPEC's base handling.
    base_url: BaseUrlSource::EnvOr {
        env: CLAUDE_OAUTH_BASE_URL_ENV,
        default: "https://api.anthropic.com/v1",
    },
    uses_oauth: true,
    oauth: Some(&CLAUDE_OAUTH_CONFIG),
    allowed_model_prefixes: None,
    // OAuth channel: no API key envs (the PKCE session is the bearer).
    api_key_envs: &[],
    vendor: "Anthropic",
    console_host: None,
    login_label: Some("Claude Pro/Max (subscription)"),
    models_dev_id: Some("anthropic"),
    wire_serves_metadata: false,
    wire_api: PlatformWireApi::Messages,
    listing: ListingDialect::Anthropic,
    // Passthrough is ignored for the Messages backend.
    chat_compat: PlatformChatCompat::Passthrough,
    // OAuth uses Authorization: Bearer, NOT x-api-key.
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

const GITHUB_COPILOT_SPEC: PlatformSpec = PlatformSpec {
    id: "github-copilot",
    display_name: "GitHub Copilot",
    base_url: BaseUrlSource::EnvOr {
        env: COPILOT_BASE_URL_ENV,
        default: "https://api.individual.githubcopilot.com",
    },
    uses_oauth: true,
    oauth: Some(&COPILOT_OAUTH_CONFIG),
    allowed_model_prefixes: None,
    // OAuth channel: no API key envs (the copilot session is the bearer).
    api_key_envs: &[],
    vendor: "GitHub",
    console_host: Some("github.com"),
    login_label: Some("GitHub Copilot (subscription)"),
    models_dev_id: Some("github-copilot"),
    // Copilot /models serves availability flags (model_picker_enabled/policy/
    // tool_calls), NOT context/thinking metadata — enrich from models.dev.
    wire_serves_metadata: false,
    // ChatCompletions ONLY: the catalog is filtered (see
    // `parse_github_copilot_listing`) to the openai-completions-served models.
    // The claude-4.x/5.x (messages) and gpt-5/oswe/mai- (responses-only) models
    // this endpoint also lists are EXCLUDED — Kigi is one-wire-per-platform and
    // per-model wire routing is deferred (documented limitation).
    wire_api: PlatformWireApi::ChatCompletions,
    // OpenAI-shape listing endpoint, but with Copilot-specific availability
    // fields — parsed by `parse_github_copilot_listing`, gated on the platform.
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    // The copilot listing filter + the wire-compat filter govern the catalog,
    // NOT the enrichment membership (a live but enrichment-lagging model stays).
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
};

const OPENAI_CODEX_SPEC: PlatformSpec = PlatformSpec {
    id: "openai-codex",
    display_name: "ChatGPT (Codex)",
    // Kigi's Responses path posts to `{base}/responses`; the codex backend
    // serves it at `.../codex/responses`, so the base carries the `/codex` tail.
    base_url: BaseUrlSource::EnvOr {
        env: CODEX_BASE_URL_ENV,
        default: "https://chatgpt.com/backend-api/codex",
    },
    uses_oauth: true,
    oauth: Some(&CODEX_OAUTH_CONFIG),
    allowed_model_prefixes: None,
    // OAuth channel: no API key envs (the PKCE session is the bearer).
    api_key_envs: &[],
    vendor: "OpenAI",
    console_host: Some("chatgpt.com"),
    login_label: Some("ChatGPT Plus/Pro (Codex)"),
    // HARDCODED catalog (see `hardcoded_catalog`): NOT enriched from models.dev
    // and NOT live-fetched — OpenAI exposes no stable public models endpoint for
    // this backend.
    models_dev_id: None,
    // The catalog is compiled-in and already carries context/thinking metadata,
    // so enrichment (and its network refresh) is skipped entirely.
    wire_serves_metadata: true,
    wire_api: PlatformWireApi::Responses,
    // Unused: the catalog does NOT come from a live `/models` listing (the
    // fetch path short-circuits to `hardcoded_catalog`).
    listing: ListingDialect::OpenAi,
    chat_compat: PlatformChatCompat::Passthrough,
    key_header: PlatformKeyHeader::Bearer,
    restrict_to_enriched: false,
    key_validation_path: None,
    strip_listing_id_prefix: None,
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
    /// xAI Grok platform (API key, OpenAI-compatible ChatCompletions).
    Xai,
    /// Alibaba Qwen Token Plan, global (API key, DashScope compatible-mode).
    QwenTokenPlan,
    /// Alibaba Qwen Token Plan, China (API key, DashScope compatible-mode).
    QwenTokenPlanCn,
    /// Kimi For Coding via a static KIMI_API_KEY (same endpoint as `KimiCode`).
    KimiCoding,
    /// Z.AI coding plan, global (API key, OpenAI-compatible GLM).
    Zai,
    /// Z.AI coding plan, China / Zhipu BigModel (API key, OpenAI-compatible).
    ZaiCodingCn,
    /// Xiaomi MiMo, global (API key, OpenAI-compatible).
    Xiaomi,
    /// Xiaomi Token Plan, China (API key, OpenAI-compatible).
    XiaomiTokenPlanCn,
    /// MiniMax, global (API key, Anthropic-compatible Messages).
    Minimax,
    /// MiniMax, China (API key, Anthropic-compatible Messages).
    MinimaxCn,
    /// xAI Grok subscription via device-code OAuth (same wire as `Xai`).
    XaiGrok,
    /// Claude Pro/Max subscription via PKCE-localhost OAuth (Anthropic Messages
    /// wire reached with an OAuth bearer instead of an API key).
    ClaudeProMax,
    /// GitHub Copilot subscription via the two-stage device-code OAuth flow
    /// (ChatCompletions wire reached with the short-lived copilot token).
    GithubCopilot,
    /// ChatGPT Plus/Pro subscription via PKCE-localhost OAuth against the
    /// ChatGPT Codex backend (Responses wire reached with an OAuth bearer +
    /// the `chatgpt-account-id` JWT claim). HARDCODED catalog, no live listing.
    OpenaiCodex,
}

impl PlatformId {
    /// All platforms, in catalog precedence order: the subscription channel
    /// first so "default model = first list item" favors it when present.
    pub const ALL: [PlatformId; 29] = [
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
        Self::Xai,
        Self::QwenTokenPlan,
        Self::QwenTokenPlanCn,
        Self::KimiCoding,
        Self::Zai,
        Self::ZaiCodingCn,
        Self::Xiaomi,
        Self::XiaomiTokenPlanCn,
        Self::Minimax,
        Self::MinimaxCn,
        Self::XaiGrok,
        Self::ClaudeProMax,
        Self::GithubCopilot,
        Self::OpenaiCodex,
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
            Self::Xai => &XAI_SPEC,
            Self::QwenTokenPlan => &QWEN_TOKEN_PLAN_SPEC,
            Self::QwenTokenPlanCn => &QWEN_TOKEN_PLAN_CN_SPEC,
            Self::KimiCoding => &KIMI_CODING_SPEC,
            Self::Zai => &ZAI_SPEC,
            Self::ZaiCodingCn => &ZAI_CODING_CN_SPEC,
            Self::Xiaomi => &XIAOMI_SPEC,
            Self::XiaomiTokenPlanCn => &XIAOMI_TOKEN_PLAN_CN_SPEC,
            Self::Minimax => &MINIMAX_SPEC,
            Self::MinimaxCn => &MINIMAX_CN_SPEC,
            Self::XaiGrok => &XAI_GROK_SPEC,
            Self::ClaudeProMax => &CLAUDE_PRO_MAX_SPEC,
            Self::GithubCopilot => &GITHUB_COPILOT_SPEC,
            Self::OpenaiCodex => &OPENAI_CODEX_SPEC,
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

    /// Generic device-code OAuth config for this platform, or `None` for
    /// API-key platforms and for Kimi Code (bespoke flow). A `Some` here means
    /// `uses_oauth()` is also true; the fetch/base-url routing treats these
    /// providers as their own `base_url()` (not the Kimi `proxy_url()`).
    pub fn oauth(self) -> Option<&'static OAuthConfig> {
        self.spec().oauth
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

    /// True ONLY for `github-copilot`: its `/models` listing and every
    /// `/chat/completions` inference request must carry the VS Code Copilot
    /// editor-identity headers (User-Agent / Editor-Version /
    /// Editor-Plugin-Version / Copilot-Integration-Id). The shell gates the
    /// listing headers and the sampler gates the inference headers on this, so
    /// every other ChatCompletions platform's request stays byte-identical.
    pub fn sends_copilot_editor_headers(self) -> bool {
        matches!(self, Self::GithubCopilot)
    }

    /// True ONLY for `openai-codex`: its `/responses` inference request must
    /// carry the Codex identity headers (`chatgpt-account-id` from the bearer
    /// JWT, `originator: codex_cli_rs`, `OpenAI-Beta: responses=experimental`, a
    /// codex `User-Agent`). The sampler gates these on this predicate, so the
    /// API-key `openai` Responses requests stay byte-identical.
    pub fn sends_codex_responses_headers(self) -> bool {
        matches!(self, Self::OpenaiCodex)
    }

    /// The compiled-in catalog for a platform that serves NO live `/models`
    /// listing (openai-codex), or `None` when the catalog comes from the wire.
    ///
    /// openai-codex's 4 models are HARDCODED (read from the official Codex CLI's
    /// `models_cache.json`, the `visibility=="list"` AND `supported_in_api==true`
    /// set) because OpenAI exposes no stable public models endpoint for the
    /// ChatGPT Codex backend. Each entry carries context window + per-model
    /// selectable reasoning efforts (incl. the codex-only `xhigh`/`max`/`ultra`
    /// tiers), so the fetch path maps them through the SAME
    /// `platform_wire_model_to_entry` output as a live listing — no new type.
    pub fn hardcoded_catalog(self) -> Option<Vec<WireModel>> {
        match self {
            Self::OpenaiCodex => Some(openai_codex_wire_models()),
            _ => None,
        }
    }
}

/// One hardcoded openai-codex model as a [`WireModel`] (context 272000, thinking
/// + image input, selectable efforts). `efforts` are the exact per-model
/// supported tiers; `default` is the model's default effort.
fn codex_wire_model(slug: &str, display_name: &str, efforts: &[&str], default: &str) -> WireModel {
    WireModel {
        id: slug.to_string(),
        context_length: 272_000,
        supports_reasoning: true,
        supports_image_in: true,
        supports_video_in: false,
        display_name: Some(display_name.to_string()),
        max_output_tokens: 0,
        supports_thinking_type: None,
        think_efforts: Some(WireThinkEfforts {
            support: true,
            valid_efforts: efforts.iter().map(|s| (*s).to_string()).collect(),
            default_effort: Some(default.to_string()),
        }),
    }
}

/// The HARDCODED openai-codex catalog: exactly the 4 `visibility=="list"` AND
/// `supported_in_api==true` models from the Codex CLI model cache. The
/// `gpt-5.3-codex-spark` (supported_in_api=false → not served by /responses),
/// `gpt-5.4`, `gpt-5.4-mini`, and `codex-auto-review` (visibility="hide") models
/// are intentionally EXCLUDED — they would list-but-not-work or are not
/// user-facing (fail-fast: never advertise a model the backend rejects).
fn openai_codex_wire_models() -> Vec<WireModel> {
    vec![
        codex_wire_model(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            &["low", "medium", "high", "xhigh", "max", "ultra"],
            "low",
        ),
        codex_wire_model(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            &["low", "medium", "high", "xhigh", "max", "ultra"],
            "medium",
        ),
        codex_wire_model(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            &["low", "medium", "high", "xhigh", "max"],
            "medium",
        ),
        codex_wire_model(
            "gpt-5.5",
            "GPT-5.5",
            &["low", "medium", "high", "xhigh"],
            "medium",
        ),
    ]
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
    // Sniff the top-level shape. Anthropic itself serves the {data:[...]}
    // envelope, but Anthropic-COMPATIBLE surfaces (e.g. MiniMax's /anthropic
    // endpoint) may serve a bare array — accept both so such a provider isn't
    // silently emptied (mirrors `parse_openai_listing`'s tolerance).
    let data: Vec<AnthropicModel> = if json.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<AnthropicModel>>(json)?
    } else {
        let listing: AnthropicListing = serde_json::from_str(json)?;
        if listing.has_more {
            tracing::warn!(
                fetched = listing.data.len(),
                "anthropic /models reports more pages beyond limit=1000; \
                 listing may be incomplete"
            );
        }
        listing.data
    };
    Ok(data
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

// ── GitHub Copilot listing adapter (github-copilot) ─────────────────────────

/// A Copilot model id that routes to the anthropic-messages wire in Pi
/// (`/^claude-(haiku|sonnet|opus)-[45]([.\-]|$)/`) — EXCLUDED from Kigi's
/// ChatCompletions catalog. Matches `claude-{haiku,sonnet,opus}-` followed by a
/// single `4`/`5` and then `.`, `-`, or end (so `claude-fable-5` is NOT a match
/// and stays served; `claude-sonnet-45` is not a real family and does not match).
fn is_copilot_messages_claude(id: &str) -> bool {
    for family in ["claude-haiku-", "claude-sonnet-", "claude-opus-"] {
        let Some(rest) = id.strip_prefix(family) else {
            continue;
        };
        let mut chars = rest.chars();
        if matches!(chars.next(), Some('4') | Some('5'))
            && matches!(chars.next(), None | Some('.') | Some('-'))
        {
            return true;
        }
    }
    false
}

/// A Copilot model id served ONLY through the `/responses` endpoint in Pi
/// (`gpt-5*` / `oswe*` / `mai-*`) — EXCLUDED from Kigi's ChatCompletions catalog.
fn is_copilot_responses_only(id: &str) -> bool {
    id.starts_with("gpt-5") || id.starts_with("oswe") || id.starts_with("mai-")
}

/// Whether a Copilot model id is served by the openai-completions wire — the
/// ONLY wire Kigi's github-copilot platform speaks. Excludes the claude-4.x/5.x
/// (messages) and gpt-5/oswe/mai- (responses-only) ids that would fail at
/// inference on `/chat/completions` (documented per-model-routing limitation).
pub fn is_copilot_completions_served(id: &str) -> bool {
    !is_copilot_messages_claude(id) && !is_copilot_responses_only(id)
}

#[derive(serde::Deserialize)]
struct CopilotListing {
    /// No default: a 200 body without `data` is a contract violation and must
    /// error like the OpenAI-shape branch, not yield an empty catalog.
    data: Vec<CopilotListingModel>,
}

#[derive(serde::Deserialize)]
struct CopilotListingModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    policy: Option<CopilotPolicy>,
    #[serde(default)]
    capabilities: Option<CopilotCapabilities>,
}

#[derive(serde::Deserialize)]
struct CopilotPolicy {
    #[serde(default)]
    state: Option<String>,
}

#[derive(serde::Deserialize)]
struct CopilotCapabilities {
    #[serde(default)]
    supports: Option<CopilotSupports>,
}

#[derive(serde::Deserialize)]
struct CopilotSupports {
    /// Absent means "not declined" → allowed (Pi: `supports.tool_calls !==
    /// false`). Only an explicit `false` drops the model.
    #[serde(default)]
    tool_calls: Option<bool>,
}

/// Parse the GitHub Copilot `GET {base}/models` response and apply BOTH filters
/// (a Copilot quirk, gated on the platform):
/// 1. availability — keep iff `model_picker_enabled == true` AND
///    `policy.state != "disabled"` AND `capabilities.supports.tool_calls !=
///    false` (Pi `isSelectableCopilotModel`);
/// 2. wire-compat — keep iff the id is openai-completions-served (drops the
///    claude-4.x/5.x messages models and the gpt-5/oswe/mai- responses-only
///    models, which Kigi's single-wire platform cannot route).
///
/// Metadata (context window, thinking) is NOT served here — enrichment from
/// models.dev "github-copilot" fills it downstream.
pub fn parse_github_copilot_listing(json: &str) -> Result<Vec<WireModel>, serde_json::Error> {
    let listing: CopilotListing = serde_json::from_str(json)?;
    let kept = listing
        .data
        .into_iter()
        .filter(|m| {
            let picker_enabled = m.model_picker_enabled;
            let policy_ok = m
                .policy
                .as_ref()
                .and_then(|p| p.state.as_deref())
                .is_none_or(|state| state != "disabled");
            let tool_calls_ok = m
                .capabilities
                .as_ref()
                .and_then(|c| c.supports.as_ref())
                .and_then(|s| s.tool_calls)
                != Some(false);
            picker_enabled && policy_ok && tool_calls_ok && is_copilot_completions_served(&m.id)
        })
        .map(|m| WireModel {
            id: m.id,
            context_length: 0,
            supports_reasoning: false,
            supports_image_in: false,
            supports_video_in: false,
            display_name: m.name,
            max_output_tokens: 0,
            supports_thinking_type: None,
            think_efforts: None,
        })
        .collect();
    Ok(kept)
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

    /// Anthropic-COMPATIBLE surfaces (e.g. MiniMax's /anthropic endpoint) may
    /// serve a bare array instead of the `{data:[...]}` envelope; the parser
    /// accepts both so such a provider isn't silently emptied. A bare object
    /// (not an array, no `data`) still errors.
    #[test]
    fn anthropic_listing_accepts_envelope_and_bare_array() {
        let envelope =
            serde_json::json!({ "data": [ { "id": "MiniMax-M2.5", "max_input_tokens": 204_800 } ] })
                .to_string();
        let bare = serde_json::json!([ { "id": "MiniMax-M2.5", "max_input_tokens": 204_800 } ])
            .to_string();
        for json in [&envelope, &bare] {
            let models = parse_anthropic_listing(json).expect("both shapes parse");
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "MiniMax-M2.5");
            assert_eq!(models[0].context_length, 204_800);
        }
        // A bare object without `data` is still a contract violation.
        assert!(parse_anthropic_listing(r#"{"foo":1}"#).is_err());
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

    /// xai-grok is the second refreshable-OAuth platform: it carries a generic
    /// device-code `OAuthConfig` (Kimi Code carries `None`), reuses the xai
    /// wire (models.dev "xai", restrict-to-enriched, Bearer, ChatCompletions),
    /// and keys its models under `xai-grok/` — distinct from the API-key `xai`.
    #[test]
    fn xai_grok_is_a_generic_oauth_platform() {
        let g = PlatformId::XaiGrok;
        assert_eq!(g.as_str(), "xai-grok");
        assert!(g.uses_oauth());
        // Kimi Code is uses_oauth yet carries no generic config (bespoke flow).
        assert!(PlatformId::KimiCode.uses_oauth());
        assert_eq!(PlatformId::KimiCode.oauth(), None);
        let cfg = g
            .oauth()
            .expect("xai-grok carries a device-code OAuthConfig");
        assert_eq!(cfg, &XAI_OAUTH_CONFIG);
        assert_eq!(cfg.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
        assert_eq!(cfg.auth_host, "https://auth.x.ai");
        assert_eq!(cfg.device_path, "/oauth2/device/code");
        assert_eq!(cfg.token_path, "/oauth2/token");
        assert_eq!(
            cfg.scope,
            "openid profile email offline_access grok-cli:access api:access"
        );
        assert_eq!(cfg.scope_key, "oauth/xai");
        assert_eq!(cfg.extra_device_field, Some(("referrer", "kigi")));
        // Scope-key lookup resolves the config (drives the generic refresher).
        assert_eq!(
            oauth_config_for_scope_key("oauth/xai"),
            Some(&XAI_OAUTH_CONFIG)
        );
        assert_eq!(oauth_config_for_scope_key("oauth/kimi-code"), None);
        // Reuses the xai wire, its OWN base env, and the models.dev "xai" keys.
        assert_eq!(g.models_dev_id(), Some("xai"));
        assert!(g.restrict_to_enriched());
        assert_eq!(g.key_header(), PlatformKeyHeader::Bearer);
        assert_eq!(g.wire_api(), PlatformWireApi::ChatCompletions);
        assert_eq!(g.api_key_env_names(), &[] as &[&str]);
        assert_eq!(g.managed_model_key("grok-4.5"), "xai-grok/grok-4.5");
        let _guard = kigi_env::EnvVarGuard::set(XAI_GROK_BASE_URL_ENV, "https://mock.grok/v1");
        assert_eq!(g.base_url(), "https://mock.grok/v1");
    }

    /// claude-pro-max is the first PKCE-localhost OAuth platform: it carries a
    /// PKCE `OAuthConfig` (authorize host ≠ token host, JSON token body), reuses
    /// the Anthropic Messages + listing wire with a Bearer key header (OAuth,
    /// NOT x-api-key), enriches from models.dev "anthropic", and keys its models
    /// under `claude-pro-max/`.
    #[test]
    fn claude_pro_max_is_a_pkce_oauth_platform() {
        let c = PlatformId::ClaudeProMax;
        assert_eq!(c.as_str(), "claude-pro-max");
        assert!(c.uses_oauth());
        let cfg = c
            .oauth()
            .expect("claude-pro-max carries a PKCE OAuthConfig");
        assert_eq!(cfg, &CLAUDE_OAUTH_CONFIG);
        assert_eq!(cfg.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        // Authorize host ≠ token host (the distinguishing PKCE trait).
        assert_eq!(cfg.auth_host, "https://claude.ai");
        assert_eq!(cfg.device_path, "/oauth/authorize");
        assert_eq!(cfg.token_host, "https://platform.claude.com");
        assert_eq!(cfg.token_path, "/v1/oauth/token");
        assert_eq!(
            cfg.scope,
            "org:create_api_key user:profile user:inference \
             user:sessions:claude_code user:mcp_servers user:file_upload"
        );
        assert_eq!(cfg.scope_key, "oauth/claude-pro-max");
        assert_eq!(cfg.extra_device_field, None);
        assert_eq!(
            cfg.flow,
            OAuthFlow::PkceLocalhost {
                redirect_port: 53692,
                redirect_path: "/callback",
            }
        );
        assert_eq!(cfg.token_body, OAuthTokenBody::Json);
        // xai stays the device-code / form contract — unaffected.
        assert_eq!(XAI_OAUTH_CONFIG.flow, OAuthFlow::DeviceCode);
        assert_eq!(XAI_OAUTH_CONFIG.token_body, OAuthTokenBody::Form);
        assert_eq!(XAI_OAUTH_CONFIG.token_host, "https://auth.x.ai");
        // Scope-key lookup resolves the config (drives the generic refresher).
        assert_eq!(
            oauth_config_for_scope_key("oauth/claude-pro-max"),
            Some(&CLAUDE_OAUTH_CONFIG)
        );
        // Anthropic Messages + listing wire, reached with a Bearer OAuth token.
        assert_eq!(c.models_dev_id(), Some("anthropic"));
        assert!(!c.restrict_to_enriched());
        assert_eq!(c.key_header(), PlatformKeyHeader::Bearer);
        assert_eq!(c.wire_api(), PlatformWireApi::Messages);
        assert_eq!(c.listing(), ListingDialect::Anthropic);
        assert_eq!(c.api_key_env_names(), &[] as &[&str]);
        assert_eq!(
            c.managed_model_key("claude-opus-4-8"),
            "claude-pro-max/claude-opus-4-8"
        );
        let _guard =
            kigi_env::EnvVarGuard::set(CLAUDE_OAUTH_BASE_URL_ENV, "https://mock.claude/v1");
        assert_eq!(c.base_url(), "https://mock.claude/v1");
    }

    /// github-copilot is the two-stage device-code OAuth platform: it carries a
    /// `GithubDeviceCopilot`/`GithubCopilotExchange` `OAuthConfig` with a
    /// copilot-token exchange endpoint (Stage 2), speaks the ChatCompletions
    /// wire with a Bearer copilot token + the editor-headers gate, enriches from
    /// models.dev "github-copilot", and keys its models under `github-copilot/`.
    #[test]
    fn github_copilot_is_a_two_stage_oauth_platform() {
        let g = PlatformId::GithubCopilot;
        assert_eq!(g.as_str(), "github-copilot");
        assert!(g.uses_oauth());
        assert!(
            g.sends_copilot_editor_headers(),
            "github-copilot must gate the editor-identity headers"
        );
        // Every OTHER platform must NOT send the editor headers (regression).
        for other in PlatformId::ALL {
            if other != PlatformId::GithubCopilot {
                assert!(
                    !other.sends_copilot_editor_headers(),
                    "{} must not send the copilot editor headers",
                    other.as_str()
                );
            }
        }
        let cfg = g
            .oauth()
            .expect("github-copilot carries a two-stage OAuthConfig");
        assert_eq!(cfg, &COPILOT_OAUTH_CONFIG);
        assert_eq!(cfg.client_id, "Iv1.b507a08c87ecfe98");
        assert_eq!(cfg.auth_host, "https://github.com");
        assert_eq!(cfg.device_path, "/login/device/code");
        assert_eq!(cfg.token_path, "/login/oauth/access_token");
        assert_eq!(cfg.scope, "read:user");
        assert_eq!(cfg.scope_key, "oauth/github-copilot");
        assert_eq!(cfg.flow, OAuthFlow::GithubDeviceCopilot);
        assert_eq!(cfg.token_body, OAuthTokenBody::GithubCopilotExchange);
        assert_eq!(
            cfg.copilot_exchange,
            Some(("https://api.github.com", "/copilot_internal/v2/token")),
            "the Stage-2 copilot-token exchange endpoint must be configured"
        );
        // xai/claude carry no copilot exchange (their refresh is a plain grant).
        assert_eq!(XAI_OAUTH_CONFIG.copilot_exchange, None);
        assert_eq!(CLAUDE_OAUTH_CONFIG.copilot_exchange, None);
        // Scope-key lookup resolves the config (drives the generic refresher's
        // Copilot re-mint dispatch).
        assert_eq!(
            oauth_config_for_scope_key("oauth/github-copilot"),
            Some(&COPILOT_OAUTH_CONFIG)
        );
        // ChatCompletions wire, Bearer, models.dev "github-copilot", own base.
        assert_eq!(g.models_dev_id(), Some("github-copilot"));
        assert_eq!(g.wire_api(), PlatformWireApi::ChatCompletions);
        assert_eq!(g.listing(), ListingDialect::OpenAi);
        assert_eq!(g.key_header(), PlatformKeyHeader::Bearer);
        assert_eq!(g.api_key_env_names(), &[] as &[&str]);
        assert!(!g.restrict_to_enriched());
        assert_eq!(g.managed_model_key("gpt-4.1"), "github-copilot/gpt-4.1");
        let _guard = kigi_env::EnvVarGuard::set(COPILOT_BASE_URL_ENV, "https://mock.copilot");
        assert_eq!(g.base_url(), "https://mock.copilot");
    }

    /// openai-codex is the ChatGPT/Codex PKCE-localhost OAuth platform: it
    /// carries a `PkceLocalhost{1455, "/auth/callback"}` / FORM `OAuthConfig`
    /// with the 3 authorize-extra params, speaks the Responses wire with a Bearer
    /// OAuth token + the codex-headers gate, and is NOT models.dev-enriched (its
    /// catalog is hardcoded).
    #[test]
    fn openai_codex_is_a_pkce_responses_oauth_platform() {
        let c = PlatformId::OpenaiCodex;
        assert_eq!(c.as_str(), "openai-codex");
        assert!(c.uses_oauth());
        assert!(
            c.sends_codex_responses_headers(),
            "openai-codex must gate the Codex identity headers"
        );
        // Every OTHER platform must NOT send the codex headers (regression).
        for other in PlatformId::ALL {
            if other != PlatformId::OpenaiCodex {
                assert!(
                    !other.sends_codex_responses_headers(),
                    "{} must not send the codex responses headers",
                    other.as_str()
                );
            }
        }
        let cfg = c.oauth().expect("openai-codex carries a PKCE OAuthConfig");
        assert_eq!(cfg, &CODEX_OAUTH_CONFIG);
        assert_eq!(cfg.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(cfg.auth_host, "https://auth.openai.com");
        assert_eq!(cfg.token_host, "https://auth.openai.com");
        assert_eq!(cfg.device_path, "/oauth/authorize");
        assert_eq!(cfg.token_path, "/oauth/token");
        assert_eq!(cfg.scope, "openid profile email offline_access");
        assert_eq!(cfg.scope_key, "oauth/openai-codex");
        assert_eq!(
            cfg.flow,
            OAuthFlow::PkceLocalhost {
                redirect_port: 1455,
                redirect_path: "/auth/callback",
            }
        );
        // FORM token body (refresh routes through the generic device refresher).
        assert_eq!(cfg.token_body, OAuthTokenBody::Form);
        assert_eq!(cfg.copilot_exchange, None);
        // The 3 authorize-only extra params; claude/xai/copilot carry none.
        assert_eq!(
            cfg.authorize_extra,
            &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "codex_cli_rs"),
            ]
        );
        assert_eq!(CLAUDE_OAUTH_CONFIG.authorize_extra, &[] as &[(&str, &str)]);
        assert_eq!(XAI_OAUTH_CONFIG.authorize_extra, &[] as &[(&str, &str)]);
        assert_eq!(COPILOT_OAUTH_CONFIG.authorize_extra, &[] as &[(&str, &str)]);
        // The chatgpt_account_id requirement is an EXPLICIT per-provider fact,
        // never inferred from the token-body encoding — a future form-encoded
        // PKCE provider must not inherit ChatGPT's account-id gate. Enforced at
        // COMPILE time: adding a provider that flips this fails the build.
        const {
            assert!(CODEX_OAUTH_CONFIG.requires_chatgpt_account_id);
            assert!(!CLAUDE_OAUTH_CONFIG.requires_chatgpt_account_id);
            assert!(!XAI_OAUTH_CONFIG.requires_chatgpt_account_id);
            assert!(!COPILOT_OAUTH_CONFIG.requires_chatgpt_account_id);
        }
        assert_eq!(
            oauth_config_for_scope_key("oauth/openai-codex"),
            Some(&CODEX_OAUTH_CONFIG)
        );
        // Responses wire, Bearer, hardcoded (no models.dev id), own base.
        assert_eq!(c.models_dev_id(), None);
        assert_eq!(c.wire_api(), PlatformWireApi::Responses);
        assert_eq!(c.key_header(), PlatformKeyHeader::Bearer);
        assert_eq!(c.api_key_env_names(), &[] as &[&str]);
        assert_eq!(c.managed_model_key("gpt-5.5"), "openai-codex/gpt-5.5");
        assert_eq!(
            c.base_url(),
            "https://chatgpt.com/backend-api/codex",
            "default codex base carries the /codex tail (→ /codex/responses)"
        );
        let _guard = kigi_env::EnvVarGuard::set(CODEX_BASE_URL_ENV, "https://mock.codex/codex");
        assert_eq!(c.base_url(), "https://mock.codex/codex");
    }

    /// The HARDCODED openai-codex catalog is exactly the 4 supported+listed
    /// models, keyed by slug, ctx 272000, each exposing its exact supported
    /// efforts (incl. the codex-only `xhigh`/`max`/`ultra` tiers). The
    /// list-but-broken / hidden models are absent. Every other platform serves
    /// NO hardcoded catalog (its models come from the live wire).
    #[test]
    fn openai_codex_hardcoded_catalog_is_the_four_supported_models() {
        let catalog = PlatformId::OpenaiCodex
            .hardcoded_catalog()
            .expect("openai-codex serves a hardcoded catalog");
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"],
            "exactly the 4 visibility=list AND supported_in_api=true models"
        );
        // Excluded: list-but-not-served + hidden models never appear.
        for absent in [
            "gpt-5.3-codex-spark",
            "gpt-5.4",
            "gpt-5.4-mini",
            "codex-auto-review",
        ] {
            assert!(
                !ids.contains(&absent),
                "{absent} must be excluded from the hardcoded catalog"
            );
        }
        for m in &catalog {
            assert_eq!(m.context_length, 272_000, "{} ctx", m.id);
            assert!(m.supports_reasoning && m.supports_image_in, "{} caps", m.id);
            let caps = m.capabilities();
            assert!(caps.contains(&ModelCapability::Thinking));
            assert!(caps.contains(&ModelCapability::ImageIn));
        }
        // Per-model efforts (the crux of "their thinking method").
        let efforts = |slug: &str| -> Vec<String> {
            catalog
                .iter()
                .find(|m| m.id == slug)
                .unwrap()
                .think_efforts
                .as_ref()
                .unwrap()
                .valid_efforts
                .clone()
        };
        assert_eq!(
            efforts("gpt-5.6-sol"),
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(
            efforts("gpt-5.6-terra"),
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(
            efforts("gpt-5.6-luna"),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(efforts("gpt-5.5"), ["low", "medium", "high", "xhigh"]);
        // Default efforts per the model table.
        let default = |slug: &str| -> Option<String> {
            catalog
                .iter()
                .find(|m| m.id == slug)
                .unwrap()
                .think_efforts
                .as_ref()
                .unwrap()
                .default_effort
                .clone()
        };
        assert_eq!(default("gpt-5.6-sol").as_deref(), Some("low"));
        assert_eq!(default("gpt-5.6-terra").as_deref(), Some("medium"));
        // Only openai-codex has a hardcoded catalog; every wire platform is None.
        for p in PlatformId::ALL {
            if p != PlatformId::OpenaiCodex {
                assert!(
                    p.hardcoded_catalog().is_none(),
                    "{} must not carry a hardcoded catalog",
                    p.as_str()
                );
            }
        }
    }

    /// The Copilot `/models` filter keeps ONLY openai-completions-served,
    /// selectable, tool-calling models — dropping the claude-4.x/5.x (messages)
    /// and gpt-5/oswe/mai- (responses-only) ids AND the disabled / picker-off /
    /// tool-call-declining ones — with the display name carried through.
    #[test]
    fn copilot_listing_filters_by_availability_and_wire_compat() {
        let body = serde_json::json!({ "data": [
            // KEPT: completions-served, picker on, no policy, tool_calls absent.
            { "id": "gpt-4.1", "name": "GPT-4.1", "model_picker_enabled": true },
            // KEPT: policy enabled + tool_calls true.
            { "id": "gemini-3-flash-preview", "model_picker_enabled": true,
              "policy": {"state": "enabled"},
              "capabilities": {"supports": {"tool_calls": true}} },
            // KEPT: claude-fable-5 is NOT a messages-routed claude family.
            { "id": "claude-fable-5", "model_picker_enabled": true },
            // DROPPED: claude-4.x → anthropic-messages wire.
            { "id": "claude-opus-4-8", "model_picker_enabled": true },
            // DROPPED: claude-5.x → anthropic-messages wire.
            { "id": "claude-sonnet-5", "model_picker_enabled": true },
            // DROPPED: gpt-5* → responses-only.
            { "id": "gpt-5.2", "model_picker_enabled": true },
            // DROPPED: mai-* → responses-only.
            { "id": "mai-code-1", "model_picker_enabled": true },
            // DROPPED: picker disabled.
            { "id": "gpt-4o", "model_picker_enabled": false },
            // DROPPED: policy disabled.
            { "id": "kimi-k2.7-code", "model_picker_enabled": true,
              "policy": {"state": "disabled"} },
            // DROPPED: tool_calls explicitly false.
            { "id": "text-embed", "model_picker_enabled": true,
              "capabilities": {"supports": {"tool_calls": false}} }
        ]})
        .to_string();
        let kept = parse_github_copilot_listing(&body).expect("valid listing");
        let ids: Vec<&str> = kept.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["gpt-4.1", "gemini-3-flash-preview", "claude-fable-5"],
            "only completions-served, selectable, tool-calling models survive"
        );
        assert_eq!(
            kept[0].display_name.as_deref(),
            Some("GPT-4.1"),
            "the wire display name must carry through"
        );
        // A 200 body without `data` is a contract violation (never empty catalog).
        assert!(parse_github_copilot_listing("{}").is_err());
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
                PlatformId::Xai => 15,
                PlatformId::QwenTokenPlan => 16,
                PlatformId::QwenTokenPlanCn => 17,
                PlatformId::KimiCoding => 18,
                PlatformId::Zai => 19,
                PlatformId::ZaiCodingCn => 20,
                PlatformId::Xiaomi => 21,
                PlatformId::XiaomiTokenPlanCn => 22,
                PlatformId::Minimax => 23,
                PlatformId::MinimaxCn => 24,
                PlatformId::XaiGrok => 25,
                PlatformId::ClaudeProMax => 26,
                PlatformId::GithubCopilot => 27,
                PlatformId::OpenaiCodex => 28,
            }
        }
        const VARIANT_COUNT: usize = 29; // update together with `ordinal`
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
