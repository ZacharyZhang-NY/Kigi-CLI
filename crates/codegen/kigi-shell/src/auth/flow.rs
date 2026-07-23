//! Auth-flow orchestration: cached credentials → silent refresh → the Kimi
//! Code device-code login (the only interactive login).

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::auth::{AuthManager, KimiAuth, KimiCodeConfig};
use crate::util::kigi_home;

/// How login presents itself; surfaced to the TUI via the auth URL event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthUrlMode {
    /// Device flow — TUI shows the verification URL (user code pre-filled).
    Device,
}

impl AuthUrlMode {
    /// Wire string for the ACP auth-url response.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Device => "device",
        }
    }
}

/// Auth URL pushed from the auth flow to the TUI.
pub struct AuthUrlInfo {
    pub url: String,
    pub mode: AuthUrlMode,
}

/// Channels for interactive login between the auth flow and the TUI/extension.
/// `code_rx` is unused by the device flow (the verification URL pre-fills the
/// user code) but kept so the ACP wiring stays uniform.
pub struct AuthChannels {
    pub url_tx: Option<oneshot::Sender<AuthUrlInfo>>,
    pub code_rx: mpsc::Receiver<String>,
}

/// GUI auth entry point (ACP `login` handler).
pub async fn run_auth_flow_with_stderr_bridge(
    auth_manager: &Arc<AuthManager>,
    kimi_code_config: &KimiCodeConfig,
    channels: AuthChannels,
    reauth: bool,
    force_interactive: bool,
) -> anyhow::Result<(KimiAuth, bool)> {
    run_auth_flow_inner(
        auth_manager,
        kimi_code_config,
        reauth,
        force_interactive,
        Some(channels),
    )
    .await
}

/// Full auth chain: cache → silent refresh → device-code login.
/// When `channels` is `None`, login output goes to stderr (CLI mode).
pub async fn run_auth_flow(
    auth_manager: &Arc<AuthManager>,
    kimi_code_config: &KimiCodeConfig,
    reauth: bool,
    channels: Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    run_auth_flow_inner(auth_manager, kimi_code_config, reauth, false, channels).await
}

/// Login flow for a GENERIC OAuth provider (xai-grok device-code,
/// claude-pro-max PKCE-localhost): use a valid cached session unless re-authing,
/// otherwise dispatch by `oauth.flow` to the device-code or PKCE-localhost login
/// (persisting under the provider's own scope via `auth_manager`). Unlike the
/// Kimi flow this does not run the silent-refresh dance — the login's
/// `AuthManager::update` persists a fresh token set directly.
pub async fn run_oauth_provider_flow(
    auth_manager: &Arc<AuthManager>,
    oauth: &'static kigi_models::OAuthConfig,
    reauth: bool,
    channels: Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    tracing::info!(
        scope_key = oauth.scope_key,
        reauth,
        "auth: starting generic oauth login"
    );
    if reauth {
        auth_manager.clear()?;
    }
    if !reauth && let Some(auth) = auth_manager.current() {
        tracing::info!(
            scope_key = oauth.scope_key,
            "auth: using cached oauth session"
        );
        return Ok((auth, false));
    }
    let mut channels = channels;
    match oauth.flow {
        kigi_models::OAuthFlow::DeviceCode => {
            crate::auth::device_code::run_device_code_login_generic(
                oauth,
                auth_manager,
                &mut channels,
            )
            .await
        }
        kigi_models::OAuthFlow::PkceLocalhost {
            redirect_port,
            redirect_path,
        } => {
            run_pkce_localhost_login(
                oauth,
                redirect_port,
                redirect_path,
                auth_manager,
                &mut channels,
            )
            .await
        }
        kigi_models::OAuthFlow::GithubDeviceCopilot => {
            crate::auth::device_code::run_device_code_login_github_copilot(
                oauth,
                auth_manager,
                &mut channels,
            )
            .await
        }
    }
}

