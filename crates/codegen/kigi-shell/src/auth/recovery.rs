//! Unauthorized (401) recovery state machine.
//!
//! When the server rejects a token, `UnauthorizedRecovery` walks through
//! a sequence of recovery steps before giving up:
//!
//! 1. **ReloadFromDisk** — re-read the persisted credential under a file
//!    lock; if it differs from the rejected one, accept it (another process
//!    may have refreshed).
//! 2. **RefreshFromAuthority** — run the refresh chain against the Kimi
//!    OAuth host, unless the live token was minted moments ago (fresh-mint
//!    guard).
//! 3. **Done** — all recovery strategies exhausted.

use std::sync::Arc;

use crate::auth::error::{AuthError, RefreshTokenError, RefreshTokenFailedReason};
use crate::auth::manager::AuthManager;
use crate::auth::model::KimiAuth;
use crate::auth::token_type::TokenType;

/// Whether a terminal `AuthError` forces a manual re-login (`false` cases
/// self-heal or are transient). Lives here (not on `AuthError`) so the error
/// model stays free of recovery policy.
pub(crate) fn forces_manual_reauth(err: &AuthError) -> bool {
    match err {
        AuthError::Refresh(RefreshTokenError::Permanent(e)) => match e.reason {
            RefreshTokenFailedReason::RefreshTokenRejected => true,
            // Self-healing via the tombstone cooldown, not a manual re-auth.
            RefreshTokenFailedReason::Other => false,
        },
        AuthError::ServerRejectedNoRecovery
        | AuthError::RecoveryExhausted
        | AuthError::TokenExpiredNoRefresh => true,
        AuthError::Refresh(RefreshTokenError::Transient(_)) | AuthError::NotLoggedIn => false,
    }
}

/// Fresh-mint guard window (±) for `ServerRejected` refreshes
/// ([`UnauthorizedRecovery::fresh_mint_guard`]). 120s outlasts in-flight
/// requests sent with a previous key plus validation lag (observed stale
/// 401s land ~20s after mint), while the refresh-threshold buffer keeps any
/// guard-returned token wire-valid. A genuinely-dead fresh token waits at
/// most this long to re-mint; the symmetric bound caps that delay when the
/// clock stepped back.
const FRESH_MINT_GUARD_SECS: i64 = 120;

/// Which recovery step to attempt next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryStep {
    /// Re-read the persisted credential (file-locked).
    ReloadFromDisk,
    /// Refresh via the Kimi OAuth host.
    RefreshFromAuthority,
    /// All strategies exhausted.
    Done,
}

/// State machine that walks through recovery strategies after a 401.
pub struct UnauthorizedRecovery {
    auth_manager: Arc<AuthManager>,
    /// The token that was rejected by the server.
    rejected_token: String,
    /// Current step in the recovery sequence.
    step: RecoveryStep,
    /// Error from `RefreshFromAuthority`, propagated on exhaustion.
    authority_error: Option<AuthError>,
    /// Whether the last authority failure was transient. Kept past the
    /// `authority_error` handoff so exhaustion preserves the
    /// transient/permanent axis (see the `Done` arm).
    authority_was_transient: bool,
}

impl UnauthorizedRecovery {
    /// `rejected` is the credential the server rejected: its key drives recovery.
    pub(crate) fn new(auth_manager: Arc<AuthManager>, rejected: Option<KimiAuth>) -> Self {
        let rejected_token = rejected.as_ref().map(|a| a.key.clone()).unwrap_or_default();
        Self {
            auth_manager,
            rejected_token,
            step: RecoveryStep::ReloadFromDisk,
            authority_error: None,
            authority_was_transient: false,
        }
    }

