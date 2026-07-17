//! Kimi Code token refresher: drives `POST /api/oauth/token` with
//! `grant_type=refresh_token` through the [`TokenRefresher`] seam.
//!
//! Ports kimi-cli `OAuthManager._refresh_tokens`' sibling-safety behavior:
//! the persisted credential is re-read before the wire call (adopt a
//! rotation instead of refreshing), and after a 401/403 the persisted
//! credential is re-read once more (with a 1s grace) so a concurrent
//! process's freshly rotated token is adopted instead of tombstoning it.

use std::sync::Arc;

use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::kimi_oauth::{self, RefreshError};
use crate::auth::manager::RefreshReason;

use super::{AuthSnapshot, RefreshOutcome, TokenRefresher};

/// Grace period after a 401/403 before concluding the refresh token is dead:
/// a concurrent instance may still be persisting its rotated token
/// (kimi-cli parity: `await asyncio.sleep(1)`).
const POST_UNAUTHORIZED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) struct KimiRefresher {
    auth: Arc<dyn AuthSnapshot>,
    /// OAuth host; `kigi_env::oauth_host()` in production, injectable for
    /// wiremock tests.
    host: String,
}

impl KimiRefresher {
    pub(crate) fn new(auth: Arc<dyn AuthSnapshot>, host: String) -> Self {
        Self { auth, host }
    }

    /// Post-401 sibling check (kimi-cli parity): wait a beat, re-read the
    /// persisted credential, and adopt it when its refresh token differs
    /// from the one the server just rejected.
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
                "adopted_rt_prefix": crate::auth::token_suffix(latest_rt),
                "rejected_rt_prefix": crate::auth::token_suffix(tried_rt),
            })),
        );
        Some(RefreshOutcome::success(latest))
    }
}

#[async_trait::async_trait]
impl TokenRefresher for KimiRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {
        tracing::info!(?reason, "auth: kimi refresh attempt starting");

        let disk_auth = self.auth.read_disk_auth();

        // Sibling short-circuit: a valid persisted token whose key differs
        // from in-memory means another process refreshed between the
        // refresh_chain disk check (under lock) and here. Adopt directly.
        if let Some(ref d) = disk_auth
            && !crate::auth::is_expired(d)
            && self.auth.current().map(|a| a.key).as_deref() != Some(&d.key)
        {
            kigi_log::unified_log::info(
                "auth.refresh.adopted_sibling_token",
                None,
                Some(serde_json::json!({
                    "disk_key_prefix": crate::auth::token_suffix(&d.key),
                })),
            );
            return RefreshOutcome::success(d.clone());
        }

        let Some(auth) = super::resolve_refresh_credential(self.auth.as_ref(), disk_auth, reason)
        else {
            tracing::warn!(?reason, "auth: no credential available for refresh");
            return RefreshOutcome::transient("no token with refresh_token available");
        };
        let Some(refresh_token) = auth.refresh_token.clone() else {
            tracing::warn!(?reason, "auth: resolved credential has no refresh token");
            return RefreshOutcome::transient("credential has no refresh token");
        };

        tracing::info!(
            rt_prefix = crate::auth::token_suffix(&refresh_token),
            expires_at = ?auth.expires_at,
            "auth: sending refresh_token grant"
        );

