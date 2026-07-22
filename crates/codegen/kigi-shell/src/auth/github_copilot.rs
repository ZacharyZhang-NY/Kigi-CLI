//! GitHub Copilot two-stage OAuth wire (github-copilot), driven by a registry
//! [`kigi_models::OAuthConfig`] whose `flow` is [`OAuthFlow::GithubDeviceCopilot`].
//!
//! Stage 1 — RFC-8628 device flow on `auth_host` (github.com). The device
//! authorization POST is the generic one ([`super::oauth_device`]); the token
//! POLL is Copilot-specific because GitHub returns its device errors in a `200`
//! body (`{error: "authorization_pending"|"slow_down"|"expired_token"}`), not a
//! `4xx`, and the success payload carries ONLY `access_token` (the DURABLE
//! GitHub token — no refresh token, no expiry).
//!
//! Stage 2 — copilot-token exchange: `GET {copilot_exchange}` bearing the GitHub
//! token + the editor headers re-mints the SHORT-LIVED copilot session token
//! (`{token, expires_at}`). This runs at login ([`exchange_copilot_token`]) and
//! on every "refresh" ([`remint_copilot_token`], dispatched by the generic
//! refresher) — the github token is unchanged and re-persisted as the
//! `refresh_token`; the copilot token becomes the `key`.
//!
//! SECURITY: the github token and the copilot token are NEVER logged (only
//! non-secret events: poll succeeded, copilot token minted/re-minted).

use chrono::{DateTime, Utc};
use kigi_models::OAuthConfig;
use serde::Deserialize;

use super::kimi_oauth::{DevicePollResult, RefreshError};
use super::model::{AuthMode, KimiAuth};