/// PKCE-localhost login (claude-pro-max JSON, openai-codex FORM): generate PKCE,
/// present the browser authorize URL (TUI channel or stderr), open the browser,
/// then await the code from EITHER the `127.0.0.1:{redirect_port}{redirect_path}`
/// loopback callback OR a manual paste (headless fallback). Exchange it at the
/// token endpoint and persist.
///
/// The `token_body` selects the wire dialect: `Json` is the claude path
/// (`state == verifier`, JSON exchange carrying `state`); `Form` is the codex
/// path (fresh-random `state`, FORM exchange WITHOUT `state`, then a FAIL-FAST
/// check that the minted JWT carries a `chatgpt_account_id` — a token without it
/// is useless for inference, so the login bails rather than persisting it).
///
/// SECURITY: the verifier / code / tokens / JWT / account id are never logged;
/// the loopback binds `127.0.0.1` only and validates `state` strictly.
async fn run_pkce_localhost_login(
    oauth: &'static kigi_models::OAuthConfig,
    redirect_port: u16,
    redirect_path: &'static str,
    auth_manager: &Arc<AuthManager>,
    channels: &mut Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    use crate::auth::oauth_pkce;

    // The token-body encoding also selects the PKCE `state` convention: the JSON
    // dialect is Claude's/Pi's `state == verifier`, while form-encoded endpoints
    // (the OAuth norm) get an INDEPENDENT random state. A new form provider
    // inheriting the standard random state is correct by default.
    let uses_form_exchange = matches!(oauth.token_body, kigi_models::OAuthTokenBody::Form);
    let pkce = if uses_form_exchange {
        oauth_pkce::generate_pkce_random_state()
    } else {
        oauth_pkce::generate_pkce()
    };
    let redirect = oauth_pkce::redirect_uri(redirect_port, redirect_path);
    let authorize_url = oauth_pkce::build_authorize_url(oauth, &redirect, &pkce);

    let mut chans = channels.take();
    if let Some(tx) = chans.as_mut().and_then(|c| c.url_tx.take()) {
        // TUI: push the URL BEFORE opening the browser (never block the UI on a
        // slow/headless browser launch).
        let _ = tx.send(AuthUrlInfo {
            url: authorize_url.clone(),
            mode: AuthUrlMode::Device,
        });
        crate::auth::device_code::open_browser_detached(&authorize_url).await;
    } else {
        eprintln!();
        eprintln!("To sign in, open this URL in your browser:");
        eprintln!();
        eprintln!("  {authorize_url}");
        eprintln!();
        if !crate::auth::device_code::open_browser_detached(&authorize_url).await {
            eprintln!("  (Could not open the browser automatically — open the URL above.)");
            eprintln!();
        }
        eprintln!("Waiting for the sign-in to complete...");
    }

    let code = await_pkce_code(redirect_port, redirect_path, &pkce, chans.as_mut()).await?;
    let auth = if uses_form_exchange {
        oauth_pkce::exchange_code_form(oauth, &code, &pkce, &redirect).await?
    } else {
        oauth_pkce::exchange_code(oauth, &code, &pkce, &redirect).await?
    };
    // FAIL FAST: an access token that yields no chatgpt_account_id cannot
    // authorize inference (it becomes the `chatgpt-account-id` header) — bail
    // rather than persist a dead session. Gated on the EXPLICIT provider fact,
    // never on the token-body encoding.
    if oauth.requires_chatgpt_account_id
        && kigi_sampling_types::chatgpt_account_id_from_jwt(&auth.key).is_none()
    {
        anyhow::bail!(
            "ChatGPT login did not return a usable account id \
             (the access token is missing the chatgpt_account_id claim)"
        );
    }
    let auth = auth_manager
        .update(auth)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save credentials: {e}"))?;
    Ok((auth, true))
}

