use crate::auth::AuthManager;
use crate::util::kigi_auth_credentials::KigiAuthCredentials;
use kigi_auth::{
    AuthCredentialProvider, CredentialSnapshot, HttpAuth, StaticAuthCredentialProvider,
};
use reqwest::RequestBuilder;
use std::sync::Arc;
/// `api_key.id` for the active credential: hash the stable API key, never the
/// OIDC bearer (which rotates). `None` for non-API-key auth.
fn api_key_id_for(auth: Option<&crate::auth::KimiAuth>) -> Option<String> {
    auth.filter(|a| matches!(a.auth_mode, crate::auth::AuthMode::ApiKey))
        .map(|a| crate::agent::config::deployment_id_from_key(&a.key))
}
/// Production impl: wraps the live `AuthManager`. 401 recovery
/// delegates to `AuthManager::unauthorized_recovery`.
pub struct ShellAuthCredentialProvider {
    auth_manager: Arc<AuthManager>,
    static_credentials: KigiAuthCredentials,
}
impl ShellAuthCredentialProvider {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        deployment_key: Option<String>,
        alpha_test_key: Option<String>,
    ) -> Self {
        let mut static_credentials = KigiAuthCredentials::new(None);
        static_credentials.deployment_key = deployment_key;
        static_credentials.alpha_test_key = alpha_test_key;
        Self {
            auth_manager,
            static_credentials,
        }
    }
}
impl std::fmt::Debug for ShellAuthCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellAuthCredentialProvider")
            .field("auth_manager", &"<configured>")
            .finish()
    }
}
impl HttpAuth for ShellAuthCredentialProvider {
    fn apply(&self, builder: RequestBuilder, base_url: &str) -> RequestBuilder {
        let mut creds = self.static_credentials.clone();
        if creds.deployment_key.is_none()
            && let Some(auth) = self.auth_manager.current_or_expired()
        {
            creds.user_token = Some(auth.key);
        }
        creds.apply(builder, base_url)
    }
}
#[async_trait::async_trait]
impl AuthCredentialProvider for ShellAuthCredentialProvider {
    fn snapshot(&self) -> CredentialSnapshot {
        if let Some(ref dk) = self.static_credentials.deployment_key {
            return CredentialSnapshot {
                token: Some(dk.clone()),
                deployment_id: crate::managed_config::resolve_deployment_id(Some(dk)),
                ..Default::default()
            };
        }
        let auth = self.auth_manager.current_or_expired();
        // The Kimi token response carries no account info; `user_id` stays
        // empty until a later feature surfaces it.
        let user_id = auth
            .as_ref()
            .map(|a| a.user_id.clone())
            .filter(|id| !id.is_empty());
        let api_key_id = api_key_id_for(auth.as_ref());
        let token = auth.map(|a| a.key);
        CredentialSnapshot {
            token,
            user_id,
            deployment_id: None,
            api_key_id,
        }
    }
    async fn refresh_after_unauthorized(&self) -> bool {
        if self.static_credentials.deployment_key.is_some() {
            return false;
        }
        self.auth_manager.try_recover_unauthorized().await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::KimiAuth;
    use crate::auth::KimiCodeConfig;
    use crate::auth::manager::AuthManager;
    use chrono::{Duration as ChronoDuration, Utc};
    use kigi_auth::AuthCredentialProvider;
    use std::sync::Mutex;
    /// Serializes tests that pin `KIGI_AUTH_EARLY_INVALIDATION_SECS`, since
    /// env vars are process-global and parallel tests would race.
    static EARLY_INVALIDATION_LOCK: Mutex<()> = Mutex::new(());
    /// RAII guard: pins `KIGI_AUTH_EARLY_INVALIDATION_SECS` to the production
    /// default (300s) while held, restoring the previous value on drop.
    /// Acquires `EARLY_INVALIDATION_LOCK` so concurrent test runners can't
    /// observe a half-mutated env.
    struct EarlyInvalidationGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }
    impl EarlyInvalidationGuard {
        fn pin_to_default() -> Self {
            let lock = EARLY_INVALIDATION_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("KIGI_AUTH_EARLY_INVALIDATION_SECS").ok();
            unsafe { std::env::set_var("KIGI_AUTH_EARLY_INVALIDATION_SECS", "300") };
            Self {
                _lock: lock,
                previous,
            }
        }
    }
    impl Drop for EarlyInvalidationGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(prev) => std::env::set_var("KIGI_AUTH_EARLY_INVALIDATION_SECS", prev),
                    None => std::env::remove_var("KIGI_AUTH_EARLY_INVALIDATION_SECS"),
                }
            }
        }
    }
    fn make_auth(key: &str, expires_in: ChronoDuration) -> KimiAuth {
        KimiAuth {
            key: key.to_string(),
            user_id: "test-user".to_string(),
            create_time: Utc::now(),
            expires_at: Some(Utc::now() + expires_in),
            ..KimiAuth::test_default()
        }
    }
    /// Build an `AuthManager` rooted at `dir`. Caller keeps `dir` alive for
    /// the duration of the test so the `TempDir` `Drop` actually cleans up.
    fn make_manager(dir: &tempfile::TempDir, initial: Option<KimiAuth>) -> Arc<AuthManager> {
        let mgr = AuthManager::new(dir.path(), KimiCodeConfig::default());
        if let Some(auth) = initial {
            mgr.hot_swap(auth);
        }
        Arc::new(mgr)
    }
    /// `apply()` and `snapshot()` agree (snapshot==wire invariant) when the
    /// in-memory token is fresh.
    #[test]
    fn apply_and_snapshot_agree_on_live_token() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_manager(
            &dir,
            Some(make_auth("live-token", ChronoDuration::hours(1))),
        );
        let provider = ShellAuthCredentialProvider::new(mgr, None, None);
        let snap = provider.snapshot();
        assert_eq!(snap.token.as_deref(), Some("live-token"));
        assert_eq!(snap.user_id.as_deref(), Some("test-user"));
    }
    /// During the 5-minute pre-refresh buffer window, `auth_manager.current()`
    /// returns `None` (the token is treated as expired-soon for refresh
    /// scheduling), but the token is still valid at the proxy. The provider
    /// must fall back to `expired_auth()` so the in-memory token gets sent
    /// instead of nothing -- which is the fix for the bulk of the
    /// `POST /v1/storage` 401s observed in production.
    #[test]
    fn falls_back_to_expired_auth_during_buffer_window() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_manager(
            &dir,
            Some(make_auth("buffer-token", ChronoDuration::minutes(4))),
        );
        assert!(mgr.current().is_none(), "buffer-window precondition");
        assert!(mgr.expired_auth().is_some(), "buffer-window precondition");
        let provider = ShellAuthCredentialProvider::new(mgr, None, None);
        let snap = provider.snapshot();
        assert_eq!(
            snap.token.as_deref(),
            Some("buffer-token"),
            "snapshot should fall back to expired_auth instead of None"
        );
        assert_eq!(snap.user_id.as_deref(), Some("test-user"));
    }
    /// When `auth_manager` has nothing at all (no in-memory auth, expired
    /// or otherwise), `snapshot()` returns `None` for the user-token branch.
    /// `apply()` would then send no Authorization header.
    #[test]
    fn no_token_when_auth_manager_is_empty() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_manager(&dir, None);
        let provider = ShellAuthCredentialProvider::new(mgr, None, None);
        let snap = provider.snapshot();
        assert!(
            snap.token.is_none(),
            "snapshot should be None when manager has no auth"
        );
        assert!(snap.user_id.is_none());
    }
    /// 401 recovery routes through `unauthorized_recovery` (pre-fix
    /// it no-oped because the refresher arg was hardcoded `None`).
    #[tokio::test]
    async fn refresh_after_unauthorized_drives_recovery_state_machine() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(AuthManager::new(
            dir.path(),
            crate::auth::KimiCodeConfig::default(),
        ));
        mgr.hot_swap(KimiAuth {
            key: "stale".into(),
            auth_mode: crate::auth::AuthMode::OAuth,
            create_time: chrono::Utc::now() - ChronoDuration::hours(2),
            user_id: "u".into(),
            refresh_token: Some("rt-stale".into()),
            expires_at: Some(chrono::Utc::now() - ChronoDuration::hours(1)),
            ..KimiAuth::test_default()
        });
        struct OkRefresher {
            calls: Arc<std::sync::atomic::AtomicU32>,
        }
        #[async_trait::async_trait]
        impl crate::auth::refresh::TokenRefresher for OkRefresher {
            async fn refresh(
                &self,
                _r: crate::auth::manager::RefreshReason,
            ) -> crate::auth::refresh::RefreshOutcome {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::auth::refresh::RefreshOutcome::Success(Box::new(KimiAuth {
                    key: "fresh".into(),
                    auth_mode: crate::auth::AuthMode::OAuth,
                    create_time: chrono::Utc::now(),
                    user_id: "u".into(),
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + ChronoDuration::hours(1)),
                    ..KimiAuth::test_default()
                }))
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        mgr.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let provider = ShellAuthCredentialProvider::new(mgr.clone(), None, None);
        assert!(provider.refresh_after_unauthorized().await);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(mgr.current().unwrap().key, "fresh");
        assert_eq!(
            provider.snapshot().token.as_deref(),
            Some("fresh"),
            "snapshot must reflect refreshed token for subsequent apply() calls"
        );
    }
    /// Deployment-key path has no recovery (operator owns the bearer).
    #[tokio::test]
    async fn refresh_after_unauthorized_is_noop_for_deployment_key() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_manager(&dir, None);
        let provider =
            ShellAuthCredentialProvider::new(mgr, Some("deployment-key".to_string()), None);
        assert!(!provider.refresh_after_unauthorized().await);
    }
    #[test]
    fn snapshot_populates_tenant_id_per_auth_mode() {
        use crate::agent::config::deployment_id_from_key;
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let dep = ShellAuthCredentialProvider::new(
            make_manager(&dir, None),
            Some("xai-token-EX".into()),
            None,
        )
        .snapshot();
        assert_eq!(
            dep.deployment_id.as_deref(),
            Some(deployment_id_from_key("xai-token-EX").as_str())
        );
        assert!(dep.api_key_id.is_none());
        let api_auth = KimiAuth {
            key: "sk-apikey-xyz".into(),
            auth_mode: crate::auth::AuthMode::ApiKey,
            expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
            ..KimiAuth::test_default()
        };
        let api = ShellAuthCredentialProvider::new(make_manager(&dir, Some(api_auth)), None, None)
            .snapshot();
        assert_eq!(
            api.api_key_id.as_deref(),
            Some(deployment_id_from_key("sk-apikey-xyz").as_str())
        );
        assert!(api.deployment_id.is_none());
        let oidc = ShellAuthCredentialProvider::new(
            make_manager(
                &dir,
                Some(make_auth("oidc-token", ChronoDuration::hours(1))),
            ),
            None,
            None,
        )
        .snapshot();
        assert!(oidc.deployment_id.is_none() && oidc.api_key_id.is_none());
    }
    /// Bootstrap mode: `snapshot()` re-reads disk so sibling-rotated
    /// tokens are picked up without a live AuthManager.
    #[test]
    fn deployment_key_wins_over_resolved_user_token() {
        let _guard = EarlyInvalidationGuard::pin_to_default();
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_manager(
            &dir,
            Some(make_auth("user-token", ChronoDuration::hours(1))),
        );
        let provider =
            ShellAuthCredentialProvider::new(mgr, Some("deployment-key-12345".to_string()), None);
        let snap = provider.snapshot();
        assert_eq!(snap.token.as_deref(), Some("deployment-key-12345"));
        assert!(snap.user_id.is_none());
    }
}