    /// Attempt the next recovery step. Walks
    /// `ReloadFromDisk -> RefreshFromAuthority -> Done`.
    /// `token_type` span field is recorded lazily via
    /// `Span::is_disabled()` to avoid the lock when tracing is off.
    #[tracing::instrument(
        skip(self),
        fields(step = ?self.step, token_type = tracing::field::Empty),
    )]
    pub async fn next(&mut self) -> Result<KimiAuth, AuthError> {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            // Only acquire the inner-lock when tracing actually
            // collects the span. `token_type()` -> `inner.read()` is
            // ~free but it's still a lock, and recovery is on the
            // 401-recovery path; making the cost zero when tracing is
            // off matches the no-trace-no-cost contract.
            span.record(
                "token_type",
                tracing::field::debug(self.auth_manager.token_type()),
            );
        }
        self.next_step_loop().await
    }

    async fn next_step_loop(&mut self) -> Result<KimiAuth, AuthError> {
        loop {
            match self.step {
                RecoveryStep::ReloadFromDisk => {
                    self.step = RecoveryStep::RefreshFromAuthority;
                    if let Some(auth) = self.try_reload_from_disk().await {
                        return Ok(auth);
                    }
                }
                RecoveryStep::RefreshFromAuthority => {
                    self.step = RecoveryStep::Done;
                    match self.try_refresh_from_authority().await {
                        Ok(auth) => return Ok(auth),
                        Err(e) => {
                            self.authority_was_transient =
                                matches!(e, AuthError::Refresh(RefreshTokenError::Transient(_)));
                            self.authority_error = Some(e);
                            return Err(self
                                .authority_error
                                .take()
                                .unwrap_or(AuthError::RecoveryExhausted));
                        }
                    }
                }
                RecoveryStep::Done => {
                    // Exhaustion after a *transient* authority failure stays
                    // transient: `RecoveryExhausted` here would count a network
                    // blip as a forced re-login and cancel the relay instead of
                    // letting it reconnect.
                    return Err(if self.authority_was_transient {
                        AuthError::transient("recovery exhausted after transient refresh failure")
                    } else {
                        AuthError::RecoveryExhausted
                    });
                }
            }
        }
    }

    /// Re-read the persisted credential. Accept the token only if it differs
    /// from the one that was rejected.
    async fn try_reload_from_disk(&self) -> Option<KimiAuth> {
        let _lock = self
            .auth_manager
            .try_lock_auth_file_async(crate::auth::manager::AUTH_LOCK_TIMEOUT)
            .await;
        if _lock.is_none() {
            tracing::warn!("auth recovery: proceeding without file lock");
        }

        let Some(disk_auth) = self.auth_manager.read_disk_auth() else {
            // Every ReloadFromDisk outcome must log (adopted / expired /
            // same-as-rejected / no entry): a silent arm hides which path
            // a recovery loop is taking. Debug level — the disk-state
            // *transition* is logged once by `read_disk_auth` itself.
            kigi_log::unified_log::debug("auth recovery: no persisted entry", None, None);
            return None;
        };
        if crate::auth::is_expired(&disk_auth) {
            tracing::debug!("auth recovery: persisted token is expired, skipping");
            kigi_log::unified_log::debug(
                "auth recovery: persisted token expired",
                None,
                Some(serde_json::json!({
                    "disk_key_prefix": crate::auth::token_suffix(&disk_auth.key),
                    "expires_at": disk_auth.expires_at.map(|e| e.to_rfc3339()),
                })),
            );
            return None;
        }
        if self.is_different_token(&disk_auth) {
            tracing::info!("auth recovery: persisted store has a different token, accepting");
            kigi_log::unified_log::info(
                "auth recovery: adopted persisted token",
                None,
                Some(serde_json::json!({
                    "adopted_key_prefix": crate::auth::token_suffix(&disk_auth.key),
                    "expires_at": disk_auth.expires_at.map(|e| e.to_rfc3339()),
                })),
            );
            self.auth_manager.hot_swap(disk_auth.clone());
            Some(disk_auth)
        } else {
            tracing::debug!("auth recovery: persisted token is same as rejected, skipping");
            kigi_log::unified_log::debug(
                "auth recovery: persisted token same as rejected",
                None,
                None,
            );
            None
        }
    }

    /// Return the live token instead of refreshing when its mint age is
    /// within ±[`FRESH_MINT_GUARD_SECS`]; anything outside (including a
    /// clock that stepped far back) falls through to a normal refresh.
    ///
    /// A 401 moments after a successful mint is a stale rejection (sent with
    /// the previous key) or validation lag on the new key — re-minting fixes
    /// neither, and a crash between the token grant and persisting the
    /// response orphans the replacement RT (forced re-login). Consumers retry
    /// with the returned token; a genuinely-bad one refreshes once the window
    /// passes.
    fn fresh_mint_guard(&self) -> Option<KimiAuth> {
        let auth = self.auth_manager.current()?;
        let mint_age_seconds = auth.mint_age_seconds();
        if !(-FRESH_MINT_GUARD_SECS..FRESH_MINT_GUARD_SECS).contains(&mint_age_seconds) {
            return None;
        }
        tracing::info!(
            mint_age_seconds,
            "auth recovery: current token freshly minted, skipping refresh"
        );
        kigi_log::unified_log::info(
            "auth recovery: fresh mint, refresh skipped",
            None,
            Some(serde_json::json!({
                "key_prefix": crate::auth::token_suffix(&auth.key),
                "mint_age_seconds": mint_age_seconds,
                "guard_seconds": FRESH_MINT_GUARD_SECS,
                "expires_at": auth.expires_at.map(|e| e.to_rfc3339()),
            })),
        );
        Some(auth)
    }

    /// Dispatch to the refresh chain based on the current `TokenType`.
    ///
    /// Per-variant outcome:
    ///
    /// - **OAuthSession**: full refresh chain via the OAuth host, unless the
    ///   live token is inside the fresh-mint guard window
    ///   ([`Self::fresh_mint_guard`]).
    /// - **SessionNoRefresh / ApiKey**: no refresh authority for these
    ///   types. We've already tried `ReloadFromDisk` (the previous
    ///   recovery step), so the server's 401 stands. Surface
    ///   [`AuthError::ServerRejectedNoRecovery`] -- *not*
    ///   `TokenExpiredNoRefresh`, because the trigger here is the
    ///   server rejecting the token (it may not have aged past any
    ///   local TTL; ApiKey in particular has no expiry). Consumers
    ///   reading the variant can distinguish "ran past local TTL" from
    ///   "server actively rejected".
    /// - **None**: no credentials at all.
    async fn try_refresh_from_authority(&self) -> Result<KimiAuth, AuthError> {
        let tt = self.auth_manager.token_type();
        match tt {
            TokenType::OAuthSession => {
                if let Some(auth) = self.fresh_mint_guard() {
                    return Ok(auth);
                }
                let result = self
                    .auth_manager
                    .refresh_chain(tt, crate::auth::manager::RefreshReason::ServerRejected)
                    .await;
                match &result {
                    Ok(auth) => {
                        kigi_log::unified_log::info(
                            "auth recovery: refreshed from authority",
                            None,
                            Some(serde_json::json!({
                                "token_type": format!("{tt:?}"),
                                "new_key_prefix": crate::auth::token_suffix(&auth.key),
                                "expires_at": auth.expires_at.map(|e| e.to_rfc3339()),
                            })),
                        );
                    }
                    Err(e) => {
                        kigi_log::unified_log::warn(
                            "auth recovery: refresh from authority failed",
                            None,
                            Some(serde_json::json!({
                                "token_type": format!("{tt:?}"),
                                "error": format!("{e}"),
                            })),
                        );
                    }
                }
                result
            }
            TokenType::SessionNoRefresh | TokenType::ApiKey => {
                kigi_log::unified_log::warn(
                    "auth recovery: no refresh authority for token type",
                    None,
                    Some(serde_json::json!({ "token_type": format!("{tt:?}") })),
                );
                Err(AuthError::ServerRejectedNoRecovery)
            }
            TokenType::None => Err(AuthError::NotLoggedIn),
        }
    }

    /// Check if a candidate token is different from the rejected one.
    fn is_different_token(&self, candidate: &KimiAuth) -> bool {
        candidate.key != self.rejected_token
    }
}