/// Await the authorization code: the loopback callback is primary; when a TUI
/// channel is present, a pasted code (redirect URL / `code#state` / bare code)
/// is accepted concurrently as a headless fallback. State is validated in both
/// arms (strict on the loopback, mismatch-rejecting on the paste).
async fn await_pkce_code(
    redirect_port: u16,
    redirect_path: &str,
    pkce: &crate::auth::oauth_pkce::PkceCodes,
    channels: Option<&mut AuthChannels>,
) -> anyhow::Result<String> {
    use crate::auth::oauth_pkce;
    match channels {
        Some(ch) => {
            tokio::select! {
                code = oauth_pkce::await_loopback_code(redirect_port, redirect_path, &pkce.state) => code,
                pasted = ch.code_rx.recv() => {
                    let pasted = pasted
                        .ok_or_else(|| anyhow::anyhow!("auth code channel closed before a code arrived"))?;
                    let params = oauth_pkce::parse_manual_paste(&pasted)?;
                    oauth_pkce::validate_pasted_state(&params, &pkce.state)?;
                    Ok(params.code)
                }
            }
        }
        None => oauth_pkce::await_loopback_code(redirect_port, redirect_path, &pkce.state).await,
    }
}

async fn run_auth_flow_inner(
    auth_manager: &Arc<AuthManager>,
    _kimi_code_config: &KimiCodeConfig,
    reauth: bool,
    force_interactive: bool,
    channels: Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    tracing::info!(reauth, force_interactive, "auth: starting auth flow");

    if reauth {
        auth_manager.clear()?;
    }

    if !force_interactive && let Some(auth) = auth_manager.current() {
        tracing::info!(auth_mode = ?auth.auth_mode, "auth: using cached credentials");
        kigi_log::unified_log::info(
            "auth: using cached credentials",
            None,
            Some(serde_json::json!({ "auth_mode": format!("{:?}", auth.auth_mode) })),
        );
        return Ok((auth, false));
    }

    if !force_interactive && !reauth && auth_manager.is_expired() {
        // Acquire the cross-process file lock so we don't race a refresher in
        // a sibling process. Without this, two processes can send the same
        // refresh_token simultaneously and trip server-side reuse detection.
        let _file_lock = auth_manager
            .try_lock_auth_file_async(crate::auth::manager::AUTH_LOCK_TIMEOUT)
            .await;

        // Read the persisted store first — another process may have already
        // refreshed.
        let disk_auth = auth_manager.read_disk_auth();
        let disk_expired = disk_auth.as_ref().is_some_and(crate::auth::is_expired);
        kigi_log::unified_log::info(
            "auth run_auth_flow expired path",
            None,
            Some(serde_json::json!({
                "got_lock": _file_lock.is_some(),
                "disk_found": disk_auth.is_some(),
                "disk_expired": disk_expired,
            })),
        );
        if let Some(d) = disk_auth.clone().filter(|d| !crate::auth::is_expired(d)) {
            kigi_log::unified_log::info("auth run_auth_flow using valid disk token", None, None);
            auth_manager.hot_swap(d.clone());
            return Ok((d, false));
        }

        // Persisted token not usable. Try the full auth() dispatcher which
        // handles refresh and sibling adoption — all through refresh_chain
        // (single mutation point).
        match auth_manager.auth().await {
            Ok(fresh) => return Ok((fresh, false)),
            Err(e) => {
                // Defer to consumer-level refresh if the store has a
                // refresh_token and the failure was transient.
                if let Some(d) = disk_auth.filter(|d| {
                    matches!(
                        &e,
                        crate::auth::error::AuthError::Refresh(
                            crate::auth::error::RefreshTokenError::Transient(_)
                        )
                    ) && d.refresh_token.is_some()
                }) {
                    kigi_log::unified_log::warn(
                        "auth run_auth_flow refresh failed, deferring to consumer refresh",
                        None,
                        Some(serde_json::json!({
                            "error": format!("{e}"),
                        })),
                    );
                    let ret = d.clone();
                    auth_manager.hot_swap(d);
                    return Ok((ret, false));
                }
                kigi_log::unified_log::warn(
                    "auth run_auth_flow refresh failed, falling through to interactive",
                    None,
                    Some(serde_json::json!({
                        "error": format!("{e}"),
                    })),
                );
            }
        }
    }

    let mut channels = channels;
    crate::auth::device_code::run_device_code_login_channels(
        &kigi_env::oauth_host(),
        auth_manager,
        &mut channels,
    )
    .await
}

