//! Generic RFC-8628 device-code OAuth wire, driven by a registry
//! [`kigi_models::OAuthConfig`] (xai-grok today; Copilot/Claude later).
//!
//! Three `application/x-www-form-urlencoded` POSTs against `{auth_host}`:
//!
//! - `POST {device_path}` — form `client_id` + `scope` + the optional
//!   `extra_device_field` (e.g. `referrer=kigi`)
//! - `POST {token_path}` (poll) — form `client_id` + `device_code` +
//!   `grant_type=urn:ietf:params:oauth:grant-type:device_code`
//! - `POST {token_path}` (refresh) — form `client_id` +
//!   `grant_type=refresh_token` + `refresh_token`, with the same exponential
//!   backoff / status handling as the Kimi wire.
//!
//! Unlike [`super::kimi_oauth`] this sends NO X-Msh device headers — just the
//! shared kigi `User-Agent` and `Accept: application/json`. Access/refresh
//! tokens are NEVER logged (only non-secret events: requested, poll succeeded,
//! refreshed).

use kigi_models::OAuthConfig;
use serde::Deserialize;

use super::kimi_oauth::{
    DeviceAuthorization, DevicePollResult, RefreshError, TokenResponse, validate_verification_uri,
};

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";
const MAX_REFRESH_RETRIES: u32 = 3;
/// HTTP statuses worth retrying a refresh for (kimi-cli parity).
const RETRYABLE_REFRESH_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    user_code: String,
    device_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    /// Optional here (the Kimi wire requires it): Pi's xAI response may omit
    /// `verification_uri_complete` and carry only `verification_uri`.
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<i64>,
}

#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn oauth_url(host: &str, path: &str) -> String {
    format!("{}{path}", host.trim_end_matches('/'))
}

fn device_form(cfg: &OAuthConfig) -> Vec<(&'static str, &'static str)> {
    let mut form = vec![("client_id", cfg.client_id), ("scope", cfg.scope)];
    if let Some((name, value)) = cfg.extra_device_field {
        form.push((name, value));
    }
    form
}

/// `POST {auth_host}{device_path}` — start a device login.
pub(crate) async fn request_device_authorization(
    cfg: &OAuthConfig,
) -> anyhow::Result<DeviceAuthorization> {
    let url = oauth_url(cfg.auth_host, cfg.device_path);
    tracing::info!(url = %url, "auth: requesting device authorization (generic oauth)");
    let resp = crate::http::shared_client()
        .post(&url)
        .header("Accept", "application/json")
        .form(&device_form(cfg))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, "auth: device authorization failed (generic oauth)");
        anyhow::bail!("Device authorization failed (HTTP {status}): {body}");
    }
    let parsed: DeviceAuthorizationResponse = resp.json().await?;

    if !parsed
        .user_code
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        anyhow::bail!("Server returned invalid user_code format (expected [A-Z0-9-])");
    }
    // Pi forces the displayed URI to https; we require a valid https (or
    // localhost) verification target, preferring the pre-filled complete form.
    let verification_uri_complete = parsed
        .verification_uri_complete
        .clone()
        .or_else(|| parsed.verification_uri.clone())
        .ok_or_else(|| anyhow::anyhow!("Server returned no verification URI"))?;
    validate_verification_uri(&verification_uri_complete)?;
    if let Some(ref uri) = parsed.verification_uri {
        validate_verification_uri(uri)?;
    }

    tracing::info!(
        user_code = %parsed.user_code,
        interval = parsed.interval.unwrap_or(5),
        expires_in = ?parsed.expires_in,
        "auth: device authorization issued (generic oauth)"
    );
    Ok(DeviceAuthorization {
        user_code: parsed.user_code,
        device_code: parsed.device_code,
        verification_uri: parsed.verification_uri.filter(|u| !u.is_empty()),
        verification_uri_complete,
        expires_in: parsed.expires_in.filter(|&e| e > 0),
        interval: parsed.interval.unwrap_or(5),
    })
}

/// One poll of `POST {auth_host}{token_path}` with the device grant.
pub(crate) async fn poll_device_token(
    cfg: &OAuthConfig,
    device_code: &str,
) -> anyhow::Result<DevicePollResult> {
    let url = oauth_url(cfg.auth_host, cfg.token_path);
    let resp = crate::http::shared_client()
        .post(&url)
        .header("Accept", "application/json")
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
    if status.is_success() {
        if let Ok(tokens) = serde_json::from_slice::<TokenResponse>(&body) {
            tracing::info!("auth: device poll succeeded, access token issued (generic oauth)");
            return Ok(DevicePollResult::Success(Box::new(tokens.into_auth())));
        }
        tracing::warn!(
            "auth: device poll returned 200 without access_token; continuing (generic oauth)"
        );
        return Ok(DevicePollResult::Pending {
            error: "missing_access_token".to_owned(),
            description: None,
        });
    }
    let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
    let error = err.error.unwrap_or_else(|| "unknown_error".to_owned());
    if error == "expired_token" {
        tracing::info!(
            "auth: device code expired; restarting device authorization (generic oauth)"
        );
        return Ok(DevicePollResult::Expired);
    }
    tracing::debug!(error = %error, "auth: device poll pending (generic oauth)");
    Ok(DevicePollResult::Pending {
        error,
        description: err.error_description,
    })
}