#[cfg(test)]
mod tests {
    //! State-machine matrix tests for `UnauthorizedRecovery`.
    //!
    //! Coverage targets:
    //! - All 4 `TokenType` variants x dispatch in `try_refresh_from_authority`.
    //! - `try_reload_from_disk`: same/different/no token on disk.
    //! - `next()` exhaustion (Done -> RecoveryExhausted).
    //! - Fresh-mint guard: ±window bounds, tombstone grace.
    //!
    //! These tests use the same in-process `AuthManager` that production
    //! does and inject a counting refresher so we can observe whether the
    //! authority was consulted.
    use super::*;
    use crate::auth::config::KimiCodeConfig;
    use crate::auth::error::RefreshTokenError;
    use crate::auth::model::{AuthMode, KimiAuth};
    use crate::auth::refresh::{RefreshOutcome, TokenRefresher};
    use crate::auth::storage::{read_auth_json, write_auth_json};
    use chrono::{Duration, Utc};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The rejected wire bearer these tests seed into the manager.
    fn rejected_cred() -> Option<KimiAuth> {
        Some(KimiAuth {
            key: "rejected-tok".into(),
            ..KimiAuth::test_default()
        })
    }

    /// Refresher fake: returns Success with a fresh token on every call.
    struct OkRefresher {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl TokenRefresher for OkRefresher {
        async fn refresh(&self, _reason: crate::auth::manager::RefreshReason) -> RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::Success(Box::new(KimiAuth {
                key: "fresh-from-authority".into(),
                auth_mode: AuthMode::OAuth,
                refresh_token: Some("rt-new".into()),
                expires_at: Some(Utc::now() + Duration::hours(1)),
                ..KimiAuth::test_default()
            }))
        }
    }