/// Non-interactive auth refresh: returns valid credentials if available
/// without ever triggering an interactive login (browser, device code).
///
/// Tries in order:
/// 1. Cached credentials (non-expired)
/// 2. Silent refresh via the persisted refresh token
///
/// Returns `None` when no valid credentials can be obtained non-interactively.
pub async fn try_ensure_fresh_auth(kimi_code_config: &KimiCodeConfig) -> Option<KimiAuth> {
    let kigi_home = kigi_home::kigi_home();
    let auth_manager = Arc::new(AuthManager::new(&kigi_home, kimi_code_config.clone()));

    // auth() handles cached-valid (fast path) and refresh — all through
    // refresh_chain (single mutation point).
    auth_manager.configure_refresher();
    match auth_manager.auth().await {
        Ok(auth) => Some(auth),
        Err(e) => {
            tracing::debug!(error = %e, "try_ensure_fresh_auth: no valid credentials available");
            None
        }
    }
}

/// Like `try_ensure_fresh_auth` but for detached modes: when fresh auth is
/// unavailable, an expired-but-refreshable session is still returned so
/// consumers self-recover on 401 rather than disabling for the leader's
/// lifetime.
pub(crate) async fn try_ensure_session_noninteractive(
    kimi_code_config: &KimiCodeConfig,
) -> Option<KimiAuth> {
    if let Some(auth) = try_ensure_fresh_auth(kimi_code_config).await {
        return Some(auth);
    }
    let kigi_home = kigi_home::kigi_home();
    let auth_manager = Arc::new(AuthManager::new(&kigi_home, kimi_code_config.clone()));

    // A refresh failure leaves the session persisted (credentials are
    // retained; the tombstone gates re-attempts). Return it so consumers
    // self-recover on 401.
    expired_refreshable_session(&auth_manager)
}

/// A cached, refreshable session (not an API key). Reached only after fresh
/// auth failed, so in practice the token is expired but recoverable on 401.
fn expired_refreshable_session(auth_manager: &AuthManager) -> Option<KimiAuth> {
    auth_manager
        .current_or_expired()
        .filter(|a| a.is_session_auth() && a.refresh_token.is_some())
}

/// Print the CLI "signed in" confirmation, clearing the spinner line first.
fn report_signed_in(auth: &KimiAuth) {
    eprint!("\r\x1b[K");
    match auth.email {
        Some(ref email) => eprintln!("✓ Signed in as {email}"),
        None => eprintln!("✓ Signed in"),
    }
}

/// CLI auth entrypoint. For GUI, use `run_auth_flow_with_stderr_bridge`.
pub async fn ensure_authenticated(
    kimi_code_config: &KimiCodeConfig,
    reauth: bool,
    message_prefix: Option<&str>,
) -> anyhow::Result<KimiAuth> {
    let kigi_home = kigi_home::kigi_home();
    let auth_manager = Arc::new(AuthManager::new(&kigi_home, kimi_code_config.clone()));

    if !reauth && let Some(auth) = auth_manager.current() {
        return Ok(auth);
    }

    // Context only — the flow below prints the sign-in prompts itself.
    if let Some(msg) = message_prefix {
        eprintln!("{msg}");
    }

    let (auth, did_auth) = run_auth_flow(&auth_manager, kimi_code_config, reauth, None).await?;

    if did_auth {
        report_signed_in(&auth);
    }

    Ok(auth)
}

/// Decides *whether to prompt* for an interactive login (the wire credential
/// is chosen separately by `ShellAuthCredentialProvider`).
///
/// With `has_noninteractive_auth`, only refresh a cached token best-effort
/// (no browser, no device prompt); otherwise require an interactive login.
pub async fn ensure_authenticated_or_noninteractive(
    kimi_code_config: &KimiCodeConfig,
    has_noninteractive_auth: bool,
    message_prefix: Option<&str>,
) -> anyhow::Result<Option<KimiAuth>> {
    if has_noninteractive_auth {
        Ok(try_ensure_fresh_auth(kimi_code_config).await)
    } else {
        ensure_authenticated(kimi_code_config, false, message_prefix)
            .await
            .map(Some)
    }
}