/// `POST {auth_host}{token_path}` with `grant_type=refresh_token`. Retries the
/// retryable statuses / network errors with exponential backoff; 401/403
/// returns immediately as [`RefreshError::Unauthorized`].
pub(crate) async fn refresh_token(
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<super::model::KimiAuth, RefreshError> {
    let url = oauth_url(cfg.auth_host, cfg.token_path);
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_REFRESH_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tracing::warn!(
                attempt,
                backoff_secs = backoff.as_secs(),
                last_error = %last_error,
                "auth: retrying token refresh (generic oauth)"
            );
            tokio::time::sleep(backoff).await;
        }
        tracing::info!(attempt, "auth: token refresh attempt (generic oauth)");
        let send_result = crate::http::shared_client()
            .post(&url)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", cfg.client_id),
                ("grant_type", REFRESH_GRANT_TYPE),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await;

        let resp = match send_result {
            Ok(resp) => resp,
            Err(e) => {
                last_error = format!("network error: {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default();
        if status == 401 || status == 403 {
            let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
            return Err(RefreshError::Unauthorized {
                status,
                description: err
                    .error_description
                    .unwrap_or_else(|| "Token refresh unauthorized.".to_owned()),
            });
        }
        if status == 200 {
            return match serde_json::from_slice::<TokenResponse>(&body) {
                Ok(tokens) => Ok(tokens.into_auth()),
                Err(e) => Err(RefreshError::Fatal {
                    status,
                    description: format!("malformed token payload: {e}"),
                }),
            };
        }
        let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
        let description = err
            .error_description
            .unwrap_or_else(|| format!("Token refresh failed (HTTP {status})."));
        if RETRYABLE_REFRESH_STATUSES.contains(&status) {
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
    use kigi_models::XAI_OAUTH_CONFIG;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_cfg(host: &'static str) -> OAuthConfig {
        OAuthConfig {
            auth_host: host,
            ..XAI_OAUTH_CONFIG
        }
    }

    fn token_json(access: &str, refresh: &str) -> serde_json::Value {
        serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
            "scope": "grok-cli:access",
            "token_type": "bearer",
        })
    }

    #[tokio::test]
    async fn device_authorization_sends_client_scope_and_referrer() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/device/code"))
            .and(body_string_contains(
                "client_id=b1a00492-073a-47ea-816f-4c329264a828",
            ))
            .and(body_string_contains("scope=openid"))
            .and(body_string_contains("referrer=kigi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "GROK-1234",
                "device_code": "dev-xai-1",
                "verification_uri": "https://x.ai/device",
                "verification_uri_complete": "https://x.ai/device?user_code=GROK-1234",
                "expires_in": 900,
                "interval": 5,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let auth = request_device_authorization(&mock_cfg(host)).await.unwrap();
        assert_eq!(auth.user_code, "GROK-1234");
        assert_eq!(auth.device_code, "dev-xai-1");
        assert_eq!(
            auth.verification_uri_complete,
            "https://x.ai/device?user_code=GROK-1234"
        );
        assert_eq!(auth.expires_in, Some(900));
    }

    #[tokio::test]
    async fn device_authorization_falls_back_to_verification_uri() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "GROK-9",
                "device_code": "d",
                "verification_uri": "https://x.ai/device",
            })))
            .mount(&server)
            .await;
        let auth = request_device_authorization(&mock_cfg(host)).await.unwrap();
        assert_eq!(auth.verification_uri_complete, "https://x.ai/device");
        assert_eq!(auth.interval, 5, "default interval");
    }

    #[tokio::test]
    async fn poll_success_builds_auth_with_expiry() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains("grant_type=urn"))
            .and(body_string_contains("device_code=dev-xai-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_json("grok-at", "grok-rt")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = poll_device_token(&mock_cfg(host), "dev-xai-1")
            .await
            .unwrap();
        let DevicePollResult::Success(auth) = result else {
            panic!("expected success, got {result:?}");
        };
        assert_eq!(auth.key, "grok-at");
        assert_eq!(auth.refresh_token.as_deref(), Some("grok-rt"));
        assert_eq!(auth.expires_in, Some(3600));
    }

    #[tokio::test]
    async fn poll_maps_authorization_pending_to_pending() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "authorization_pending" })),
            )
            .mount(&server)
            .await;
        let result = poll_device_token(&mock_cfg(host), "dev-xai-1")
            .await
            .unwrap();
        match result {
            DevicePollResult::Pending { error, .. } => assert_eq!(error, "authorization_pending"),
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_success_round_trip() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=grok-rt-old"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(token_json("grok-at-new", "grok-rt-new")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let auth = refresh_token(&mock_cfg(host), "grok-rt-old").await.unwrap();
        assert_eq!(auth.key, "grok-at-new");
        assert_eq!(auth.refresh_token.as_deref(), Some("grok-rt-new"));
    }

    #[tokio::test]
    async fn refresh_401_maps_to_unauthorized() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({ "error_description": "refresh revoked" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = refresh_token(&mock_cfg(host), "grok-rt-dead")
            .await
            .unwrap_err();
        match err {
            RefreshError::Unauthorized {
                status,
                description,
            } => {
                assert_eq!(status, 401);
                assert_eq!(description, "refresh revoked");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }
}