    /// Refresher fake: returns PermanentFailure (rejected refresh token).
    struct FailRefresher {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl TokenRefresher for FailRefresher {
        async fn refresh(&self, _reason: crate::auth::manager::RefreshReason) -> RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::permanent(RefreshTokenFailedReason::RefreshTokenRejected, None)
        }
    }

    fn mgr() -> (tempfile::TempDir, Arc<AuthManager>) {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
        (dir, m)
    }

    fn seed(mgr: &AuthManager, mode: AuthMode, refresh_token: Option<&str>) {
        let auth = KimiAuth {
            key: "rejected-tok".into(),
            auth_mode: mode,
            refresh_token: refresh_token.map(str::to_string),
            // Past expiry so `current()` returns None and the refresh
            // chain actually has to do work.
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..KimiAuth::test_default()
        };
        mgr.hot_swap(auth);
    }

    // -- TokenType dispatch matrix ----------------------------------------

    #[tokio::test]
    async fn dispatch_oauth_session_uses_refresh_chain() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        // ReloadFromDisk fails (no disk auth), then RefreshFromAuthority succeeds.
        let auth = rec.next().await.expect("recovery should succeed");
        assert_eq!(auth.key, "fresh-from-authority");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // -- Fresh-mint guard --------------------------------------------------

    /// Seed a *valid* (unexpired) in-memory token whose `create_time` lies
    /// `mint_age` in the past (negative = clock stepped back since mint).
    fn seed_valid(mgr: &AuthManager, mint_age: Duration) {
        mgr.hot_swap(KimiAuth {
            key: "rejected-tok".into(),
            auth_mode: AuthMode::OAuth,
            refresh_token: Some("rt".into()),
            create_time: Utc::now() - mint_age,
            expires_at: Some(Utc::now() + Duration::hours(1)),
            expires_in: Some(3600),
            ..KimiAuth::test_default()
        });
    }