/// `kigi login` handler for CLI entry points (tui, pager): the device-code
/// flow is THE login.
///
/// Runs with `force_interactive` semantics — cached credentials are skipped
/// but not cleared, so abandoning the device prompt doesn't log the user out.
pub async fn run_cli_login(config: &crate::agent::config::Config) -> anyhow::Result<()> {
    let kigi_home = kigi_home::kigi_home();
    let auth_manager = Arc::new(AuthManager::new(
        &kigi_home,
        config.kimi_code_config.clone(),
    ));
    let (auth, did_auth) =
        run_auth_flow_inner(&auth_manager, &config.kimi_code_config, false, true, None).await?;
    if did_auth {
        report_signed_in(&auth);
    }
    Ok(())
}

/// Result of a logout operation. Used by both the CLI subcommand and
/// the ACP `/logout` slash command so the presentation layer can format
/// the outcome without duplicating the auth logic.
pub struct LogoutResult {
    /// `true` if a cached OAuth session was found and cleared.
    pub was_logged_in: bool,
    /// Email of the session that was cleared (if available).
    pub email: Option<String>,
    /// `true` if an API-key env var is set.
    pub api_key_still_set: bool,
}

/// Core logout logic shared by the CLI subcommand and the ACP handler.
///
/// When `scope` is `None`, clears the default scope (same as `/logout`
/// in the TUI). When `Some`, removes only that scope entry. The session
/// credential is removed from both stores (keyring + file).
pub fn perform_logout(
    auth_manager: &AuthManager,
    scope: Option<&str>,
) -> std::io::Result<LogoutResult> {
    let auth = auth_manager.current_or_expired();
    let email = auth.as_ref().and_then(|a| a.email.clone());
    let was_logged_in = auth.is_some();
    // Intentional credential removal must be attributable in
    // unified.jsonl, so a later "auth entry gone" can be
    // distinguished from accidental loss (deleted/corrupt file).
    kigi_log::unified_log::info(
        "auth: logout",
        None,
        Some(serde_json::json!({
            "was_logged_in": was_logged_in,
            "scope": scope.unwrap_or("(current)"),
        })),
    );
    if was_logged_in {
        if let Some(scope) = scope {
            auth_manager.remove_scope(scope)?;
        } else {
            auth_manager.clear()?;
        }
    }
    Ok(LogoutResult {
        was_logged_in,
        email,
        api_key_still_set: crate::agent::auth_method::has_xai_api_key_env(),
    })
}

