//! Kimi Code OAuth wire protocol (PRD F1).
//!
//! Three calls against `{host}` (= `kigi_env::oauth_host()`), all
//! `application/x-www-form-urlencoded` POSTs carrying the device-identity
//! headers from [`super::device`]:
//!
//! - `POST /api/oauth/device_authorization` — form `client_id`
//! - `POST /api/oauth/token` (poll) — form `client_id` + `device_code` +
//!   `grant_type=urn:ietf:params:oauth:grant-type:device_code`
//! - `POST /api/oauth/token` (refresh) — form `client_id` +
//!   `grant_type=refresh_token` + `refresh_token`, with exponential backoff
//!   over the retryable statuses {429, 500, 502, 503, 504} (3 tries) and
//!   401/403 mapped to [`RefreshError::Unauthorized`].
//!
//! Ported from kimi-cli `auth/oauth.py` (the authoritative reference).

use chrono::{Duration, Utc};
use serde::Deserialize;

use super::device::device_headers;
use super::model::{AuthMode, KimiAuth};

/// Kimi Code OAuth client id (fixed for the official device-flow client).
pub(crate) const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";

/// Refresh retry budget over the retryable statuses / network blips.
const MAX_REFRESH_RETRIES: u32 = 3;
/// HTTP statuses worth retrying a refresh for (kimi-cli parity).
const RETRYABLE_REFRESH_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

/// Result of `POST /api/oauth/device_authorization`.
#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub device_code: String,
    /// Bare verification page (may be absent; the complete URI is required).
    pub verification_uri: Option<String>,
    /// Verification page with the user code pre-filled — what we display
    /// and open in the browser.
    pub verification_uri_complete: String,
    /// Device-code lifetime; `None` when the server omits it.
    pub expires_in: Option<i64>,
    /// Poll interval in seconds (server default 5; floored at 1 by callers).
    pub interval: i64,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    user_code: String,
    device_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<i64>,
}

/// Successful token payload (device grant and refresh grant share it).
#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

impl TokenResponse {
    /// Materialize the credential: `expires_at = now + expires_in`.
    pub(crate) fn into_auth(self) -> KimiAuth {
        let now = Utc::now();
        KimiAuth {
            key: self.access_token,
            auth_mode: AuthMode::OAuth,
            create_time: now,
            user_id: String::new(),
            email: None,
            refresh_token: Some(self.refresh_token),
            expires_at: Some(now + Duration::seconds(self.expires_in)),
            expires_in: Some(self.expires_in),
            scope: self.scope,
            token_type: self.token_type,
        }
    }
}

#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// One poll tick against the token endpoint.
#[derive(Debug)]
pub(crate) enum DevicePollResult {
    /// 200 with an access token — login complete.
    Success(Box<KimiAuth>),
    /// `error == "expired_token"` — restart the whole device authorization.
    Expired,
    /// Any other non-200 outcome (`authorization_pending`, `slow_down`,
    /// unknown errors) — wait and poll again. `slow_down` additionally bumps
    /// the caller's interval.
    Pending {
        error: String,
        description: Option<String>,
    },
}

fn oauth_url(host: &str, path: &str) -> String {
    format!("{}{path}", host.trim_end_matches('/'))
}

