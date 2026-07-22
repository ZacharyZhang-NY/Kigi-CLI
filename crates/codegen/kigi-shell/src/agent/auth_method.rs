use agent_client_protocol as acp;

use crate::agent::config::ModelEntry;

/// Shared, live handle to the agent's current ACP auth method id.
///
/// `Arc` so a clone can cross the per-session-thread boundary at spawn; the
/// `ArcSwapOption` interior lets the agent's `authenticate` handler publish a
/// new method that every running session's per-turn auth gate observes on its
/// next turn -- no re-spawn. `None` until the first `authenticate`. Auth is
/// process-global (one user, one `AuthManager`), so all sessions sharing one
/// cell is correct.
pub(crate) type SharedAuthMethodId = std::sync::Arc<arc_swap::ArcSwapOption<acp::AuthMethodId>>;

/// Construct a [`SharedAuthMethodId`]. `None` is the pre-`authenticate` state.
pub(crate) fn new_shared_auth_method_id(initial: Option<acp::AuthMethodId>) -> SharedAuthMethodId {
    std::sync::Arc::new(arc_swap::ArcSwapOption::new(
        initial.map(std::sync::Arc::new),
    ))
}

/// Primary env var that, when set, advertises `xai.api_key` as a viable auth
/// method. NOTE: `xai.api_key` is the *house* bring-your-own-key method (the
/// upstream product is house-branded "xai"), unrelated to the x.ai/Grok
/// provider. That collision is why the primary env moved here to `KIGI_API_KEY`
/// — `XAI_API_KEY` is now the x.ai/Grok provider key (see `XAI_SPEC`).
///
/// Kept as a constant so test code and the production check stay in sync.
pub const HOUSE_API_KEY_ENV_VAR: &str = "KIGI_API_KEY";

/// Back-compat fallback env: `XAI_API_KEY` was the house BYOK key before it
/// became the x.ai/Grok provider key. Still honored so existing house-BYOK
/// deployments keep working (they share the key with the Grok provider).
pub const XAI_API_KEY_ENV_VAR: &str = "XAI_API_KEY";

/// Legacy env var name (pre-`XAI_API_KEY`). Checked last so the oldest
/// deployments keep working.
pub const LEGACY_XAI_API_KEY_ENV_VAR: &str = "KIGI_CODE_XAI_API_KEY";

/// Read the house BYOK API key from the environment.
///
/// Checks `KIGI_API_KEY` first, then the back-compat `XAI_API_KEY`, then the
/// legacy `KIGI_CODE_XAI_API_KEY`.
pub fn read_xai_api_key_env() -> Result<String, std::env::VarError> {
    std::env::var(HOUSE_API_KEY_ENV_VAR)
        .or_else(|_| std::env::var(XAI_API_KEY_ENV_VAR))
        .or_else(|_| std::env::var(LEGACY_XAI_API_KEY_ENV_VAR))
}

/// Returns `true` if any house BYOK env is set: `KIGI_API_KEY` (primary) or the
/// back-compat `XAI_API_KEY` / `KIGI_CODE_XAI_API_KEY`.
pub fn has_xai_api_key_env() -> bool {
    read_xai_api_key_env().is_ok()
}

/// Whether `xai.api_key` should be advertised (and pushed FIRST) when building
/// the `auth_methods` list at `initialize()` time.
///
/// Regression: `xai.api_key` must stay first when only per-model credentials
/// exist (no global `XAI_API_KEY`). Deferring it made BYOK users hit the login
/// screen because the pager uses `auth_methods.first()` for startup metadata.
///
/// [`build_auth_methods`] consumes this predicate and pins the ordering;
/// its tests catch call-site and predicate regressions.
///
/// Probes `std::env` at call time and consults each `ModelEntry` for a
/// resolvable api_key/env_key -- both inputs can change between calls, so the
/// result is not cached.
pub fn should_advertise_xai_api_key<'a, I>(models: I) -> bool
where
    I: IntoIterator<Item = &'a ModelEntry>,
{
    has_xai_api_key_env() || models.into_iter().any(ModelEntry::has_own_credentials)
}

/// Inputs to [`build_auth_methods`].
///
/// Booleans are computed by the caller (`MvpAgent::initialize()`) because they
/// depend on async side effects (token refresh) and shared mutable state
/// (`AuthManager`). The list-construction logic itself is pure so it can be
/// unit-tested without any of that machinery.
pub struct AuthMethodsBuildInputs<'a> {
    /// True if `xai.api_key` should be advertised AT ALL. Caller computes via
    /// [`should_advertise_xai_api_key`].
    pub has_external_api_key: bool,
    /// True if a cached session token is available (either present at startup
    /// or recovered via silent refresh).
    pub has_cached_token: bool,
    /// Optional display label for the interactive login method.
    pub login_label: Option<&'a str>,
}

/// Output of [`build_auth_methods`].
pub struct BuiltAuthMethods {
    /// Auth methods in advertised order. ORDER IS THE CONTRACT: the pager's
    /// `startup_auth_metadata()` reads `methods.first()` to decide whether
    /// interactive login is needed.
    pub methods: Vec<acp::AuthMethod>,
    /// The default `auth_method_id` to install on the agent. `cached_token`
    /// wins over `xai.api_key` when both are present; `None` means an
    /// interactive login is required.
    pub default_auth_method_id: Option<acp::AuthMethodId>,
}

