//! Kimi Code device-code login (PRD F1).
//!
//! Two-phase API mirroring kimi-cli's `login_kimi_code`:
//!   1. [`crate::auth::kimi_oauth::request_device_authorization`] — get a
//!      user code + verification URL from the OAuth host
//!   2. [`complete_device_code_login`] — poll the token endpoint until
//!      approved, then persist the token set via the `AuthManager`
//!
//! Poll semantics: the server-provided interval (default 5s, floored at 1s)
//! paces the loop; `slow_down` bumps it by 5s; `expired_token` restarts the
//! whole device authorization (fresh user code); every other non-200 outcome
//! (`authorization_pending`, unknown errors) waits and continues.

use std::sync::Arc;

use crate::auth::kimi_oauth::{
    DeviceAuthorization, DevicePollResult, poll_device_token, request_device_authorization,
};
use crate::auth::{AuthChannels, AuthManager, AuthUrlInfo, AuthUrlMode, KimiAuth};

/// Extra wait added to the poll interval when the server answers `slow_down`
/// (OAuth-standard device-flow backpressure).
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;

/// Outcome of one full poll loop over a single device authorization.
enum PollLoopOutcome {
    /// Access token issued.
    Done(Box<KimiAuth>),
    /// The device code expired before the user approved — request a fresh
    /// authorization and start over.
    Restart,
}

/// Device-code login shared by the TUI and CLI.
///
/// With `channels` (TUI) the verification URL goes to `url_tx` and the
/// browser opens automatically. Without `channels` (CLI) the URL + code are
/// printed to stderr. The caller reports success (`✓ Signed in`).
pub async fn run_device_code_login_channels(
    host: &str,
    auth_manager: &Arc<AuthManager>,
    channels: &mut Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    let interactive_tui = channels.is_some();
    let mut channels = channels.take();
    loop {
        let device_auth = request_device_authorization(host).await?;
        let display_uri = device_auth.verification_uri_complete.clone();

        if interactive_tui {
            // TUI: push the URL through the channel BEFORE opening the
            // browser, so the UI isn't blocked on a slow/hanging browser
            // launch (e.g. SSH/headless).
            if let Some(tx) = channels.as_mut().and_then(|c| c.url_tx.take()) {
                let _ = tx.send(AuthUrlInfo {
                    url: display_uri.clone(),
                    mode: AuthUrlMode::Device,
                });
            }
            open_browser_detached(&display_uri).await;
        } else {
            prompt_on_stderr(&device_auth).await;
        }

        match complete_device_code_login(host, &device_auth).await? {
            PollLoopOutcome::Done(auth) => {
                let auth = auth_manager
                    .update(*auth)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to save credentials: {e}"))?;
                return Ok((auth, true));
            }
            PollLoopOutcome::Restart => {
                tracing::info!("auth: device code expired, restarting device authorization");
                if !interactive_tui {
                    eprintln!("Device code expired — requesting a new one...");
                }
                // The TUI already consumed url_tx; the restarted flow can
                // only reach the user via the (re-)opened browser page.
                continue;
            }
        }
    }
}

/// Print the verification URL + user code to stderr and open the browser.
async fn prompt_on_stderr(device_auth: &DeviceAuthorization) {
    let display_uri = &device_auth.verification_uri_complete;
    eprintln!();
    eprintln!("To sign in, open this URL in your browser:");
    eprintln!();
    eprintln!("  {display_uri}");
    eprintln!();
    if !open_browser_detached(display_uri).await {
        eprintln!("  (Could not open browser automatically — open the URL above manually.)");
        eprintln!();
    }
    // Show the code so the user can confirm it matches the browser
    // (anti-phishing): the complete URL pre-fills it.
    eprintln!("Confirm this code in your browser:");
    eprintln!();
    eprintln!("  {}", device_auth.user_code);
    eprintln!();
    eprintln!(
        "\x1b[90mOnly continue with a code you requested. \
         Don't share it with anyone.\x1b[0m"
    );
    eprintln!();
    eprintln!("Waiting for authorization...");
}

/// Poll the token endpoint until the user approves, the device code expires
/// (→ [`PollLoopOutcome::Restart`]), or the wire fails.
async fn complete_device_code_login(
    host: &str,
    device_auth: &DeviceAuthorization,
) -> anyhow::Result<PollLoopOutcome> {
    let mut poll_interval = std::time::Duration::from_secs(device_auth.interval.max(1) as u64);
    loop {
        // Sleep first: an immediate poll on a fresh code only returns
        // authorization_pending (and risks slow_down).
        tokio::time::sleep(poll_interval).await;
        match poll_device_token(host, &device_auth.device_code).await? {
            DevicePollResult::Success(auth) => {
                tracing::info!("auth: device login authorized");
                return Ok(PollLoopOutcome::Done(auth));
            }
            DevicePollResult::Expired => return Ok(PollLoopOutcome::Restart),
            DevicePollResult::Pending { error, description } => {
                if error == "slow_down" {
                    poll_interval += std::time::Duration::from_secs(SLOW_DOWN_INCREMENT_SECS);
                    tracing::info!(
                        new_interval_secs = poll_interval.as_secs(),
                        "auth: server asked to slow down device polling"
                    );
                } else {
                    tracing::debug!(
                        error = %error,
                        description = ?description,
                        "auth: device authorization pending"
                    );
                }
            }
        }
    }
}

