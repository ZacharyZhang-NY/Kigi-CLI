//! Generic device-code token refresher: drives `POST {token_path}` with
//! `grant_type=refresh_token` for any [`kigi_models::OAuthConfig`] provider
//! (xai-grok today) through the [`TokenRefresher`] seam.
//!
//! Structurally identical to [`super::kimi_refresher::KimiRefresher`] — same
//! sibling-adoption + post-401 grace — but the wire call goes through
//! [`crate::auth::oauth_device`] (no X-Msh headers) instead of the Kimi wire.
//! Access/refresh tokens are NEVER logged.

use std::sync::Arc;

use kigi_models::{OAuthConfig, OAuthTokenBody};

use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::kimi_oauth::RefreshError;
use crate::auth::manager::RefreshReason;
use crate::auth::{github_copilot, oauth_device, oauth_pkce};

use super::{AuthSnapshot, RefreshOutcome, TokenRefresher};

/// Grace period after a 401/403 before concluding the refresh token is dead:
/// a concurrent instance may still be persisting its rotated token.
const POST_UNAUTHORIZED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) struct GenericDeviceRefresher {
    auth: Arc<dyn AuthSnapshot>,
    cfg: &'static OAuthConfig,
}

impl GenericDeviceRefresher {
    pub(crate) fn new(auth: Arc<dyn AuthSnapshot>, cfg: &'static OAuthConfig) -> Self {
        Self { auth, cfg }
    }

    /// Post-401 sibling check: wait a beat, re-read the persisted credential,
    /// and adopt it when its refresh token differs from the rejected one.
    async fn adopt_rotation_after_unauthorized(&self, tried_rt: &str) -> Option<RefreshOutcome> {
        tokio::time::sleep(POST_UNAUTHORIZED_GRACE).await;
        let latest = self.auth.read_disk_auth()?;
        let latest_rt = latest.refresh_token.as_deref()?;
        if latest_rt == tried_rt {
            return None;
        }
        kigi_log::unified_log::info(
            "auth.refresh.adopted_rotation_after_401",
            None,
            Some(serde_json::json!({
                "scope_key": self.cfg.scope_key,
                "adopted_rt_prefix": crate::auth::token_suffix(latest_rt),
                "rejected_rt_prefix": crate::auth::token_suffix(tried_rt),
            })),
        );
        Some(RefreshOutcome::success(latest))
    }
}

#[async_trait::async_trait]
impl TokenRefresher for GenericDeviceRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {
        tracing::info!(
            ?reason,
            scope_key = self.cfg.scope_key,
            "auth: generic refresh attempt"
        );

        let disk_auth = self.auth.read_disk_auth();

        // Sibling short-circuit: a valid persisted token whose key differs from
        // in-memory means another process refreshed already — adopt directly.
        if let Some(ref d) = disk_auth
            && !crate::auth::is_expired(d)
            && self.auth.current().map(|a| a.key).as_deref() != Some(&d.key)
        {
            kigi_log::unified_log::info(
                "auth.refresh.adopted_sibling_token",
                None,
                Some(serde_json::json!({
                    "scope_key": self.cfg.scope_key,
                    "disk_key_prefix": crate::auth::token_suffix(&d.key),
                })),
            );
            return RefreshOutcome::success(d.clone());
        }

        let Some(auth) = super::resolve_refresh_credential(self.auth.as_ref(), disk_auth, reason)
        else {
            tracing::warn!(
                ?reason,
                "auth: no credential available for refresh (generic)"
            );
            return RefreshOutcome::transient("no token with refresh_token available");
        };
        let Some(refresh_token) = auth.refresh_token.clone() else {
            tracing::warn!(
                ?reason,
                "auth: resolved credential has no refresh token (generic)"
            );
            return RefreshOutcome::transient("credential has no refresh token");
        };

        tracing::info!(
            rt_prefix = crate::auth::token_suffix(&refresh_token),
            expires_at = ?auth.expires_at,
            "auth: sending refresh_token grant (generic oauth)"
        );