/// RFC-8628 device grant type (shared with the generic device wire).
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Copilot-exchange retry budget over 5xx / network blips (parity with the
/// device/PKCE refresh wires); 401/403 fails fast (the github token is dead).
const MAX_EXCHANGE_RETRIES: u32 = 3;
const RETRYABLE_EXCHANGE_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

/// The four VS Code Copilot editor-identity headers every Copilot request
/// carries. Non-secret wire constants owned by `kigi_sampling_types` (the
/// single source shared with the sampler's inference gate).
fn editor_headers() -> [(&'static str, &'static str); 4] {
    [
        ("User-Agent", kigi_sampling_types::COPILOT_USER_AGENT),
        (
            "Editor-Version",
            kigi_sampling_types::COPILOT_EDITOR_VERSION,
        ),
        (
            "Editor-Plugin-Version",
            kigi_sampling_types::COPILOT_EDITOR_PLUGIN_VERSION,
        ),
        (
            "Copilot-Integration-Id",
            kigi_sampling_types::COPILOT_INTEGRATION_ID,
        ),
    ]
}

/// GitHub's device-token poll response: EITHER `access_token` (the durable
/// GitHub token) OR an `error` (in a `200` body). No refresh token / expiry.
#[derive(Deserialize, Default)]
struct GithubDeviceTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// One poll of `POST {auth_host}{token_path}` (github.com/login/oauth/
/// access_token) with the device grant. GitHub answers `200` for BOTH success
/// and the pending/slow_down/expired errors, so the outcome is read from the
/// body, not the status. On success the [`KimiAuth`] carries the GitHub token as
/// `key` with NO refresh token / expiry — the caller finalizes it via the
/// copilot exchange before persisting.
pub(crate) async fn poll_github_device_token(
    cfg: &OAuthConfig,
    device_code: &str,
) -> anyhow::Result<DevicePollResult> {
    let url = format!("{}{}", cfg.auth_host.trim_end_matches('/'), cfg.token_path);
    let resp = crate::http::shared_client()
        .post(&url)
        .header("Accept", "application/json")
        .header("User-Agent", kigi_sampling_types::COPILOT_USER_AGENT)
        .form(&[
            ("client_id", cfg.client_id),
            ("device_code", device_code),
            ("grant_type", DEVICE_GRANT_TYPE),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Token polling request failed: {e}"))?;

    let status = resp.status();
    if status.is_server_error() {
        anyhow::bail!("Token polling server error: {status}");
    }
    let body = resp.bytes().await?;
    let parsed: GithubDeviceTokenResponse = serde_json::from_slice(&body).unwrap_or_default();

    if let Some(access) = parsed.access_token.filter(|t| !t.is_empty()) {
        tracing::info!("auth: github device poll succeeded, github token issued (copilot)");
        return Ok(DevicePollResult::Success(Box::new(github_token_auth(
            access,
        ))));
    }
    match parsed.error.as_deref() {
        Some("expired_token") => {
            tracing::info!("auth: github device code expired; restarting (copilot)");
            Ok(DevicePollResult::Expired)
        }
        Some(error) => {
            tracing::debug!(error, "auth: github device poll pending (copilot)");
            Ok(DevicePollResult::Pending {
                error: error.to_owned(),
                description: None,
            })
        }
        None => Ok(DevicePollResult::Pending {
            error: "missing_access_token".to_owned(),
            description: None,
        }),
    }
}

/// A transient [`KimiAuth`] holding ONLY the durable GitHub token (no refresh
/// token / expiry) — the intermediate device-flow result, finalized by the
/// copilot exchange before it is ever persisted.
fn github_token_auth(github_token: String) -> KimiAuth {
    KimiAuth {
        key: github_token,
        auth_mode: AuthMode::OAuth,
        create_time: Utc::now(),
        user_id: String::new(),
        email: None,
        refresh_token: None,
        expires_at: None,
        expires_in: None,
        scope: None,
        token_type: None,
    }
}

/// Stage-2 copilot-token exchange response (`GET copilot_internal/v2/token`).
/// `endpoints`/`proxy-ep` are ignored: Kigi resolves the base URL from the
/// platform registry (the individual-subscription endpoint, or the
/// `KIGI_COPILOT_BASE_URL` override).
#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    /// Unix seconds when the copilot token expires (~30 min out).
    expires_at: i64,
}

/// Materialize the persisted credential from a copilot-token exchange:
/// `key` = the short-lived copilot token, `refresh_token` = the DURABLE github
/// token (so every re-mint re-exchanges it), `expires_at` = the copilot expiry.
fn copilot_auth(resp: CopilotTokenResponse, github_token: &str) -> anyhow::Result<KimiAuth> {
    let now = Utc::now();
    // FAIL-FAST: an uninterpretable expiry means we cannot schedule the re-mint,
    // so reject it rather than silently falling back to a long default TTL — which
    // would let the ~30-min copilot token 401 on the wire ~30 min later.
    let expires_at = DateTime::from_timestamp(resp.expires_at, 0).ok_or_else(|| {
        anyhow::anyhow!(
            "copilot token has an out-of-range expires_at: {}",
            resp.expires_at
        )
    })?;
    // The manager's dynamic threshold (`max(300, expires_in × 0.5)`) drives the
    // proactive re-mint; `expires_in` is the copilot token's remaining life.
    let expires_in = (expires_at - now).num_seconds();
    Ok(KimiAuth {
        key: resp.token,
        auth_mode: AuthMode::OAuth,
        create_time: now,
        user_id: String::new(),
        email: None,
        refresh_token: Some(github_token.to_owned()),
        expires_at: Some(expires_at),
        expires_in: Some(expires_in),
        scope: None,
        token_type: Some("bearer".to_owned()),
    })
}

/// The `(host, path)` of the copilot-token exchange endpoint, or a fatal error
/// when the config lacks it (a non-Copilot config reaching this wire is a bug).
fn exchange_url(cfg: &OAuthConfig) -> anyhow::Result<String> {
    let (host, path) = cfg.copilot_exchange.ok_or_else(|| {
        anyhow::anyhow!("github-copilot config missing copilot_exchange endpoint")
    })?;
    Ok(format!("{}{path}", host.trim_end_matches('/')))
}

/// `GET {copilot_exchange}` bearing the github token + editor headers.
async fn send_copilot_exchange(
    cfg: &OAuthConfig,
    github_token: &str,
) -> anyhow::Result<reqwest::Response> {
    let url = exchange_url(cfg)?;
    let mut req = crate::http::shared_client()
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {github_token}"));
    for (name, value) in editor_headers() {
        req = req.header(name, value);
    }
    req.send()
        .await
        .map_err(|e| anyhow::anyhow!("copilot-token exchange request failed: {e}"))
}

/// Exchange the durable GitHub token for a copilot session token (login path).
/// FAIL-FAST: a non-2xx response aborts login (never a silent fallback).
pub(crate) async fn exchange_copilot_token(
    cfg: &OAuthConfig,
    github_token: &str,
) -> anyhow::Result<KimiAuth> {
    tracing::info!(
        scope_key = cfg.scope_key,
        "auth: exchanging github token for copilot token"
    );
    let resp = send_copilot_exchange(cfg, github_token).await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, scope_key = cfg.scope_key, "auth: copilot-token exchange failed");
        anyhow::bail!("Copilot token exchange failed (HTTP {status}): {body}");
    }
    let parsed: CopilotTokenResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("malformed copilot token payload: {e}"))?;
    tracing::info!(scope_key = cfg.scope_key, "auth: copilot token minted");
    copilot_auth(parsed, github_token)
}