/// Build the `auth_methods` list and default `auth_method_id` from
/// pre-computed inputs.
///
/// REGRESSION GUARD: when `has_external_api_key` is true, the **first** entry
/// MUST be `xai.api_key`. A prior change deferred it to the END for per-model
/// credentials, which made the pager send per-model-key users to the login
/// screen. Unit tests lock this.
///
/// Ordering (when each method is enabled):
/// 1. `xai.api_key`     (if `has_external_api_key`)
/// 2. `cached_token`    (if `has_cached_token`)
/// 3. `kimi-code`        (the Kimi Code device login)
/// 4. every generic device-code OAuth login (`xai-grok`, …), in
///    `PlatformId::ALL` order — interactive logins after `kimi-code`
/// 5. every API-key registry platform, in `PlatformId::ALL` order
///    (`moonshot-cn`, `moonshot-ai`, …), always advertised
///
/// The platform methods are for the INTERACTIVE login picker only: they come
/// after `kimi-code` so they can never become `auth_methods.first()` (the
/// pager's startup metadata / eager-auth fallback reads `first()`), and they
/// are never the `default_auth_method_id` (a configured platform key already
/// authenticates eagerly via `xai.api_key` — the catalog entries it stamps
/// satisfy `should_advertise_xai_api_key`).
///
/// `default_auth_method_id`:
/// - `cached_token` if `has_cached_token`
/// - `xai.api_key`  else if `has_external_api_key`
/// - `None`         otherwise
pub fn build_auth_methods(inputs: AuthMethodsBuildInputs<'_>) -> BuiltAuthMethods {
    let AuthMethodsBuildInputs {
        has_external_api_key,
        has_cached_token,
        login_label,
    } = inputs;

    let mut methods: Vec<acp::AuthMethod> = Vec::new();
    let mut default_auth_method_id: Option<acp::AuthMethodId> = None;

    if has_external_api_key {
        methods.push(xai_api_key_auth_method());
        default_auth_method_id = Some(acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID));
    }

    if has_cached_token {
        methods.push(cached_token_auth_method());
        // cached_token wins over xai.api_key for default_auth_method_id so
        // is_session_based_auth() returns true and OAuth refresh stays alive.
        let overrode_api_key = default_auth_method_id.is_some();
        default_auth_method_id = Some(acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID));
        if overrode_api_key {
            kigi_log::unified_log::info(
                "auth method priority: cached_token overrides xai.api_key for default_auth_method_id",
                None,
                Some(serde_json::json!({
                    "has_external_api_key": has_external_api_key,
                    "has_cached_token": has_cached_token,
                })),
            );
        }
    }

    methods.push(kimi_code_auth_method(login_label));
    // Generic device-code OAuth logins (xai-grok, …) are interactive logins
    // too: advertise them right after kimi-code, BEFORE the API-key platforms,
    // so they stay out of the `auth_methods.first()` startup-metadata slot yet
    // ahead of the api-key picker rows.
    for platform in kigi_models::PlatformId::ALL {
        if platform.oauth().is_some() {
            methods.push(oauth_platform_auth_method(platform));
        }
    }
    for platform in kigi_models::PlatformId::ALL {
        if !platform.uses_oauth() {
            methods.push(platform_auth_method(platform));
        }
    }

    BuiltAuthMethods {
        methods,
        default_auth_method_id,
    }
}

/// ACP session auth method. Use `is_session_based_method` for classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethodKind {
    XaiApiKey,
    CachedToken,
    KimiCode,
    /// Registry API-key platform login (method id = the platform id).
    ApiKeyPlatform(kigi_models::PlatformId),
    /// Generic device-code OAuth platform login (method id = the platform id,
    /// e.g. `xai-grok`). Interactive, like Kimi Code.
    OAuthPlatform(kigi_models::PlatformId),
    Unknown,
}

impl AuthMethodKind {
    pub fn from_id(id: &acp::AuthMethodId) -> Self {
        match id.0.as_ref() {
            XAI_API_KEY_METHOD_ID => Self::XaiApiKey,
            CACHED_TOKEN_AUTH_METHOD_ID => Self::CachedToken,
            KIMI_CODE_METHOD_ID => Self::KimiCode,
            other => match kigi_models::PlatformId::parse(other) {
                // A generic device-code OAuth platform (xai-grok).
                Some(p) if p.oauth().is_some() => Self::OAuthPlatform(p),
                // A non-OAuth API-key registry platform.
                Some(p) if !p.uses_oauth() => Self::ApiKeyPlatform(p),
                _ => Self::Unknown,
            },
        }
    }

    /// API key auth: no auth.json session, no refresh, no browser round-trip.
    /// The registry platform methods qualify — they validate a configured
    /// platform key and then behave exactly like an external-API-key session.
    pub fn is_api_key(self) -> bool {
        matches!(self, Self::XaiApiKey | Self::ApiKeyPlatform(_))
    }

    /// `true` for session-based methods (cached_token, interactive login).
    ///
    /// `OAuthPlatform` (xai-grok) qualifies: it mints a refreshable device-code
    /// session, so the per-turn refresh / 401-recovery gate must be ACTIVE for
    /// it. This is correct ONLY because the session routes a model's
    /// bearer/refresh/recovery to that model's OWN scope-keyed `AuthManager`
    /// (see `SessionActor::auth_manager_for_model`) — the gate being active
    /// wraps the grok manager for a grok turn, never the Kimi one.
    pub fn is_session_based(self) -> bool {
        matches!(
            self,
            Self::CachedToken | Self::KimiCode | Self::OAuthPlatform(_)
        )
    }

    /// Requires user interaction (device-code login in the browser). Both the
    /// Kimi Code login and every generic device-code OAuth platform qualify.
    pub fn needs_interactive_login(self) -> bool {
        matches!(self, Self::KimiCode | Self::OAuthPlatform(_))
    }

    /// The generic device-code OAuth platform behind this method, if any
    /// (drives the `authenticate` dispatch to the generic device flow).
    pub fn oauth_platform(self) -> Option<kigi_models::PlatformId> {
        match self {
            Self::OAuthPlatform(p) => Some(p),
            _ => None,
        }
    }