        // Refresh over the provider's token-body encoding: xai's endpoint is
        // form-encoded (device wire); Claude's is JSON (PKCE wire); GitHub
        // Copilot's "refresh" is a copilot-token RE-MINT — a `GET
        // copilot_internal/v2/token` bearing the durable github token (the
        // `refresh_token` field here), NOT a refresh_token grant. All three
        // return the same `Result<KimiAuth, RefreshError>`.
        let wire_result = match self.cfg.token_body {
            OAuthTokenBody::Form => oauth_device::refresh_token(self.cfg, &refresh_token).await,
            OAuthTokenBody::Json => oauth_pkce::refresh_token(self.cfg, &refresh_token).await,
            OAuthTokenBody::GithubCopilotExchange => {
                github_copilot::remint_copilot_token(self.cfg, &refresh_token).await
            }
        };
        match wire_result {
            Ok(new_auth) => {
                kigi_log::unified_log::info(
                    "auth.refresh.token_rotated",
                    None,
                    Some(serde_json::json!({
                        "scope_key": self.cfg.scope_key,
                        "new_key_prefix": crate::auth::token_suffix(&new_auth.key),
                        "expires_at": new_auth.expires_at.map(|e| e.to_rfc3339()),
                    })),
                );
                RefreshOutcome::success(new_auth)
            }
            Err(RefreshError::Unauthorized {
                status,
                description,
            }) => {
                tracing::warn!(status, %description, "auth: refresh token rejected (generic)");
                if let Some(adopted) = self.adopt_rotation_after_unauthorized(&refresh_token).await
                {
                    return adopted;
                }
                kigi_log::unified_log::warn(
                    "auth.refresh.unauthorized",
                    None,
                    Some(serde_json::json!({
                        "scope_key": self.cfg.scope_key,
                        "status": status,
                        "description": description,
                        "rt_prefix": crate::auth::token_suffix(&refresh_token),
                    })),
                );
                RefreshOutcome::permanent(
                    RefreshTokenFailedReason::RefreshTokenRejected,
                    Some(refresh_token),
                )
            }
            Err(
                e @ (RefreshError::Exhausted { .. }
                | RefreshError::Fatal { .. }
                | RefreshError::Local(_)),
            ) => {
                tracing::warn!(error = %e, "auth: refresh attempt failed (transient, generic)");
                kigi_log::unified_log::warn(
                    "auth.refresh.transient_wire_failure",
                    None,
                    Some(serde_json::json!({
                        "scope_key": self.cfg.scope_key,
                        "error": format!("{e}"),
                    })),
                );
                RefreshOutcome::transient(format!("token refresh failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::model::KimiAuth;
    use chrono::{Duration, Utc};
    use kigi_models::XAI_OAUTH_CONFIG;
    use parking_lot::Mutex;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct FakeSnapshot {
        current: Mutex<Option<KimiAuth>>,
        disk: Mutex<Option<KimiAuth>>,
    }
    impl FakeSnapshot {
        fn new(current: Option<KimiAuth>, disk: Option<KimiAuth>) -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new(current),
                disk: Mutex::new(disk),
            })
        }
    }
    impl AuthSnapshot for FakeSnapshot {
        fn current(&self) -> Option<KimiAuth> {
            self.current
                .lock()
                .clone()
                .filter(|a| !crate::auth::is_expired(a))
        }
        fn expired_auth(&self) -> Option<KimiAuth> {
            self.current.lock().clone().filter(crate::auth::is_expired)
        }
        fn read_disk_auth(&self) -> Option<KimiAuth> {
            self.disk.lock().clone()
        }
        fn is_expired(&self) -> bool {
            self.current
                .lock()
                .as_ref()
                .is_some_and(crate::auth::is_expired)
        }
    }

    fn expired_session(key: &str, rt: &str) -> KimiAuth {
        KimiAuth {
            key: key.into(),
            refresh_token: Some(rt.into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            expires_in: Some(3600),
            ..KimiAuth::test_default()
        }
    }

    fn mock_cfg(host: &'static str) -> OAuthConfig {
        OAuthConfig {
            auth_host: host,
            ..XAI_OAUTH_CONFIG
        }
    }

    /// A successful refresh rotates the token via the generic wire.
    #[tokio::test]
    async fn refresh_success_returns_rotated_token() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(body_string_contains("refresh_token=grok-rt-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "grok-at-new",
                "refresh_token": "grok-rt-new",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let stale = expired_session("grok-at-old", "grok-rt-old");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let cfg: &'static OAuthConfig = Box::leak(Box::new(mock_cfg(host)));
        let refresher = GenericDeviceRefresher::new(snap, cfg);
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        let RefreshOutcome::Success(new_auth) = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        assert_eq!(new_auth.key, "grok-at-new");
        assert_eq!(new_auth.refresh_token.as_deref(), Some("grok-rt-new"));
    }

    /// A 401 on refresh (with no sibling rotation) tombstones the rejected
    /// refresh token as a permanent failure.
    #[tokio::test]
    async fn unauthorized_is_permanent_failure() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({ "error_description": "revoked" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let stale = expired_session("grok-at-old", "grok-rt-dead");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let cfg: &'static OAuthConfig = Box::leak(Box::new(mock_cfg(host)));
        let refresher = GenericDeviceRefresher::new(snap, cfg);
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        let RefreshOutcome::PermanentFailure {
            error,
            rejected_refresh_token,
        } = outcome
        else {
            panic!("expected permanent failure, got {outcome:?}");
        };
        assert_eq!(error.reason, RefreshTokenFailedReason::RefreshTokenRejected);
        assert_eq!(rejected_refresh_token.as_deref(), Some("grok-rt-dead"));
    }
}