/// `kigi logout` CLI handler. Calls [`perform_logout`] and formats
/// the result to stderr.
pub fn run_cli_logout(config: &crate::agent::config::Config) -> anyhow::Result<()> {
    let kigi_home = kigi_home::kigi_home();
    let auth_manager = AuthManager::new(&kigi_home, config.kimi_code_config.clone());
    let result = perform_logout(&auth_manager, None)
        .map_err(|e| anyhow::anyhow!("Failed to clear auth: {e}"))?;
    if !result.was_logged_in {
        eprintln!("No cached session to log out of.");
        if result.api_key_still_set {
            eprintln!("You are authenticated via an API-key environment variable.");
        }
        return Ok(());
    }
    if let Some(email) = result.email {
        eprintln!("Logged out (was signed in as {email})");
    } else {
        eprintln!("Logged out");
    }
    if result.api_key_still_set {
        eprintln!("An API-key environment variable is still set and will be used.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;
    use chrono::Utc;

    // A Kimi Code OAuth session credential.
    fn oauth_session(key: &str, refresh: Option<&str>) -> KimiAuth {
        KimiAuth {
            key: key.into(),
            auth_mode: AuthMode::OAuth,
            refresh_token: refresh.map(str::to_string),
            ..KimiAuth::test_default()
        }
    }

    #[test]
    fn expired_refreshable_session_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = AuthManager::new(dir.path(), KimiCodeConfig::default());

        // Expired but refreshable → returned. Guards a `current_or_expired()`
        // -> `current()` regression that would disable detached consumers on
        // a transient blip.
        mgr.hot_swap(KimiAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oauth_session("expired-but-refreshable", Some("rt"))
        });
        assert!(
            mgr.current().is_none(),
            "precondition: token must be expired"
        );
        assert_eq!(
            expired_refreshable_session(&mgr).map(|a| a.key),
            Some("expired-but-refreshable".to_string())
        );

        // No refresh token → rejected: never hand consumers a token they
        // can't recover on 401.
        mgr.hot_swap(oauth_session("no-rt", None));
        assert!(expired_refreshable_session(&mgr).is_none());

        // API keys are excluded (not a session).
        mgr.hot_swap(KimiAuth {
            auth_mode: AuthMode::ApiKey,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oauth_session("api-key", Some("rt"))
        });
        assert!(expired_refreshable_session(&mgr).is_none());
    }

    /// When the in-memory token is expired but the store has a valid token,
    /// run_auth_flow should return the stored token without interactive login.
    #[tokio::test]
    async fn run_auth_flow_uses_valid_disk_token_when_expired() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KimiCodeConfig::default();

        // Write a valid token via a second AuthManager (simulates a sibling
        // process that already refreshed).
        let writer = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let valid_disk = KimiAuth {
            key: "fresh-token-from-disk".into(),
            auth_mode: AuthMode::OAuth,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            expires_in: Some(3600),
            refresh_token: Some("new-rt".into()),
            ..KimiAuth::test_default()
        };
        writer.update(valid_disk).await.unwrap();

        // Primary manager: in-memory token is expired
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let expired = KimiAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::OAuth,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("old-rt".into()),
            ..KimiAuth::test_default()
        };
        mgr.hot_swap(expired);
        assert!(mgr.is_expired());

        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None).await.unwrap();

        assert_eq!(auth.key, "fresh-token-from-disk");
        assert!(!is_new_login, "should not be a new login");
        // In-memory should be updated via hot_swap
        assert_eq!(mgr.current().unwrap().key, "fresh-token-from-disk");
    }

    /// When the in-memory token is valid (not expired), run_auth_flow should
    /// return it directly without checking the store.
    #[tokio::test]
    async fn run_auth_flow_returns_cached_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KimiCodeConfig::default();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));

        let valid = KimiAuth {
            key: "still-valid".into(),
            auth_mode: AuthMode::OAuth,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            expires_in: Some(3600),
            ..KimiAuth::test_default()
        };
        mgr.hot_swap(valid);

        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None).await.unwrap();

        assert_eq!(auth.key, "still-valid");
        assert!(!is_new_login);
    }

    #[tokio::test]
    async fn run_auth_flow_defers_to_consumer_refresh_on_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KimiCodeConfig::default();

        let writer = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let expired_with_rt = KimiAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::OAuth,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("valid-refresh-token".into()),
            ..KimiAuth::test_default()
        };
        writer.update(expired_with_rt.clone()).await.unwrap();

        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        mgr.hot_swap(expired_with_rt);
        assert!(mgr.is_expired());
        mgr.set_refresher(std::sync::Arc::new(AlwaysTransientRefresher));

        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None).await.unwrap();

        assert_eq!(auth.key, "expired-access-token");
        assert!(auth.refresh_token.is_some());
        assert!(!is_new_login);
    }

    struct AlwaysTransientRefresher;

    #[async_trait::async_trait]
    impl crate::auth::refresh::TokenRefresher for AlwaysTransientRefresher {
        async fn refresh(
            &self,
            _reason: crate::auth::manager::RefreshReason,
        ) -> crate::auth::refresh::RefreshOutcome {
            crate::auth::refresh::RefreshOutcome::TransientFailure {
                message: "simulated network failure".into(),
            }
        }
    }
}