/// Re-mint the copilot token from the durable GitHub token (refresh path,
/// dispatched from the generic refresher). This is NOT a `refresh_token` grant:
/// it re-runs the copilot exchange. `refresh_token` is the github token; the
/// returned [`KimiAuth`] preserves it. Retries 5xx / network blips; 401/403
/// (the github token is revoked) fails fast as [`RefreshError::Unauthorized`].
pub(crate) async fn remint_copilot_token(
    cfg: &OAuthConfig,
    github_token: &str,
) -> Result<KimiAuth, RefreshError> {
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_EXCHANGE_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tracing::warn!(
                attempt,
                backoff_secs = backoff.as_secs(),
                "auth: retrying copilot-token re-mint"
            );
            tokio::time::sleep(backoff).await;
        }
        let resp = match send_copilot_exchange(cfg, github_token).await {
            Ok(resp) => resp,
            Err(e) => {
                last_error = format!("{e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.unwrap_or_default();
        if status == 401 || status == 403 {
            return Err(RefreshError::Unauthorized {
                status,
                description: "GitHub token rejected at copilot-token exchange.".to_owned(),
            });
        }
        if status == 200 {
            return match serde_json::from_slice::<CopilotTokenResponse>(&bytes) {
                Ok(parsed) => match copilot_auth(parsed, github_token) {
                    Ok(auth) => {
                        tracing::info!(scope_key = cfg.scope_key, "auth: copilot token re-minted");
                        Ok(auth)
                    }
                    Err(e) => Err(RefreshError::Fatal {
                        status,
                        description: format!("{e}"),
                    }),
                },
                Err(e) => Err(RefreshError::Fatal {
                    status,
                    description: format!("malformed copilot token payload: {e}"),
                }),
            };
        }
        let description = format!("copilot-token exchange failed (HTTP {status}).");
        if RETRYABLE_EXCHANGE_STATUSES.contains(&status) {
            last_error = description;
            continue;
        }
        return Err(RefreshError::Fatal {
            status,
            description,
        });
    }
    Err(RefreshError::Exhausted { last_error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use kigi_models::COPILOT_OAUTH_CONFIG;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A COPILOT_OAUTH_CONFIG pointed at a mock server for both stages.
    fn mock_cfg(host: &'static str, exchange: &'static str) -> OAuthConfig {
        OAuthConfig {
            auth_host: host,
            token_host: host,
            copilot_exchange: Some((exchange, "/copilot_internal/v2/token")),
            ..COPILOT_OAUTH_CONFIG
        }
    }

    /// GitHub's device poll returns pending errors in a 200 body — mapped to
    /// Pending (authorization_pending / slow_down) and Expired (expired_token),
    /// never mis-read as a token.
    #[tokio::test]
    async fn github_device_poll_maps_200_body_errors() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .and(body_string_contains("grant_type=urn"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "authorization_pending" })),
            )
            .mount(&server)
            .await;
        let result = poll_github_device_token(&mock_cfg(host, host), "dev-1")
            .await
            .unwrap();
        assert!(
            matches!(result, DevicePollResult::Pending { error, .. } if error == "authorization_pending")
        );
    }

    #[tokio::test]
    async fn github_device_poll_expired_restarts() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "error": "expired_token" })),
            )
            .mount(&server)
            .await;
        let result = poll_github_device_token(&mock_cfg(host, host), "dev-1")
            .await
            .unwrap();
        assert!(matches!(result, DevicePollResult::Expired));
    }

    /// A successful poll yields the DURABLE github token as `key` with NO
    /// refresh token / expiry (the copilot exchange finalizes it next).
    #[tokio::test]
    async fn github_device_poll_success_is_bare_github_token() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access_token": "gho_github_tok" })),
            )
            .mount(&server)
            .await;
        let DevicePollResult::Success(auth) = poll_github_device_token(&mock_cfg(host, host), "d")
            .await
            .unwrap()
        else {
            panic!("expected success");
        };
        assert_eq!(auth.key, "gho_github_tok");
        assert_eq!(
            auth.refresh_token, None,
            "github token is not a refresh grant"
        );
        assert_eq!(auth.expires_at, None, "the github token is long-lived");
    }

    /// The Stage-2 exchange rides the github Bearer + editor headers and maps
    /// `{token, expires_at}` onto `key=copilot`, `refresh_token=github`, with a
    /// future `expires_at`.
    #[tokio::test]
    async fn copilot_exchange_maps_token_and_persists_github_as_refresh() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        let future = (Utc::now() + Duration::minutes(30)).timestamp();
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("Authorization", "Bearer gho_github_tok"))
            .and(header("Editor-Version", "vscode/1.107.0"))
            .and(header("Copilot-Integration-Id", "vscode-chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "token": "tid=abc;copilot-tok", "expires_at": future }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let auth = exchange_copilot_token(&mock_cfg(host, host), "gho_github_tok")
            .await
            .unwrap();
        assert_eq!(auth.key, "tid=abc;copilot-tok", "key = copilot token");
        assert_eq!(
            auth.refresh_token.as_deref(),
            Some("gho_github_tok"),
            "the durable github token is persisted as refresh_token"
        );
        assert!(
            auth.expires_at.is_some_and(|e| e > Utc::now()),
            "copilot expiry must be in the future"
        );
    }

    /// The copilot re-mint (refresh) re-exchanges the github token for a NEW
    /// copilot token, keeping the github token as refresh_token.
    #[tokio::test]
    async fn copilot_remint_returns_new_copilot_token() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        let future = (Utc::now() + Duration::minutes(30)).timestamp();
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("Authorization", "Bearer gho_github_tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "token": "copilot-tok-2", "expires_at": future }),
            ))
            .mount(&server)
            .await;
        let auth = remint_copilot_token(&mock_cfg(host, host), "gho_github_tok")
            .await
            .unwrap();
        assert_eq!(auth.key, "copilot-tok-2");
        assert_eq!(auth.refresh_token.as_deref(), Some("gho_github_tok"));
    }

    /// A 401 at the exchange (github token revoked) fails fast as Unauthorized
    /// (drives the manager's permanent-failure / re-login path).
    #[tokio::test]
    async fn copilot_remint_401_is_unauthorized() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = remint_copilot_token(&mock_cfg(host, host), "dead-github-tok")
            .await
            .unwrap_err();
        assert!(
            matches!(err, RefreshError::Unauthorized { status: 401, .. }),
            "got {err:?}"
        );
    }

    /// FAIL-FAST: an out-of-range `expires_at` is rejected rather than silently
    /// degrading to a long default TTL (which would 401 on the wire ~30 min in).
    #[tokio::test]
    async fn copilot_exchange_rejects_out_of_range_expiry() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "token": "copilot-tok", "expires_at": i64::MAX }),
            ))
            .mount(&server)
            .await;
        let err = exchange_copilot_token(&mock_cfg(host, host), "gho_github_tok")
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("out-of-range expires_at"),
            "got {err}"
        );
    }
}