    pub fn auth_error_message(self) -> &'static str {
        if self.is_session_based() {
            AUTH_ERROR_SESSION_EXPIRED
        } else {
            AUTH_ERROR_API_KEY
        }
    }
}

/// `true` for session-based ACP methods (cached_token, interactive login).
pub fn is_session_based_method(method_id: &acp::AuthMethodId) -> bool {
    AuthMethodKind::from_id(method_id).is_session_based()
}

/// Per-model BYOK status: whether the selected model carries its own
/// `[model.*]` `api_key`/`env_key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelByok {
    /// Model has its own per-model key (not refreshable).
    Byok,
    /// Model has no per-model key (session auth governs).
    NotByok,
    /// Config couldn't be loaded/parsed — BYOK status indeterminate.
    Unknown,
}

impl ModelByok {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Byok => "byok",
            Self::NotByok => "not_byok",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether this session+model uses a refreshable session token.
///
/// Gates on stable inputs, not `Credentials.auth_type`: that field collapses
/// to `ApiKey` when the session-token cache is momentarily empty and
/// `XAI_API_KEY` is set, which demoted live sessions to non-refreshable
/// api-key mode and 401'd every prompt until restart. `model_byok` still
/// excludes genuine per-model BYOK, whose keys are not refreshable.
///
/// `Unknown` (BYOK status indeterminate — config currently unparseable, no
/// sampling config yet, or the per-model memo was cleared) must **not** demote
/// a live session to non-refreshable api-key mode: that re-sends the stale
/// buffered token on every turn and 401s with `bad-credentials` until restart.
/// It refreshes when `endpoint_is_first_party` — the request targets the
/// first-party API, where sending the session token cannot leak to a
/// third-party BYOK endpoint. A definite `NotByok` always refreshes (it only
/// ever routes to the session endpoint); a definite `Byok` never does.
pub fn session_token_auth_gate(
    is_session_based_method: bool,
    model_byok: ModelByok,
    endpoint_is_first_party: bool,
) -> bool {
    is_session_based_method
        && match model_byok {
            ModelByok::NotByok => true,
            ModelByok::Byok => false,
            ModelByok::Unknown => endpoint_is_first_party,
        }
}

pub const AUTH_ERROR_SESSION_EXPIRED: &str =
    "Session expired. Run `kigi login` to re-authenticate.";

pub const AUTH_ERROR_API_KEY: &str = "Authentication failed. Run `kigi login`, set KIGI_API_KEY, or add api_key to ~/.kigi/config.toml.";

/// Next ACP method id when `cached_token` cannot proceed (missing / expired):
/// prefer non-interactive `xai.api_key` when advertiseable, else the
/// interactive device login.
pub fn method_id_after_cached_token_unavailable(has_external_api_key: bool) -> &'static str {
    if has_external_api_key {
        XAI_API_KEY_METHOD_ID
    } else {
        KIMI_CODE_METHOD_ID
    }
}

pub const XAI_API_KEY_METHOD_ID: &str = "xai.api_key";
pub fn xai_api_key_auth_method() -> acp::AuthMethod {
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID),
            "xai.api_key".to_string(),
        )
        .description(Some(format!(
            "{HOUSE_API_KEY_ENV_VAR} or api_key/env_key in config.toml"
        ))),
    )
}

pub const CACHED_TOKEN_AUTH_METHOD_ID: &str = "cached_token";
pub fn cached_token_auth_method() -> acp::AuthMethod {
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID),
            "cached_token".to_string(),
        )
        .description(Some("Cached Kimi Code session".to_string())),
    )
}

/// Interactive login method id, advertised over ACP by this agent and
/// selected by the in-repo pager. Both sides of the ACP boundary live in
/// this repo, so the id is renamed in lockstep everywhere.
pub const KIMI_CODE_METHOD_ID: &str = "kimi-code";

/// The Kimi Code device-code login.
pub fn kimi_code_auth_method(label: Option<&str>) -> acp::AuthMethod {
    let name = label.unwrap_or("Kimi Code");
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(KIMI_CODE_METHOD_ID),
            name.to_string(),
        )
        .description(Some(format!("Sign in with {name}"))),
    )
}

/// A generic device-code OAuth platform's interactive login method (method id
/// = the platform id, e.g. `xai-grok`; label from the spec's `login_label`).
pub fn oauth_platform_auth_method(platform: kigi_models::PlatformId) -> acp::AuthMethod {
    let name = platform.login_label();
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(acp::AuthMethodId::new(platform.as_str()), name.to_string())
            .description(Some(format!("Sign in with {name}"))),
    )
}

/// Interactive API-key login method ids equal
/// [`kigi_models::PlatformId::as_str`] (`moonshot-cn` / `moonshot-ai` / …),
/// which is also the `[platforms.<id>]` config-table name and the auth.json
/// scope — one id everywhere.
pub const MOONSHOT_CN_METHOD_ID: &str = "moonshot-cn";
pub const MOONSHOT_AI_METHOD_ID: &str = "moonshot-ai";

/// The API-key registry platform behind an interactive method id. `None`
/// for every other id (including `kimi-code`, whose platform uses OAuth).
pub fn platform_for_method_id(id: &acp::AuthMethodId) -> Option<kigi_models::PlatformId> {
    platform_for_method_id_str(id.0.as_ref())
}

fn platform_for_method_id_str(id: &str) -> Option<kigi_models::PlatformId> {
    kigi_models::PlatformId::parse(id).filter(|p| !p.uses_oauth())
}

/// An API-key registry platform's login method (picker label + description
/// from the platform's spec row).
pub fn platform_auth_method(platform: kigi_models::PlatformId) -> acp::AuthMethod {
    let description = match platform.console_host() {
        Some(host) => format!("API key from {host}"),
        None => format!("API key for {}", platform.display_name()),
    };
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(platform.as_str()),
            platform.login_label().to_string(),
        )
        .description(Some(description)),
    )
}

