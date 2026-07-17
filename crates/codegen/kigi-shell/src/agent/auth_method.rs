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

/// Env var that, when set, advertises `xai.api_key` as a viable auth method.
///
/// Kept as a constant so test code and the production check stay in sync.
pub const XAI_API_KEY_ENV_VAR: &str = "XAI_API_KEY";

/// Legacy env var name. Checked as a fallback when `XAI_API_KEY` is not set,
/// so existing deployments that use the old name keep working.
pub const LEGACY_XAI_API_KEY_ENV_VAR: &str = "KIGI_CODE_XAI_API_KEY";

/// Read the API key from the environment.
///
/// Checks `XAI_API_KEY` first, then falls back to the legacy
/// `KIGI_CODE_XAI_API_KEY` for backward compatibility.
pub fn read_xai_api_key_env() -> Result<String, std::env::VarError> {
    std::env::var(XAI_API_KEY_ENV_VAR).or_else(|_| std::env::var(LEGACY_XAI_API_KEY_ENV_VAR))
}

/// Returns `true` if either `XAI_API_KEY` or `KIGI_CODE_XAI_API_KEY` is set.
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
/// 3. `grok.com`        (the Kimi Code device login)
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
    GrokCom,
    Unknown,
}

impl AuthMethodKind {
    pub fn from_id(id: &acp::AuthMethodId) -> Self {
        match id.0.as_ref() {
            XAI_API_KEY_METHOD_ID => Self::XaiApiKey,
            CACHED_TOKEN_AUTH_METHOD_ID => Self::CachedToken,
            KIGI_COM_METHOD_ID => Self::GrokCom,
            _ => Self::Unknown,
        }
    }

    /// API key auth: no auth.json, no refresh, no user interaction.
    pub fn is_api_key(self) -> bool {
        matches!(self, Self::XaiApiKey)
    }

    /// `true` for session-based methods (cached_token, interactive login).
    pub fn is_session_based(self) -> bool {
        matches!(self, Self::CachedToken | Self::GrokCom)
    }

    /// Requires user interaction (device-code login in the browser).
    pub fn needs_interactive_login(self) -> bool {
        matches!(self, Self::GrokCom)
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

pub const AUTH_ERROR_API_KEY: &str = "Authentication failed. Run `kigi login`, set XAI_API_KEY, or add api_key to ~/.kigi/config.toml.";

/// Next ACP method id when `cached_token` cannot proceed (missing / expired):
/// prefer non-interactive `xai.api_key` when advertiseable, else the
/// interactive device login.
pub fn method_id_after_cached_token_unavailable(has_external_api_key: bool) -> &'static str {
    if has_external_api_key {
        XAI_API_KEY_METHOD_ID
    } else {
        KIGI_COM_METHOD_ID
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
            "{XAI_API_KEY_ENV_VAR} or api_key/env_key in config.toml"
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

/// Interactive login method id. The literal `"grok.com"` is kept for ACP
/// wire-compat with the in-repo pager (renaming is a cross-crate wire change
/// deferred to the command-surface milestone).
pub const KIGI_COM_METHOD_ID: &str = "grok.com";

/// The Kimi Code device-code login.
pub fn kimi_code_auth_method(label: Option<&str>) -> acp::AuthMethod {
    let name = label.unwrap_or("Kimi Code");
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(acp::AuthMethodId::new(KIGI_COM_METHOD_ID), name.to_string())
            .description(Some(format!("Sign in with {name}"))),
    )
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
            KIGI_COM_METHOD_ID,
        );
    }

    /// Classifier matrix for all auth method variants.
    #[test]
    fn auth_method_kind_classifier_matrix() {
        let session_methods = [CACHED_TOKEN_AUTH_METHOD_ID, KIGI_COM_METHOD_ID];
        for id in session_methods {
            let kind = AuthMethodKind::from_id(&acp::AuthMethodId::new(id));
            assert!(kind.is_session_based(), "{id} must be session-based");
            assert!(!kind.is_api_key(), "{id} must not be api-key");
        }
        let api = AuthMethodKind::from_id(&acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID));
        assert!(api.is_api_key());
        assert!(!api.is_session_based());
        assert!(!api.needs_interactive_login());
        let unknown = AuthMethodKind::from_id(&acp::AuthMethodId::new("who-knows"));
        assert_eq!(unknown, AuthMethodKind::Unknown);
        assert!(!unknown.is_session_based());
        // Only the interactive login needs a browser.
        assert!(
            AuthMethodKind::from_id(&acp::AuthMethodId::new(KIGI_COM_METHOD_ID))
                .needs_interactive_login()
        );
        assert!(
            !AuthMethodKind::from_id(&acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID))
                .needs_interactive_login()
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
            vec![XAI_API_KEY_METHOD_ID, KIGI_COM_METHOD_ID]
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
                KIGI_COM_METHOD_ID
            ]
        );
        assert_eq!(default_id(&built), Some(CACHED_TOKEN_AUTH_METHOD_ID));
    }

    /// Session-only user: cached_token first, interactive login as fallback.
    #[test]
    fn session_only_user_first_method_is_cached_token() {
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_cached_token: true,
            ..default_inputs()
        });
        assert_eq!(
            method_ids(&built),
            vec![CACHED_TOKEN_AUTH_METHOD_ID, KIGI_COM_METHOD_ID]
        );
        assert_eq!(default_id(&built), Some(CACHED_TOKEN_AUTH_METHOD_ID));
        assert_eq!(
            first_kind(&built.methods),
            Some(AuthMethodKind::CachedToken)
        );
    }

    /// Fresh user: only the interactive login is advertised; no default
    /// method (login required).
    #[test]
    fn fresh_user_only_advertises_interactive_login() {
        let built = build_auth_methods(default_inputs());
        assert_eq!(method_ids(&built), vec![KIGI_COM_METHOD_ID]);
        assert_eq!(default_id(&built), None);
    }

    /// `XAI_API_KEY` alone (no per-model creds) triggers advertising
    /// `xai.api_key` as the first method.
    #[test]
    #[serial]
    fn global_external_api_key_advertises_xai_api_key_first() {
        let _set = EnvGuard::set(XAI_API_KEY_ENV_VAR, "xai-external-key");
        let cfg = Config::default();
        let models = resolve_model_list(&cfg, None);
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
        let _unset = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
        let _set = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-key");
        assert!(has_xai_api_key_env());
        assert_eq!(read_xai_api_key_env().unwrap(), "legacy-key");
    }

    /// The new env var takes precedence over the legacy one.
    #[test]
    #[serial]
    fn new_env_var_takes_precedence_over_legacy() {
        let _new = EnvGuard::set(XAI_API_KEY_ENV_VAR, "new-key");
        let _legacy = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-key");
        assert_eq!(read_xai_api_key_env().unwrap(), "new-key");
    }
}