fn with_device_headers(
    mut builder: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::RequestBuilder> {
    for (name, value) in device_headers()? {
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// Defend against control characters / non-https redirects from a
/// compromised or mis-configured OAuth host. Shared with the generic
/// device-code wire ([`super::oauth_device`]).
pub(crate) fn validate_verification_uri(uri: &str) -> anyhow::Result<()> {
    if uri.chars().any(|c| c.is_ascii_control()) {
        anyhow::bail!("Server returned invalid verification URI");
    }
    let parsed = url::Url::parse(uri)
        .map_err(|_| anyhow::anyhow!("Server returned invalid verification URI"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("localhost") | Some("127.0.0.1")) => Ok(()),
        _ => anyhow::bail!("Server returned unsupported verification URI scheme"),
    }
}

/// `POST {host}/api/oauth/device_authorization` — start a device login.
pub(crate) async fn request_device_authorization(
    host: &str,
) -> anyhow::Result<DeviceAuthorization> {
    let url = oauth_url(host, "/api/oauth/device_authorization");
    tracing::info!(url = %url, "auth: requesting device authorization");
    let resp = with_device_headers(crate::http::shared_client().post(&url))?
        .form(&[("client_id", KIMI_CODE_CLIENT_ID)])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, "auth: device authorization failed");
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
    validate_verification_uri(&parsed.verification_uri_complete)?;
    if let Some(ref uri) = parsed.verification_uri {
        validate_verification_uri(uri)?;
    }

    tracing::info!(
        user_code = %parsed.user_code,
        interval = parsed.interval.unwrap_or(5),
        expires_in = ?parsed.expires_in,
        "auth: device authorization issued"
    );
    Ok(DeviceAuthorization {
        user_code: parsed.user_code,
        device_code: parsed.device_code,
        verification_uri: parsed.verification_uri.filter(|u| !u.is_empty()),
        verification_uri_complete: parsed.verification_uri_complete,
        expires_in: parsed.expires_in.filter(|&e| e > 0),
        interval: parsed.interval.unwrap_or(5),
    })
}

/// One poll of `POST {host}/api/oauth/token` with the device grant.
///
/// 5xx and network/decode failures are errors (kimi-cli parity: the login
/// loop surfaces them); everything else maps onto [`DevicePollResult`].
pub(crate) async fn poll_device_token(
    host: &str,
    device_code: &str,
) -> anyhow::Result<DevicePollResult> {
    let url = oauth_url(host, "/api/oauth/token");
    let resp = with_device_headers(crate::http::shared_client().post(&url))?
        .form(&[
            ("client_id", KIMI_CODE_CLIENT_ID),
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
            tracing::info!("auth: device poll succeeded, access token issued");
            return Ok(DevicePollResult::Success(Box::new(tokens.into_auth())));
        }
        // 200 without an access token: treat as still-pending (kimi-cli
        // requires "access_token" in the payload before accepting).
        tracing::warn!("auth: device poll returned 200 without access_token; continuing");
        return Ok(DevicePollResult::Pending {
            error: "missing_access_token".to_owned(),
            description: None,
        });
    }
    let err: OAuthErrorBody = serde_json::from_slice(&body).unwrap_or_default();
    let error = err.error.unwrap_or_else(|| "unknown_error".to_owned());
    if error == "expired_token" {
        tracing::info!("auth: device code expired; restarting device authorization");
        return Ok(DevicePollResult::Expired);
    }
    tracing::debug!(error = %error, "auth: device poll pending");
    Ok(DevicePollResult::Pending {
        error,
        description: err.error_description,
    })
}

/// Why a refresh call terminally or transiently failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RefreshError {
    /// 401/403 — the refresh token was rejected. Triggers the tombstone
    /// cooldown in the manager.
    #[error("token refresh unauthorized (HTTP {status}): {description}")]
    Unauthorized { status: u16, description: String },
    /// Non-retryable non-200 status.
    #[error("token refresh failed (HTTP {status}): {description}")]
    Fatal { status: u16, description: String },
    /// Retry budget exhausted over retryable statuses / network blips.
    #[error("token refresh failed after {MAX_REFRESH_RETRIES} attempts: {last_error}")]
    Exhausted { last_error: String },
    /// Local failure before the wire (e.g. device-id creation failed).
    #[error(transparent)]
    Local(#[from] anyhow::Error),
}

/// `POST {host}/api/oauth/token` with `grant_type=refresh_token`.
///
/// Retries the retryable statuses and network errors with exponential
/// backoff (`2^attempt` seconds); 401/403 returns immediately as
/// [`RefreshError::Unauthorized`].
pub(crate) async fn refresh_token(
    host: &str,
    refresh_token: &str,
) -> Result<KimiAuth, RefreshError> {
    let url = oauth_url(host, "/api/oauth/token");
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_REFRESH_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tracing::warn!(
                attempt,
                backoff_secs = backoff.as_secs(),
                last_error = %last_error,
                "auth: retrying token refresh"
            );
            tokio::time::sleep(backoff).await;
        }
        tracing::info!(attempt, "auth: token refresh attempt");
        let send_result = with_device_headers(crate::http::shared_client().post(&url))?
            .form(&[
                ("client_id", KIMI_CODE_CLIENT_ID),
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
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token_json(access: &str, refresh: &str) -> serde_json::Value {
        serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
            "scope": "kimi-code",
            "token_type": "bearer",
        })
    }

    #[tokio::test]
    async fn device_authorization_parses_wire_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .and(body_string_contains(format!(
                "client_id={KIMI_CODE_CLIENT_ID}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "WXYZ-6789",
                "device_code": "dev-code-1",
                "verification_uri": "https://www.kimi.com/code/authorize_device",
                "verification_uri_complete": "https://www.kimi.com/code/authorize_device?user_code=WXYZ-6789",
                "expires_in": 1800,
                "interval": 7,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let auth = request_device_authorization(&server.uri()).await.unwrap();
        assert_eq!(auth.user_code, "WXYZ-6789");
        assert_eq!(auth.device_code, "dev-code-1");
        assert_eq!(auth.interval, 7);
        assert_eq!(auth.expires_in, Some(1800));
        assert_eq!(
            auth.verification_uri_complete,
            "https://www.kimi.com/code/authorize_device?user_code=WXYZ-6789"
        );
    }

    #[tokio::test]
    async fn device_authorization_sends_device_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .and(wiremock::matchers::header_exists("X-Msh-Device-Name"))
            .and(wiremock::matchers::header_exists("X-Msh-Device-Model"))
            .and(wiremock::matchers::header_exists("X-Msh-Device-Id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "AAAA",
                "device_code": "d",
                "verification_uri_complete": "https://www.kimi.com/code/authorize_device?user_code=AAAA",
                "interval": 5,
            })))
            .expect(1)
            .mount(&server)
            .await;
        request_device_authorization(&server.uri()).await.unwrap();
    }

    #[tokio::test]
    async fn device_authorization_defaults_interval_to_five() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "AAAA",
                "device_code": "d",
                "verification_uri_complete": "https://www.kimi.com/code/authorize_device?user_code=AAAA",
            })))
            .mount(&server)
            .await;
        let auth = request_device_authorization(&server.uri()).await.unwrap();
        assert_eq!(auth.interval, 5);
        assert_eq!(auth.expires_in, None);
    }

    #[tokio::test]
    async fn device_authorization_rejects_bad_verification_uri() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "AAAA",
                "device_code": "d",
                "verification_uri_complete": "javascript:alert(1)",
            })))
            .mount(&server)
            .await;
        let err = request_device_authorization(&server.uri())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verification URI"), "{err}");
    }

    #[tokio::test]
    async fn device_authorization_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(400).set_body_string("nope"))
            .mount(&server)
            .await;
        let err = request_device_authorization(&server.uri())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "{err}");
    }

    #[tokio::test]
    async fn poll_success_builds_auth_with_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("grant_type=urn"))
            .and(body_string_contains("device_code=dev-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-1", "rt-1")))
            .expect(1)
            .mount(&server)
            .await;
        let result = poll_device_token(&server.uri(), "dev-1").await.unwrap();
        let DevicePollResult::Success(auth) = result else {
            panic!("expected success, got {result:?}");
        };
        assert_eq!(auth.key, "at-1");
        assert_eq!(auth.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(auth.expires_in, Some(3600));
        let remaining = auth.expires_at.unwrap() - chrono::Utc::now();
        assert!(
            (3590..=3600).contains(&remaining.num_seconds()),
            "expires_at must be ~now+expires_in, got {remaining:?}"
        );
        assert_eq!(auth.scope.as_deref(), Some("kimi-code"));
        assert_eq!(auth.token_type.as_deref(), Some("bearer"));
    }

    #[tokio::test]
    async fn poll_maps_expired_token_to_restart() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "expired_token" })),
            )
            .mount(&server)
            .await;
        let result = poll_device_token(&server.uri(), "dev-1").await.unwrap();
        assert!(matches!(result, DevicePollResult::Expired), "{result:?}");
    }

    #[tokio::test]
    async fn poll_maps_pending_and_unknown_errors_to_pending() {
        let server = MockServer::start().await;
        for error in ["authorization_pending", "slow_down", "surprise_error"] {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/api/oauth/token"))
                .respond_with(
                    ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": error })),
                )
                .mount(&server)
                .await;
            let result = poll_device_token(&server.uri(), "dev-1").await.unwrap();
            match result {
                DevicePollResult::Pending { error: got, .. } => assert_eq!(got, error),
                other => panic!("expected pending for {error}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn poll_server_error_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let err = poll_device_token(&server.uri(), "dev-1").await.unwrap_err();
        assert!(err.to_string().contains("server error"), "{err}");
    }

    #[tokio::test]
    async fn refresh_success_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-new", "rt-new")))
            .expect(1)
            .mount(&server)
            .await;
        let auth = refresh_token(&server.uri(), "rt-old").await.unwrap();
        assert_eq!(auth.key, "at-new");
        assert_eq!(auth.refresh_token.as_deref(), Some("rt-new"));
    }

    #[tokio::test]
    async fn refresh_401_maps_to_unauthorized_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(
                    serde_json::json!({ "error_description": "refresh token revoked" }),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = refresh_token(&server.uri(), "rt-dead").await.unwrap_err();
        match err {
            RefreshError::Unauthorized {
                status,
                description,
            } => {
                assert_eq!(status, 401);
                assert_eq!(description, "refresh token revoked");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_retries_retryable_status_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-2", "rt-2")))
            .expect(1)
            .mount(&server)
            .await;
        let auth = refresh_token(&server.uri(), "rt-old").await.unwrap();
        assert_eq!(auth.key, "at-2");
    }

    #[tokio::test]
    async fn refresh_exhausts_after_three_retryable_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        let err = refresh_token(&server.uri(), "rt-old").await.unwrap_err();
        assert!(matches!(err, RefreshError::Exhausted { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn refresh_non_retryable_status_is_fatal_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error_description": "bad request" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = refresh_token(&server.uri(), "rt-old").await.unwrap_err();
        match err {
            RefreshError::Fatal {
                status,
                description,
            } => {
                assert_eq!(status, 400);
                assert_eq!(description, "bad request");
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }
}
