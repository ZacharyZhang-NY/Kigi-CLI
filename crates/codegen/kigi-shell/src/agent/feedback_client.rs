//! Feedback client for the Kimi Code platform.
//!
//! Port of kimi-cli's `/feedback` slash command (kimi-cli
//! `src/kimi_cli/ui/shell/slash.py`, `feedback()`): subscription (OAuth)
//! sessions POST the user's feedback text to `{coding_api_base_url}/feedback`
//! with a Bearer token; everyone else is pointed at the GitHub issue tracker.
//! The request body carries exactly the fields kimi-cli sends:
//! `session_id`, `content`, `version`, `os`, `model`.

use std::sync::Arc;

use serde::Serialize;

/// Where non-subscription users (no OAuth session) submit feedback instead.
pub const FEEDBACK_ISSUES_URL: &str = "https://github.com/ZacharyZhang-NY/Kigi-CLI/issues";

/// HTTP error from the feedback endpoint with a preserved status code, so
/// callers can distinguish auth failures (401) without string matching.
#[derive(Debug, thiserror::Error)]
#[error("feedback submission failed with status {status}: {body}")]
pub struct FeedbackApiError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl FeedbackApiError {
    /// Returns `true` if this is a 401 Unauthorized response.
    pub fn is_unauthorized(&self) -> bool {
        self.status == reqwest::StatusCode::UNAUTHORIZED
    }
}

/// JSON body for `POST {base}/feedback` — field names exactly as kimi-cli
/// sends them (slash.py `payload = {...}`).
#[derive(Debug, Clone, Serialize)]
struct FeedbackPayload<'a> {
    session_id: &'a str,
    content: &'a str,
    version: &'a str,
    os: &'a str,
    model: Option<&'a str>,
}

/// Bearer-token source for the feedback POST: the live OAuth session by
/// default, or a fixed token in tests.
#[derive(Clone)]
enum BearerSource {
    AuthManager(Arc<crate::auth::AuthManager>),
    Static(String),
}

/// Client for the Kimi Code feedback endpoint.
#[derive(Clone)]
pub struct FeedbackClient {
    http: reqwest::Client,
    base_url: String,
    bearer: BearerSource,
    session_id: Option<String>,
}

impl FeedbackClient {
    /// Client bound to the live OAuth session. Callers gate construction on
    /// an existing session auth (see `MvpAgent::feedback_client`).
    pub(crate) fn new(
        base_url: impl Into<String>,
        auth_manager: Arc<crate::auth::AuthManager>,
    ) -> Self {
        Self {
            http: crate::http::shared_client(),
            base_url: base_url.into(),
            bearer: BearerSource::AuthManager(auth_manager),
            session_id: None,
        }
    }

    /// Client with a fixed Bearer token (tests).
    #[cfg(test)]
    pub(crate) fn with_static_token(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: crate::http::shared_client(),
            base_url: base_url.into(),
            bearer: BearerSource::Static(token.into()),
            session_id: None,
        }
    }

    /// Default session id used when a submission doesn't carry one.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    fn bearer_token(&self) -> Option<String> {
        match &self.bearer {
            BearerSource::AuthManager(am) => am
                .current_or_expired()
                .filter(|a| a.is_session_auth())
                .map(|a| a.key.clone()),
            BearerSource::Static(token) => Some(token.clone()),
        }
    }

    /// `POST {base}/feedback` (kimi-cli slash.py parity). `model` is the
    /// active model key, when known. On 401 the OAuth session is refreshed
    /// once and the request retried.
    pub async fn submit_feedback(
        &self,
        session_id: &str,
        content: &str,
        model: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let sid = if session_id.is_empty() {
            self.session_id.as_deref().unwrap_or_default()
        } else {
            session_id
        };
        match self.post_feedback(sid, content, model).await {
            Err(e)
                if e.downcast_ref::<FeedbackApiError>()
                    .is_some_and(FeedbackApiError::is_unauthorized)
                    && self.try_refresh_credentials().await =>
            {
                self.post_feedback(sid, content, model).await
            }
            other => other,
        }
    }

    async fn post_feedback(
        &self,
        session_id: &str,
        content: &str,
        model: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let token = self
            .bearer_token()
            .ok_or_else(|| anyhow::anyhow!("no OAuth session token for feedback submission"))?;
        let url = format!("{}/feedback", self.base_url.trim_end_matches('/'));
        let payload = FeedbackPayload {
            session_id,
            content,
            version: kigi_version::VERSION,
            os: crate::auth::device::device_model(),
            model,
        };
        let response = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            tracing::info!(session_id = %session_id, "feedback submitted");
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        tracing::warn!(status = status.as_u16(), "feedback submission rejected");
        Err(FeedbackApiError { status, body }.into())
    }

    async fn try_refresh_credentials(&self) -> bool {
        match &self.bearer {
            BearerSource::AuthManager(am) => am.try_recover_unauthorized().await,
            BearerSource::Static(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{bearer_token, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Happy path: POST /feedback with Bearer and the exact kimi-cli body
    /// fields succeeds.
    #[tokio::test]
    async fn submit_feedback_posts_kimi_body_with_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/feedback"))
            .and(bearer_token("tok-123"))
            .and(body_partial_json(serde_json::json!({
                "session_id": "sess-1",
                "content": "love it",
                "version": kigi_version::VERSION,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = FeedbackClient::with_static_token(server.uri(), "tok-123");
        client
            .submit_feedback("sess-1", "love it", Some("kimi-k2"))
            .await
            .expect("feedback should succeed");
    }

    /// The body carries the `os` and `model` fields kimi-cli sends.
    #[tokio::test]
    async fn submit_feedback_includes_os_and_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/feedback"))
            .and(body_partial_json(serde_json::json!({
                "model": "kimi-k2",
                "os": crate::auth::device::device_model(),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = FeedbackClient::with_static_token(server.uri(), "tok");
        client
            .submit_feedback("sess-2", "hi", Some("kimi-k2"))
            .await
            .expect("feedback should succeed");
    }

    /// Auth failure: a 401 surfaces as `FeedbackApiError::is_unauthorized`
    /// (static-token clients cannot refresh, so no retry).
    #[tokio::test]
    async fn submit_feedback_auth_failure_is_typed_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/feedback"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": "invalid token"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = FeedbackClient::with_static_token(server.uri(), "bad-token");
        let err = client
            .submit_feedback("sess-3", "hello", None)
            .await
            .expect_err("401 must fail");
        let api_err = err
            .downcast_ref::<FeedbackApiError>()
            .expect("typed FeedbackApiError");
        assert!(api_err.is_unauthorized());
        assert!(api_err.body.contains("invalid token"));
    }
}