/// Open `url` in the browser off-thread: `webbrowser::open` is synchronous and
/// would stall the single-threaded TUI loop. Returns `true` on success so the
/// caller can decide how to notify the user (eprintln on CLI, nothing on TUI
/// where the URL is already rendered in the widget).
async fn open_browser_detached(url: &str) -> bool {
    let url = url.to_owned();
    match tokio::task::spawn_blocking(move || webbrowser::open(&url)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::info!(error = %e, "device auth: could not open browser automatically");
            false
        }
        Err(e) => {
            tracing::info!(error = %e, "device auth: browser-open task failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::KimiCodeConfig;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn device_auth_json(code: &str) -> serde_json::Value {
        serde_json::json!({
            "user_code": "ABCD-1234",
            "device_code": code,
            "verification_uri": "https://auth.kimi.com/device",
            "verification_uri_complete": "https://auth.kimi.com/device?code=ABCD-1234",
            "expires_in": 600,
            "interval": 0, // floored to 1s by the poll loop
        })
    }

    fn token_json(access: &str) -> serde_json::Value {
        serde_json::json!({
            "access_token": access,
            "refresh_token": "rt-1",
            "expires_in": 3600,
            "scope": "kimi-code",
            "token_type": "bearer",
        })
    }

    fn auth_manager(dir: &tempfile::TempDir) -> Arc<AuthManager> {
        Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()))
    }

    /// End-to-end (mock server): authorization → pending → token, persisting
    /// via the AuthManager.
    #[tokio::test]
    async fn device_login_persists_token_after_pending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_auth_json("dev-1")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "authorization_pending" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("device_code=dev-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-done")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mgr = auth_manager(&dir);
        let mut channels = None;
        let (auth, is_new) = run_device_code_login_channels(&server.uri(), &mgr, &mut channels)
            .await
            .unwrap();
        assert!(is_new);
        assert_eq!(auth.key, "at-done");
        assert_eq!(
            mgr.current_or_expired().map(|a| a.key),
            Some("at-done".into()),
            "login must land in the manager cache"
        );
        assert!(
            dir.path().join("auth.json").exists(),
            "login must persist to the fallback file store"
        );
    }

    /// `expired_token` during polling restarts the whole device
    /// authorization (fresh device code), then completes.
    #[tokio::test]
    async fn expired_token_restarts_device_authorization() {
        let server = MockServer::start().await;
        // First authorization issues dev-1; second issues dev-2.
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_auth_json("dev-1")))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_auth_json("dev-2")))
            .expect(1)
            .mount(&server)
            .await;
        // dev-1 polls expire; dev-2 polls succeed.
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("device_code=dev-1"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "expired_token" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("device_code=dev-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-restarted")))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mgr = auth_manager(&dir);
        let mut channels = None;
        let (auth, _) = run_device_code_login_channels(&server.uri(), &mgr, &mut channels)
            .await
            .unwrap();
        assert_eq!(auth.key, "at-restarted");
    }

    /// `slow_down` is wait-and-continue (interval bumped), never fatal.
    /// Unknown-error continuation is covered at the wire level by
    /// `kimi_oauth::tests::poll_maps_pending_and_unknown_errors_to_pending`.
    #[tokio::test]
    async fn slow_down_keeps_polling_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_auth_json("dev-1")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "slow_down" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-patient")))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mgr = auth_manager(&dir);
        let mut channels = None;
        let started = std::time::Instant::now();
        let (auth, _) = run_device_code_login_channels(&server.uri(), &mgr, &mut channels)
            .await
            .unwrap();
        assert_eq!(auth.key, "at-patient");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(6),
            "slow_down must bump the poll interval by {SLOW_DOWN_INCREMENT_SECS}s"
        );
    }

    /// A 5xx from the token endpoint is a hard error (kimi-cli parity).
    #[tokio::test]
    async fn server_error_during_poll_fails_login() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_auth_json("dev-1")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mgr = auth_manager(&dir);
        let mut channels = None;
        let err = run_device_code_login_channels(&server.uri(), &mgr, &mut channels)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server error"), "{err}");
        assert!(
            mgr.current_or_expired().is_none(),
            "failed login must not persist credentials"
        );
    }
}