/// Actionable error for a platform `authenticate` with no key configured.
pub fn missing_platform_key_error(platform: kigi_models::PlatformId) -> String {
    match platform.api_key_env_names().first() {
        Some(env_var) => format!(
            "No API key configured for {} \u{2014} paste one in the login screen or set {env_var}",
            platform.as_str(),
        ),
        None => format!(
            "No API key configured for {} \u{2014} paste one in the login screen",
            platform.as_str(),
        ),
    }
}

/// Validate + accept an API-key platform's key for `authenticate`.
///
/// `key` is the caller-resolved credential (env > auth.json > config; see
/// `resolve_platform_api_key`) — `None` fails with the actionable
/// missing-key message. A present key is validated with
/// `GET {platform_base}/models` (the same endpoint the catalog fetch uses):
/// 401 → "invalid API key"; any other non-success status or network error
/// surfaces as-is. SECURITY: the key is only ever sent as the platform's
/// key header (Bearer or x-api-key) — it must never appear in errors or
/// logs.
pub(crate) async fn authenticate_platform_api_key(
    platform: kigi_models::PlatformId,
    key: Option<&str>,
) -> Result<(), acp::Error> {
    let auth_err = |message: String| {
        let mut err = acp::Error::auth_required();
        err.message = message;
        err
    };
    let Some(key) = key else {
        return Err(auth_err(missing_platform_key_error(platform)));
    };
    let url = format!(
        "{}{}",
        platform.base_url().trim_end_matches('/'),
        platform.key_validation_path()
    );
    let request = match platform.key_header() {
        kigi_models::PlatformKeyHeader::Bearer => crate::http::shared_client()
            .get(&url)
            .header("Authorization", format!("Bearer {key}")),
        kigi_models::PlatformKeyHeader::XApiKey => crate::http::shared_client()
            .get(&url)
            .header("x-api-key", key)
            .header("anthropic-version", kigi_sampling_types::ANTHROPIC_VERSION),
    };
    let response = request
        .send()
        .await
        .map_err(|e| auth_err(format!("Couldn't reach {}: {e}", platform.as_str())))?;
    let status = response.status();
    if status.as_u16() == 401 {
        return Err(auth_err(format!(
            "Invalid API key for {} \u{2014} check your key on {}",
            platform.as_str(),
            platform.console_host().unwrap_or("the provider console"),
        )));
    }
    if !status.is_success() {
        return Err(auth_err(format!(
            "{} key validation failed: HTTP {}",
            platform.as_str(),
            status.as_u16(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{Config, resolve_model_list};
    use agent_client_protocol as acp;
    use serial_test::serial;

    /// When API-key credentials are advertiseable, fall through from a dead
    /// `cached_token` to non-interactive `xai.api_key` (not the browser).
    #[test]
    fn after_cached_token_unavailable_prefers_api_key_when_advertiseable() {
        assert_eq!(
            method_id_after_cached_token_unavailable(true),
            XAI_API_KEY_METHOD_ID,
        );
    }

    /// No advertiseable API-key credentials → interactive device login.
    #[test]
    fn after_cached_token_unavailable_falls_to_interactive_login() {
        assert_eq!(
            method_id_after_cached_token_unavailable(false),
            KIMI_CODE_METHOD_ID,
        );
    }

    /// Classifier matrix for all auth method variants.
    #[test]
    fn auth_method_kind_classifier_matrix() {
        let session_methods = [CACHED_TOKEN_AUTH_METHOD_ID, KIMI_CODE_METHOD_ID];
        for id in session_methods {
            let kind = AuthMethodKind::from_id(&acp::AuthMethodId::new(id));
            assert!(kind.is_session_based(), "{id} must be session-based");
            assert!(!kind.is_api_key(), "{id} must not be api-key");
        }
        let api = AuthMethodKind::from_id(&acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID));
        assert!(api.is_api_key());
        assert!(!api.is_session_based());
        assert!(!api.needs_interactive_login());
        // Registry platform methods are API-key shaped: NOT session-based (no
        // token refresh may ever run for them) and no browser round-trip.
        for id in [MOONSHOT_CN_METHOD_ID, MOONSHOT_AI_METHOD_ID] {
            let kind = AuthMethodKind::from_id(&acp::AuthMethodId::new(id));
            assert!(
                matches!(kind, AuthMethodKind::ApiKeyPlatform(p) if p.as_str() == id),
                "{id} must classify as its ApiKeyPlatform"
            );
            assert!(kind.is_api_key(), "{id} must classify as api-key");
            assert!(!kind.is_session_based(), "{id} must not be session-based");
            assert!(
                !is_session_based_method(&acp::AuthMethodId::new(id)),
                "is_session_based_method({id}) must stay false"
            );
            assert!(
                !kind.needs_interactive_login(),
                "{id} must not need a browser login"
            );
        }
        let unknown = AuthMethodKind::from_id(&acp::AuthMethodId::new("who-knows"));
        assert_eq!(unknown, AuthMethodKind::Unknown);
        assert!(!unknown.is_session_based());
        // Only the interactive login needs a browser.
        assert!(
            AuthMethodKind::from_id(&acp::AuthMethodId::new(KIMI_CODE_METHOD_ID))
                .needs_interactive_login()
        );
        assert!(
            !AuthMethodKind::from_id(&acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID))
                .needs_interactive_login()
        );
    }

    /// xai-grok is a generic device-code OAuth login: it classifies as an
    /// `OAuthPlatform`, needs an interactive (browser) login, is NOT api-key,
    /// is NOT an API-key registry platform, and is advertised right after
    /// kimi-code among the interactive logins.
    #[test]
    fn xai_grok_is_an_interactive_oauth_login() {
        let id = acp::AuthMethodId::new("xai-grok");
        let kind = AuthMethodKind::from_id(&id);
        assert_eq!(
            kind,
            AuthMethodKind::OAuthPlatform(kigi_models::PlatformId::XaiGrok)
        );
        assert!(
            kind.needs_interactive_login(),
            "xai-grok needs a browser login"
        );
        assert!(!kind.is_api_key(), "xai-grok is not an api-key method");
        assert_eq!(
            kind.oauth_platform(),
            Some(kigi_models::PlatformId::XaiGrok)
        );
        // Never an API-key picker target (keeps it out of the paste-box path).
        assert_eq!(platform_for_method_id(&id), None);
        // Placement: immediately after kimi-code, before the api-key rows.
        let built = build_auth_methods(default_inputs());
        let ids = method_ids(&built);
        let kimi_pos = ids.iter().position(|m| *m == KIMI_CODE_METHOD_ID).unwrap();
        assert_eq!(
            ids[kimi_pos + 1],
            "xai-grok",
            "xai-grok must be the interactive login right after kimi-code"
        );
        assert_eq!(
            ids[kimi_pos + 2],
            "claude-pro-max",
            "claude-pro-max is the next interactive OAuth login, after xai-grok"
        );
        assert_eq!(
            ids[kimi_pos + 3],
            "github-copilot",
            "github-copilot is the next interactive OAuth login, after claude-pro-max"
        );
        assert_eq!(
            ids[kimi_pos + 4],
            "openai-codex",
            "openai-codex is the next interactive OAuth login, after github-copilot"
        );
        assert_eq!(
            ids[kimi_pos + 5],
            MOONSHOT_CN_METHOD_ID,
            "the api-key rows follow the generic oauth logins"
        );
    }

    /// A generic device-code OAuth platform (xai-grok) is SESSION-BASED: its
    /// device-code session is refreshable, so the per-turn refresh / 401 gate
    /// must be active for it (routing then sends the model's OWN manager). It
    /// stays an interactive login and is NOT api-key-shaped.
    #[test]
    fn oauth_platform_is_session_based_and_refreshable() {
        let id = acp::AuthMethodId::new("xai-grok");
        let kind = AuthMethodKind::from_id(&id);
        assert_eq!(
            kind,
            AuthMethodKind::OAuthPlatform(kigi_models::PlatformId::XaiGrok)
        );
        assert!(
            kind.is_session_based(),
            "OAuthPlatform must be session-based so the refresh/401 gate is active"
        );
        assert!(
            is_session_based_method(&id),
            "is_session_based_method(xai-grok) must be true"
        );
        // Session-based, yet still an interactive browser login and never
        // api-key-shaped.
        assert!(kind.needs_interactive_login());
        assert!(!kind.is_api_key());
        // The session-expired copy (not the api-key copy) is the right error.
        assert_eq!(kind.auth_error_message(), AUTH_ERROR_SESSION_EXPIRED);
    }

    /// claude-pro-max classifies as an interactive OAuth login too (the
    /// authenticate handler dispatches it to the PKCE-localhost flow by the
    /// config's `flow`): session-based, needs a browser, never api-key, and
    /// `oauth_platform()` returns ClaudeProMax.
    #[test]
    fn claude_pro_max_is_an_interactive_oauth_login() {
        let id = acp::AuthMethodId::new("claude-pro-max");
        let kind = AuthMethodKind::from_id(&id);
        assert_eq!(
            kind,
            AuthMethodKind::OAuthPlatform(kigi_models::PlatformId::ClaudeProMax)
        );
        assert!(kind.needs_interactive_login());
        assert!(kind.is_session_based());
        assert!(!kind.is_api_key());
        assert_eq!(
            kind.oauth_platform(),
            Some(kigi_models::PlatformId::ClaudeProMax)
        );
        // Never an API-key picker target (keeps it out of the paste-box path).
        assert_eq!(platform_for_method_id(&id), None);
    }

    /// The OAuth platform id must never resolve as an API-key platform
    /// method — `platform_for_method_id`'s `uses_oauth` filter is what keeps
    /// the generic `authenticate` arm from hijacking the device login.
    #[test]
    fn oauth_platform_id_is_not_an_api_key_method() {
        assert_eq!(
            platform_for_method_id(&acp::AuthMethodId::new(KIMI_CODE_METHOD_ID)),
            None
        );
        assert_eq!(
            AuthMethodKind::from_id(&acp::AuthMethodId::new(KIMI_CODE_METHOD_ID)),
            AuthMethodKind::KimiCode
        );
    }

    #[test]
    fn session_token_auth_gate_matrix() {
        // Session method + NotByok → refresh.
        assert!(session_token_auth_gate(true, ModelByok::NotByok, false));
        // Session method + Byok → never.
        assert!(!session_token_auth_gate(true, ModelByok::Byok, true));
        // Session method + Unknown → only on first-party endpoints.
        assert!(session_token_auth_gate(true, ModelByok::Unknown, true));
        assert!(!session_token_auth_gate(true, ModelByok::Unknown, false));
        // Non-session method → never.
        assert!(!session_token_auth_gate(false, ModelByok::NotByok, true));
    }

    /// RAII guard restoring an env var on drop (panic-safe).
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn default_inputs() -> AuthMethodsBuildInputs<'static> {
        AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: false,
            login_label: None,
        }
    }

    fn method_ids(built: &BuiltAuthMethods) -> Vec<&str> {
        built.methods.iter().map(|m| m.id().0.as_ref()).collect()
    }

    fn default_id(built: &BuiltAuthMethods) -> Option<&str> {
        built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.as_ref())
    }

    fn first_kind(methods: &[acp::AuthMethod]) -> Option<AuthMethodKind> {
        methods.first().map(|m| AuthMethodKind::from_id(m.id()))
    }

    /// BYOK: `xai.api_key` must be `auth_methods.first()`; deferred-to-last
    /// ordering sends per-model-key users to the login screen.
    #[test]
    fn byok_first_method_is_xai_api_key() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            ..default_inputs()
        });
        assert_eq!(
            method_ids(&built),
            vec![
                XAI_API_KEY_METHOD_ID,
                KIMI_CODE_METHOD_ID,
                "xai-grok",
                "claude-pro-max",
                "github-copilot",
                "openai-codex",
                MOONSHOT_CN_METHOD_ID,
                MOONSHOT_AI_METHOD_ID,
                "openai",
                "anthropic",
                "deepseek",
                "groq",
                "mistral",
                "fireworks",
                "google",
                "openrouter",
                "together",
                "cerebras",
                "nvidia",
                "vercel-ai-gateway",
                "xai",
                "qwen-token-plan",
                "qwen-token-plan-cn",
                "kimi-coding",
                "zai",
                "zai-coding-cn",
                "xiaomi",
                "xiaomi-token-plan-cn",
                "minimax",
                "minimax-cn"
            ]
        );
        assert_eq!(default_id(&built), Some(XAI_API_KEY_METHOD_ID));
        assert!(
            !AuthMethodKind::from_id(built.methods[0].id()).needs_interactive_login(),
            "auth_methods.first() must not need interactive login"
        );
    }

    /// API key + cached session: `xai.api_key` stays first in the advertised
    /// list, but the session wins the default (refresh stays alive).
    #[test]
    fn byok_with_cached_token_keeps_xai_api_key_first() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: true,
            ..default_inputs()
        });
        assert_eq!(
            method_ids(&built),
            vec![
                XAI_API_KEY_METHOD_ID,
                CACHED_TOKEN_AUTH_METHOD_ID,
                KIMI_CODE_METHOD_ID,
                "xai-grok",
                "claude-pro-max",
                "github-copilot",
                "openai-codex",
                MOONSHOT_CN_METHOD_ID,
                MOONSHOT_AI_METHOD_ID,
                "openai",
                "anthropic",
                "deepseek",
                "groq",
                "mistral",
                "fireworks",
                "google",
                "openrouter",
                "together",
                "cerebras",
                "nvidia",
                "vercel-ai-gateway",
                "xai",
                "qwen-token-plan",
                "qwen-token-plan-cn",
                "kimi-coding",
                "zai",
                "zai-coding-cn",
                "xiaomi",
                "xiaomi-token-plan-cn",
                "minimax",
                "minimax-cn"
            ]
        );
        assert_eq!(default_id(&built), Some(CACHED_TOKEN_AUTH_METHOD_ID));
    }

    /// Session-only user: cached_token first, interactive logins after it.
    #[test]
    fn session_only_user_first_method_is_cached_token() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_cached_token: true,
            ..default_inputs()
        });
        assert_eq!(
            method_ids(&built),
            vec![
                CACHED_TOKEN_AUTH_METHOD_ID,
                KIMI_CODE_METHOD_ID,
                "xai-grok",
                "claude-pro-max",
                "github-copilot",
                "openai-codex",
                MOONSHOT_CN_METHOD_ID,
                MOONSHOT_AI_METHOD_ID,
                "openai",
                "anthropic",
                "deepseek",
                "groq",
                "mistral",
                "fireworks",
                "google",
                "openrouter",
                "together",
                "cerebras",
                "nvidia",
                "vercel-ai-gateway",
                "xai",
                "qwen-token-plan",
                "qwen-token-plan-cn",
                "kimi-coding",
                "zai",
                "zai-coding-cn",
                "xiaomi",
                "xiaomi-token-plan-cn",
                "minimax",
                "minimax-cn"
            ]
        );
        assert_eq!(default_id(&built), Some(CACHED_TOKEN_AUTH_METHOD_ID));
        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::CachedToken)
        );
    }

    /// Fresh user: the interactive picker methods are advertised — the OAuth
    /// device login FIRST (`auth_methods.first()` drives the login screen),
    /// then the two Moonshot API-key logins. No default method (login
    /// required).
    #[test]
    fn fresh_user_advertises_picker_methods_kimi_code_first() {
        let built = build_auth_methods(default_inputs());
        assert_eq!(
            method_ids(&built),
            vec![
                KIMI_CODE_METHOD_ID,
                "xai-grok",
                "claude-pro-max",
                "github-copilot",
                "openai-codex",
                MOONSHOT_CN_METHOD_ID,
                MOONSHOT_AI_METHOD_ID,
                "openai",
                "anthropic",
                "deepseek",
                "groq",
                "mistral",
                "fireworks",
                "google",
                "openrouter",
                "together",
                "cerebras",
                "nvidia",
                "vercel-ai-gateway",
                "xai",
                "qwen-token-plan",
                "qwen-token-plan-cn",
                "kimi-coding",
                "zai",
                "zai-coding-cn",
                "xiaomi",
                "xiaomi-token-plan-cn",
                "minimax",
                "minimax-cn"
            ]
        );
        assert_eq!(default_id(&built), None);
        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::KimiCode));
    }

    /// The moonshot methods must never be the default (eager) method: the
    /// pager authenticates `default_auth_method_id` without user interaction,
    /// and a configured moonshot key already rides the `xai.api_key` path.
    #[test]
    fn moonshot_methods_are_never_the_default() {
        for (api, cached) in [(false, false), (true, false), (false, true), (true, true)] {
            let built = build_auth_methods(AuthMethodsBuildInputs {
                has_external_api_key: api,
                has_cached_token: cached,
                ..default_inputs()
            });
            assert!(
                !matches!(
                    default_id(&built),
                    Some(MOONSHOT_CN_METHOD_ID) | Some(MOONSHOT_AI_METHOD_ID)
                ),
                "default must not be a moonshot method (api={api}, cached={cached})"
            );
        }
    }

    /// `XAI_API_KEY` alone (no per-model creds) triggers advertising
    /// `xai.api_key` as the first method.
    #[test]
    #[serial]
    fn global_external_api_key_advertises_xai_api_key_first() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-external-key");
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None, &Default::default());
        let has_external_api_key = should_advertise_xai_api_key(models.values());
        assert!(has_external_api_key);
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key,
            ..default_inputs()
        });
        assert_eq!(first_kind(&built.methods), Some(AuthMethodKind::XaiApiKey));
    }

    /// Legacy env var fallback keeps working.
    #[test]
    #[serial]
    fn legacy_env_var_fallback_advertises_xai_api_key() {
        let _house = EnvGuard::unset(HOUSE_API_KEY_ENV_VAR);
        let _unset = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
        let _set = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-key");
        assert!(has_xai_api_key_env());
        assert_eq!(read_xai_api_key_env().unwrap(), "legacy-key");
    }

    /// `XAI_API_KEY` takes precedence over the older legacy env var.
    #[test]
    #[serial]
    fn xai_env_var_takes_precedence_over_legacy() {
        let _house = EnvGuard::unset(HOUSE_API_KEY_ENV_VAR);
        let _new = EnvGuard::set(XAI_API_KEY_ENV_VAR, "new-key");
        let _legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-key");
        assert_eq!(read_xai_api_key_env().unwrap(), "new-key");
    }

    /// After the migration, `KIGI_API_KEY` is the house BYOK primary and wins
    /// over the back-compat `XAI_API_KEY` (now the x.ai/Grok provider key).
    #[test]
    #[serial]
    fn house_env_var_takes_precedence_over_xai() {
        let _house = EnvGuard::set(HOUSE_API_KEY_ENV_VAR, "house-key");
        let _xai = EnvGuard::set(XAI_API_KEY_ENV_VAR, "grok-key");
        let _legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-key");
        assert_eq!(read_xai_api_key_env().unwrap(), "house-key");
    }

    /// Moonshot authenticate with no configured key: actionable error naming
    /// the platform, the login screen, and the platform-scoped env var. No
    /// HTTP is attempted (`key: None` short-circuits).
    #[tokio::test]
    async fn moonshot_authenticate_without_key_is_actionable() {
        let err = authenticate_platform_api_key(kigi_models::PlatformId::MoonshotCn, None)
            .await
            .expect_err("missing key must fail");
        assert_eq!(
            err.message,
            "No API key configured for moonshot-cn \u{2014} paste one in the login screen \
             or set KIGI_MOONSHOT_CN_API_KEY"
        );
    }

    /// Moonshot authenticate validates the key against `GET {base}/models`;
    /// a 200 accepts the key.
    #[tokio::test]
    #[serial]
    async fn moonshot_authenticate_valid_key_succeeds() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer sk-good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::MOONSHOT_CN_BASE_URL_ENV, &server.uri());
        authenticate_platform_api_key(kigi_models::PlatformId::MoonshotCn, Some("sk-good"))
            .await
            .expect("200 from /models must validate the key");
    }

    /// A 401 from `/models` is an invalid key — the error names the platform
    /// and console, and NEVER contains the key itself.
    #[tokio::test]
    #[serial]
    async fn moonshot_authenticate_401_is_invalid_key_error() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::MOONSHOT_AI_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(
            kigi_models::PlatformId::MoonshotAi,
            Some("sk-bad-secret"),
        )
        .await
        .expect_err("401 must fail");
        assert_eq!(
            err.message,
            "Invalid API key for moonshot-ai \u{2014} check your key on platform.moonshot.ai"
        );
        assert!(
            !err.message.contains("sk-bad-secret"),
            "the key must never leak into errors"
        );
    }

    /// OpenRouter's `/models` is PUBLIC (200 for any key), so validation must
    /// hit its auth-requiring `/key` endpoint instead — otherwise a bad key
    /// false-accepts at login. The mock serves `/models` 200 always; a bad
    /// key must still be rejected (proving `/models` is NOT what's validated).
    #[tokio::test]
    #[serial]
    async fn openrouter_validates_against_key_endpoint_not_public_models() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        // Public listing: 200 for anyone. If validation used this, a bad key
        // would pass.
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] })),
            )
            .mount(&server)
            .await;
        // Auth-required key endpoint: 401 for a bad key.
        wiremock::Mock::given(method("GET"))
            .and(path("/key"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::OPENROUTER_BASE_URL_ENV, &server.uri());
        let err =
            authenticate_platform_api_key(kigi_models::PlatformId::OpenRouter, Some("sk-or-bad"))
                .await
                .expect_err("a bad key must be rejected via /key, not accepted via /models");
        assert_eq!(
            err.message,
            "Invalid API key for openrouter \u{2014} check your key on openrouter.ai"
        );
    }

    /// Vercel's `/models` is public too; validation must hit `/credits`
    /// (401s for a bad key). A bad key is rejected even though `/models`
    /// would 200.
    #[tokio::test]
    #[serial]
    async fn vercel_validates_against_credits_endpoint_not_public_models() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/credits"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::VERCEL_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(kigi_models::PlatformId::Vercel, Some("vg-bad"))
            .await
            .expect_err("a bad key must be rejected via /credits, not accepted via /models");
        assert_eq!(
            err.message,
            "Invalid API key for vercel-ai-gateway \u{2014} check your key on vercel.com"
        );
    }

    /// A valid OpenRouter key: `/key` returns 200 → accepted.
    #[tokio::test]
    #[serial]
    async fn openrouter_valid_key_succeeds_via_key_endpoint() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/key"))
            .and(header("Authorization", "Bearer sk-or-good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": { "label": "k" } })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::OPENROUTER_BASE_URL_ENV, &server.uri());
        authenticate_platform_api_key(kigi_models::PlatformId::OpenRouter, Some("sk-or-good"))
            .await
            .expect("200 from /key must validate the key");
    }

    /// xAI has no validation-path override: `/v1/models` itself requires auth
    /// (401 without a valid key), so it doubles as the validator. A bad key is
    /// rejected.
    #[tokio::test]
    #[serial]
    async fn xai_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::XAI_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(kigi_models::PlatformId::Xai, Some("xai-bad"))
            .await
            .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for xai \u{2014} check your key on console.x.ai"
        );
    }

    /// A valid xAI key: `/models` returns 200 for the Bearer header → accepted.
    #[tokio::test]
    #[serial]
    async fn xai_valid_key_succeeds_via_models() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer xai-good"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::XAI_BASE_URL_ENV, &server.uri());
        authenticate_platform_api_key(kigi_models::PlatformId::Xai, Some("xai-good"))
            .await
            .expect("200 from /models must validate the key");
    }

    /// Qwen Token Plan (global): DashScope /models is auth-gated (401 for a bad
    /// key), so it validates the key; the error names the Model Studio console.
    #[tokio::test]
    #[serial]
    async fn qwen_token_plan_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::QWEN_TOKEN_PLAN_BASE_URL_ENV, &server.uri());
        let err =
            authenticate_platform_api_key(kigi_models::PlatformId::QwenTokenPlan, Some("qtp-bad"))
                .await
                .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for qwen-token-plan \u{2014} check your key on \
             modelstudio.console.alibabacloud.com"
        );
    }

    /// Qwen Token Plan (China): distinct base URL + console host; same
    /// auth-gated /models validation.
    #[tokio::test]
    #[serial]
    async fn qwen_token_plan_cn_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::QWEN_TOKEN_PLAN_CN_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(
            kigi_models::PlatformId::QwenTokenPlanCn,
            Some("qtp-cn-bad"),
        )
        .await
        .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for qwen-token-plan-cn \u{2014} check your key on \
             bailian.console.aliyun.com"
        );
    }

    /// Kimi-For-Coding static key: /coding/v1/models requires auth (401 for a
    /// bad key), so it validates the key. Base resolves via KIGI_CODE_BASE_URL.
    #[tokio::test]
    #[serial]
    async fn kimi_coding_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_env::CODE_BASE_URL_ENV, &server.uri());
        let err =
            authenticate_platform_api_key(kigi_models::PlatformId::KimiCoding, Some("kc-bad"))
                .await
                .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for kimi-coding \u{2014} check your key on www.kimi.com"
        );
    }

    /// Z.AI (global): /models is auth-gated (401 for a bad key) → validator.
    #[tokio::test]
    #[serial]
    async fn zai_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::ZAI_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(kigi_models::PlatformId::Zai, Some("zai-bad"))
            .await
            .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for zai \u{2014} check your key on z.ai"
        );
    }

    /// Z.AI Coding CN: distinct base URL (Zhipu BigModel) + console host.
    #[tokio::test]
    #[serial]
    async fn zai_coding_cn_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::ZAI_CODING_CN_BASE_URL_ENV, &server.uri());
        let err =
            authenticate_platform_api_key(kigi_models::PlatformId::ZaiCodingCn, Some("zai-cn-bad"))
                .await
                .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for zai-coding-cn \u{2014} check your key on open.bigmodel.cn"
        );
    }

    /// Xiaomi MiMo (global): /models is auth-gated (401 for a bad key).
    #[tokio::test]
    #[serial]
    async fn xiaomi_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::XIAOMI_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(kigi_models::PlatformId::Xiaomi, Some("xm-bad"))
            .await
            .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for xiaomi \u{2014} check your key on xiaomimimo.com"
        );
    }

    /// Xiaomi Token Plan CN: distinct base URL; same auth-gated validation.
    #[tokio::test]
    #[serial]
    async fn xiaomi_token_plan_cn_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(
            kigi_models::XIAOMI_TOKEN_PLAN_CN_BASE_URL_ENV,
            &server.uri(),
        );
        let err = authenticate_platform_api_key(
            kigi_models::PlatformId::XiaomiTokenPlanCn,
            Some("xm-cn-bad"),
        )
        .await
        .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for xiaomi-token-plan-cn \u{2014} check your key on xiaomimimo.com"
        );
    }

    /// MiniMax (global): Anthropic-compatible /models is x-api-key-gated (401
    /// for a bad key) → validator. Confirms the XApiKey header path is used.
    #[tokio::test]
    #[serial]
    async fn minimax_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{header, method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("x-api-key", "mm-bad"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::MINIMAX_BASE_URL_ENV, &server.uri());
        let err = authenticate_platform_api_key(kigi_models::PlatformId::Minimax, Some("mm-bad"))
            .await
            .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for minimax \u{2014} check your key on platform.minimax.io"
        );
    }

    /// MiniMax (China): distinct base URL + console host.
    #[tokio::test]
    #[serial]
    async fn minimax_cn_validates_against_models_and_rejects_bad_key() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::MINIMAX_CN_BASE_URL_ENV, &server.uri());
        let err =
            authenticate_platform_api_key(kigi_models::PlatformId::MinimaxCn, Some("mm-cn-bad"))
                .await
                .expect_err("a 401 from /models must reject the key");
        assert_eq!(
            err.message,
            "Invalid API key for minimax-cn \u{2014} check your key on platform.minimaxi.com"
        );
    }
}