    /// Run one recovery against a counting refresher; return the outcome and
    /// how many times the authority was consulted.
    async fn recover_with_ok_refresher(m: &Arc<AuthManager>) -> (Result<KimiAuth, AuthError>, u32) {
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let result = rec.next().await;
        (result, calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn fresh_mint_guard_skips_wire_for_freshly_minted_token() {
        let (_d, m) = mgr();
        seed_valid(&m, Duration::seconds(10));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("guard returns the live token").key,
            "rejected-tok"
        );
        assert_eq!(calls, 0, "a 10s-old token must not be re-minted");
    }

    #[tokio::test]
    async fn fresh_mint_guard_treats_small_negative_age_as_fresh() {
        // Clock stepped back slightly since mint (NTP nudge).
        let (_d, m) = mgr();
        seed_valid(&m, Duration::seconds(-60));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("guard returns the live token").key,
            "rejected-tok"
        );
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn fresh_mint_guard_refreshes_when_clock_stepped_far_back() {
        // A large backwards clock step must not wedge recovery for the whole
        // step: outside the ±window the guard stands down.
        let (_d, m) = mgr();
        seed_valid(&m, Duration::hours(-1));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("recovery should succeed").key,
            "fresh-from-authority"
        );
        assert_eq!(calls, 1, "far-negative mint age must reach the wire");
    }