        match kimi_oauth::refresh_token(&self.host, &refresh_token).await {
            Ok(new_auth) => {
                kigi_log::unified_log::info(
                    "auth.refresh.token_rotated",
                    None,
                    Some(serde_json::json!({
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
                tracing::warn!(status, %description, "auth: refresh token rejected");
                if let Some(adopted) = self.adopt_rotation_after_unauthorized(&refresh_token).await
                {
                    return adopted;
                }
                kigi_log::unified_log::warn(
                    "auth.refresh.unauthorized",
                    None,
                    Some(serde_json::json!({
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
            // kimi-cli parity: non-401 failures never tombstone; the next
            // 60s tick (or pre-request check) retries.
            Err(
                e @ (RefreshError::Exhausted { .. }
                | RefreshError::Fatal { .. }
                | RefreshError::Local(_)),
            ) => {
                tracing::warn!(error = %e, "auth: refresh attempt failed (transient)");
                kigi_log::unified_log::warn(
                    "auth.refresh.transient_wire_failure",
                    None,
                    Some(serde_json::json!({ "error": format!("{e}") })),
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
    use parking_lot::Mutex;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Scriptable snapshot: `disk` can be swapped mid-test to simulate a
    /// sibling process rotating the persisted credential.
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

    fn valid_session(key: &str, rt: &str) -> KimiAuth {
        KimiAuth {
            key: key.into(),
            refresh_token: Some(rt.into()),
            expires_at: Some(Utc::now() + Duration::hours(2)),
            expires_in: Some(7200),
            ..KimiAuth::test_default()
        }
    }

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
    async fn refresh_success_returns_rotated_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .and(body_string_contains("refresh_token=rt-old"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_json("at-new", "rt-new")))
            .expect(1)
            .mount(&server)
            .await;
        let stale = expired_session("at-old", "rt-old");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let refresher = KimiRefresher::new(snap, server.uri());

        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        let RefreshOutcome::Success(new_auth) = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        assert_eq!(new_auth.key, "at-new");
        assert_eq!(new_auth.refresh_token.as_deref(), Some("rt-new"));
    }

    #[tokio::test]
    async fn adopts_valid_sibling_token_without_wire_call() {
        // Disk has a fresh token with a different key: adopt, no HTTP.
        let server = MockServer::start().await;
        // No mock mounted: any request would 404 and fail the refresh.
        let snap = FakeSnapshot::new(
            Some(expired_session("at-old", "rt-old")),
            Some(valid_session("at-sibling", "rt-sibling")),
        );
        let refresher = KimiRefresher::new(snap, server.uri());
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        let RefreshOutcome::Success(adopted) = outcome else {
            panic!("expected sibling adoption, got {outcome:?}");
        };
        assert_eq!(adopted.key, "at-sibling");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "sibling adoption must not consume a refresh token on the wire"
        );
    }

    #[tokio::test]
    async fn unauthorized_tombstones_the_tried_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({ "error_description": "revoked" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let stale = expired_session("at-old", "rt-dead");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let refresher = KimiRefresher::new(snap, server.uri());

        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        let RefreshOutcome::PermanentFailure {
            error,
            rejected_refresh_token,
        } = outcome
        else {
            panic!("expected permanent failure, got {outcome:?}");
        };
        assert_eq!(error.reason, RefreshTokenFailedReason::RefreshTokenRejected);
        assert_eq!(rejected_refresh_token.as_deref(), Some("rt-dead"));
    }

    #[tokio::test]
    async fn unauthorized_adopts_sibling_rotation_instead_of_tombstoning() {
        // 401 lands, but by the time we re-check, a sibling has persisted a
        // rotated credential — adopt it (the mutual-logout race guard).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let stale = expired_session("at-old", "rt-dead");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let refresher = KimiRefresher::new(snap.clone(), server.uri());

        // Swap the persisted credential while the wire call is in flight.
        let rotator = {
            let snap = snap.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                *snap.disk.lock() = Some(valid_session("at-rotated", "rt-rotated"));
            })
        };
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        rotator.await.unwrap();
        let RefreshOutcome::Success(adopted) = outcome else {
            panic!("expected rotation adoption, got {outcome:?}");
        };
        assert_eq!(adopted.key, "at-rotated");
    }

    #[tokio::test]
    async fn wire_exhaustion_is_transient_not_tombstoned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth/token"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        let stale = expired_session("at-old", "rt-old");
        let snap = FakeSnapshot::new(Some(stale.clone()), Some(stale));
        let refresher = KimiRefresher::new(snap, server.uri());
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        assert!(
            matches!(outcome, RefreshOutcome::TransientFailure { .. }),
            "5xx exhaustion must stay transient: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn no_credential_is_transient() {
        let server = MockServer::start().await;
        let snap = FakeSnapshot::new(None, None);
        let refresher = KimiRefresher::new(snap, server.uri());
        let outcome = refresher.refresh(RefreshReason::PreRequest).await;
        assert!(matches!(outcome, RefreshOutcome::TransientFailure { .. }));
    }
}