    #[tokio::test]
    async fn fresh_mint_guard_lets_old_token_refresh() {
        let (_d, m) = mgr();
        seed_valid(&m, Duration::minutes(10));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("recovery should succeed").key,
            "fresh-from-authority"
        );
        assert_eq!(
            calls, 1,
            "outside the guard window ServerRejected must reach the wire"
        );
    }

    #[tokio::test]
    async fn fresh_mint_guard_wins_over_cached_tombstone() {
        // A fresh *valid* token is served even when a tombstone is cached
        // for its refresh token — mirrors `auth()`'s wire-valid grace arm;
        // the tombstone re-applies once the guard window passes.
        let (_d, m) = mgr();
        seed_valid(&m, Duration::seconds(10));
        m.record_permanent_failure(
            "rt".into(),
            RefreshTokenFailedReason::RefreshTokenRejected.into(),
        );
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result
                .expect("guard precedes the tombstone short-circuit")
                .key,
            "rejected-tok"
        );
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn dispatch_session_without_refresh_token_returns_server_rejected_no_recovery() {
        // OAuth without refresh_token classifies as SessionNoRefresh.
        let (_d, m) = mgr();
        seed(&m, AuthMode::OAuth, None);

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(matches!(err, AuthError::ServerRejectedNoRecovery));
    }

    #[tokio::test]
    async fn dispatch_api_key_returns_server_rejected_no_recovery() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::ApiKey, None);

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::ServerRejectedNoRecovery),
            "ApiKey recovery should surface ServerRejectedNoRecovery (not \
             TokenExpiredNoRefresh), got {err:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_none_returns_not_logged_in() {
        let (_d, m) = mgr();
        // No seed — inner stays None → TokenType::None.
        // Single next() falls through ReloadFromDisk → RefreshFromAuthority.
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::NotLoggedIn),
            "None token type should surface NotLoggedIn, got {err:?}",
        );
    }

    // -- ReloadFromDisk matrix --------------------------------------------

    #[tokio::test]
    async fn reload_from_disk_picks_up_different_token() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));

        // Sibling process wrote a different valid token to disk.
        let scope = m.kimi_code_config().auth_scope();
        let fresh = KimiAuth {
            key: "fresh-from-disk".into(),
            auth_mode: AuthMode::OAuth,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            expires_in: Some(3600),
            ..KimiAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, fresh);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec
            .next()
            .await
            .expect("recovery should pick up the disk token");
        assert_eq!(auth.key, "fresh-from-disk");
    }

    #[tokio::test]
    async fn reload_from_disk_skips_same_token_then_proceeds_to_authority() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));

        // Disk has the SAME token that was rejected -- skip, fall through.
        let scope = m.kimi_code_config().auth_scope();
        let same = KimiAuth {
            key: "rejected-tok".into(),
            auth_mode: AuthMode::OAuth,
            refresh_token: Some("rt".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            expires_in: Some(3600),
            ..KimiAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, same);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();

        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("authority refresh succeeds");
        assert_eq!(auth.key, "fresh-from-authority");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fall-through to authority must invoke the refresher exactly once",
        );
    }

    // -- Done state -------------------------------------------------------

    /// With no stored authority error (the first `next()` succeeded), driving
    /// past `Done` surfaces `RecoveryExhausted`. The transient-failure case is
    /// pinned by `exhaustion_after_transient_failure_stays_transient`.
    #[tokio::test]
    async fn next_after_done_returns_recovery_exhausted() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));
        m.set_refresher(Arc::new(OkRefresher {
            calls: Arc::new(AtomicU32::new(0)),
        }));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let _ = rec.next().await.unwrap();
        let err = loop {
            if let Err(e) = rec.next().await {
                break e;
            }
        };
        assert!(
            matches!(err, AuthError::RecoveryExhausted),
            "Done state must surface RecoveryExhausted, got {err:?}",
        );
    }

    /// Exhaustion after a *transient* authority failure preserves the
    /// transient axis: surfacing `RecoveryExhausted` would count a network
    /// blip as a forced re-login (`manual_auth`) and make the relay cancel
    /// instead of reconnect.
    #[tokio::test]
    async fn exhaustion_after_transient_failure_stays_transient() {
        /// Refresher fake: transient failure on every call.
        struct TransientFailRefresher;
        #[async_trait::async_trait]
        impl TokenRefresher for TransientFailRefresher {
            async fn refresh(
                &self,
                _reason: crate::auth::manager::RefreshReason,
            ) -> RefreshOutcome {
                RefreshOutcome::transient("network blip")
            }
        }

        let (_d, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));
        m.set_refresher(Arc::new(TransientFailRefresher));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        // First next(): the authority's transient error propagates as-is.
        let first = rec.next().await.unwrap_err();
        assert!(
            matches!(first, AuthError::Refresh(RefreshTokenError::Transient(_))),
            "authority transient must propagate, got {first:?}",
        );

        // Driving past exhaustion must stay transient too.
        let err = loop {
            if let Err(e) = rec.next().await {
                break e;
            }
        };
        assert!(
            matches!(err, AuthError::Refresh(RefreshTokenError::Transient(_))),
            "exhaustion after a transient failure must stay transient, got {err:?}",
        );
        assert!(
            !forces_manual_reauth(&err),
            "a transient exhaustion must not force a manual re-login",
        );
    }

    // -- Tombstone short-circuit (cross-check) ------------

    #[tokio::test]
    async fn refresh_authority_short_circuits_on_cached_tombstone() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));
        // Pre-record a tombstone scoped to the seeded refresh token.
        m.record_permanent_failure(
            "rt".into(),
            RefreshTokenFailedReason::RefreshTokenRejected.into(),
        );

        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(FailRefresher {
            calls: calls.clone(),
        }));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(matches!(
            err,
            AuthError::Refresh(RefreshTokenError::Permanent(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "refresher must not be invoked while the tombstone cooldown is live",
        );
    }

    // -- ReloadFromDisk rejects expired disk tokens -------------------------

    /// Regression: disk holds a different but expired token. Recovery
    /// must skip it and fall through to RefreshFromAuthority, not
    /// return it for the caller to send on the wire (instant 401).
    #[tokio::test]
    async fn reload_from_disk_rejects_expired_different_token() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::OAuth, Some("rt"));

        let scope = m.kimi_code_config().auth_scope();
        let expired_different = KimiAuth {
            key: "different-but-expired".into(),
            auth_mode: AuthMode::OAuth,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..KimiAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, expired_different);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();

        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));

        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("should fall through to authority");
        assert_eq!(
            auth.key, "fresh-from-authority",
            "must skip the expired disk token and use the refresher"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
