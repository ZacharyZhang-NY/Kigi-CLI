//! `AuthManager` behavior tests for the Kimi Code auth stack:
//! tombstone semantics (PRD F1), persistence (keyring + file fallback),
//! dynamic refresh threshold, dispatch, and the proactive-tick loop body.
//!
//! Cross-process lock behavior is covered in `manager/lock.rs`; sleep-gate
//! internals in `manager/sleep_gate.rs`; wire behavior in `kimi_oauth.rs` /
//! `refresh/kimi_refresher.rs`.

use super::*;
use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::model::AuthMode;
use chrono::Utc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

fn mgr() -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    (dir, m)
}

/// Session credential with `remaining_secs` of lifetime left out of a
/// `expires_in`-second grant.
fn session(key: &str, rt: &str, expires_in: i64, remaining_secs: i64) -> KimiAuth {
    KimiAuth {
        key: key.into(),
        auth_mode: AuthMode::OAuth,
        refresh_token: Some(rt.into()),
        expires_at: Some(Utc::now() + Duration::seconds(remaining_secs)),
        expires_in: Some(expires_in),
        ..KimiAuth::test_default()
    }
}

/// Counting refresher returning a fixed success.
struct OkRefresher {
    calls: Arc<AtomicU32>,
}
#[async_trait::async_trait]
impl TokenRefresher for OkRefresher {
    async fn refresh(&self, _reason: RefreshReason) -> RefreshOutcome {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        RefreshOutcome::success(session("at-refreshed", "rt-refreshed", 3600, 3600))
    }
}

/// Counting refresher that always reports the refresh token rejected.
struct RejectRefresher {
    calls: Arc<AtomicU32>,
    rejected_rt: String,
}
#[async_trait::async_trait]
impl TokenRefresher for RejectRefresher {
    async fn refresh(&self, _reason: RefreshReason) -> RefreshOutcome {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        RefreshOutcome::permanent(
            RefreshTokenFailedReason::RefreshTokenRejected,
            Some(self.rejected_rt.clone()),
        )
    }
}

fn install_ok_refresher(m: &Arc<AuthManager>) -> Arc<AtomicU32> {
    let calls = Arc::new(AtomicU32::new(0));
    m.set_refresher(Arc::new(OkRefresher {
        calls: calls.clone(),
    }));
    calls
}

// ── Dynamic refresh threshold (PRD: max(300, expires_in × 0.5)) ─────────

/// A 7200s-lifetime token with 3000s left is inside the 3600s threshold:
/// `current()` hides it (refresh due) while `expired_auth()` still exposes
/// it (wire-valid bearer for senders).
#[test]
fn threshold_hides_current_but_keeps_expired_auth() {
    let (_d, m) = mgr();
    m.hot_swap(session("at", "rt", 7200, 3000));
    assert!(m.current().is_none(), "inside threshold → refresh due");
    assert_eq!(m.expired_auth().map(|a| a.key), Some("at".into()));
    assert!(m.is_expired());
    assert!(m.has_usable_token(), "still wire-valid");

    // 5000s left is outside the threshold → fresh.
    m.hot_swap(session("at2", "rt", 7200, 5000));
    assert_eq!(m.current().map(|a| a.key), Some("at2".into()));
    assert!(!m.is_expired());
}

/// Hard expiry: a genuinely past-expiry token is not usable.
#[test]
fn hard_expired_token_is_not_usable() {
    let (_d, m) = mgr();
    m.hot_swap(session("at", "rt", 3600, -10));
    assert!(m.current().is_none());
    assert!(m.expired_auth().is_some(), "kept for its refresh token");
    assert!(!m.has_usable_token());
}

// ── auth() dispatch ─────────────────────────────────────────────────────

#[tokio::test]
async fn auth_returns_not_logged_in_when_empty() {
    let (_d, m) = mgr();
    let err = m.auth().await.unwrap_err();
    assert!(matches!(err, AuthError::NotLoggedIn), "got {err:?}");
}

#[tokio::test]
async fn auth_fast_path_returns_valid_cached_token() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-valid", "rt", 7200, 7200));
    let auth = m.auth().await.unwrap();
    assert_eq!(auth.key, "at-valid");
}

#[tokio::test]
async fn auth_expired_api_key_surfaces_token_expired_no_refresh() {
    let (_d, m) = mgr();
    m.hot_swap(KimiAuth {
        key: "sk-old".into(),
        auth_mode: AuthMode::ApiKey,
        create_time: Utc::now() - Duration::days(31),
        ..KimiAuth::test_default()
    });
    let err = m.auth().await.unwrap_err();
    assert!(matches!(err, AuthError::TokenExpiredNoRefresh), "{err:?}");
}

#[tokio::test]
async fn auth_expired_session_refreshes_via_chain() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-old", 3600, -10));
    let calls = install_ok_refresher(&m);
    let auth = m.auth().await.unwrap();
    assert_eq!(auth.key, "at-refreshed");
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    // Refresh persisted: a fresh manager on the same home adopts it.
    assert_eq!(m.current().map(|a| a.key), Some("at-refreshed".into()));
}

/// Refresh success persists to the store so a sibling manager adopts it.
#[tokio::test]
async fn refresh_success_is_visible_to_sibling_manager() {
    let (dir, m) = mgr();
    m.hot_swap(session("at-old", "rt-old", 3600, -10));
    install_ok_refresher(&m);
    m.auth().await.unwrap();

    let sibling = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    assert_eq!(
        sibling.current().map(|a| a.key),
        Some("at-refreshed".into()),
        "sibling must load the rotated credential from the store"
    );
}

/// Refresh success wakes `wait_for_token_refresh` waiters.
#[tokio::test]
async fn refresh_notifies_waiters() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-old", 3600, -10));
    install_ok_refresher(&m);
    let waiter = {
        let m = m.clone();
        tokio::spawn(async move {
            m.wait_for_token_refresh(std::time::Duration::from_secs(5))
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    m.auth().await.unwrap();
    assert!(
        waiter.await.unwrap(),
        "waiter must observe the token change"
    );
}

// ── Tombstone semantics (PRD F1) ────────────────────────────────────────

/// A 401-rejected refresh sets a tombstone keyed by the rejected refresh
/// token; subsequent auth() calls short-circuit without hitting the wire.
#[tokio::test]
async fn rejected_refresh_sets_tombstone_and_short_circuits() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-dead", 3600, -10));
    let calls = Arc::new(AtomicU32::new(0));
    m.set_refresher(Arc::new(RejectRefresher {
        calls: calls.clone(),
        rejected_rt: "rt-dead".into(),
    }));

    let err = m.auth().await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Refresh(crate::auth::error::RefreshTokenError::Permanent(_))
        ),
        "{err:?}"
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert!(m.has_permanent_failure(), "tombstone must be live");

    // Second attempt: short-circuit, no second wire call.
    let err2 = m.auth().await.unwrap_err();
    assert!(
        matches!(
            err2,
            AuthError::Refresh(crate::auth::error::RefreshTokenError::Permanent(_))
        ),
        "{err2:?}"
    );
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        1,
        "tombstone must prevent a second refresh attempt"
    );
}

/// The tombstone auto-clears when the persisted refresh token differs
/// (another process rotated the credential).
#[tokio::test]
async fn tombstone_clears_when_persisted_refresh_token_rotates() {
    let (dir, m) = mgr();
    m.hot_swap(session("at-old", "rt-dead", 3600, -10));
    m.record_permanent_failure(
        "rt-dead".into(),
        RefreshTokenFailedReason::RefreshTokenRejected.into(),
    );
    assert!(m.has_permanent_failure());

    // Sibling process rotates the persisted credential.
    let sibling = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    sibling
        .update(session("at-rotated", "rt-rotated", 3600, 3600))
        .await
        .unwrap();

    assert!(
        !m.has_permanent_failure(),
        "tombstone must auto-clear once the persisted refresh token differs"
    );
    // And auth() adopts the sibling's credential.
    let auth = m.auth().await.unwrap();
    assert_eq!(auth.key, "at-rotated");
}

/// The tombstone cooldown (300s) ages out on the monotonic clock.
#[tokio::test]
async fn tombstone_cooldown_ages_out() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-dead", 3600, -10));
    m.record_permanent_failure(
        "rt-dead".into(),
        RefreshTokenFailedReason::RefreshTokenRejected.into(),
    );
    assert!(m.has_permanent_failure());
    m.force_permanent_failure_aged_out();
    assert!(
        !m.has_permanent_failure(),
        "cooldown elapsed → retry allowed"
    );
}

/// The cooldown also elapses on the wall clock alone (system slept through
/// the cooldown; monotonic clock paused).
#[tokio::test]
async fn tombstone_cooldown_ages_out_across_suspend() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-dead", 3600, -10));
    m.record_permanent_failure(
        "rt-dead".into(),
        RefreshTokenFailedReason::RefreshTokenRejected.into(),
    );
    m.force_permanent_failure_wall_aged_out();
    assert!(
        !m.has_permanent_failure(),
        "wall-clock aging alone must clear the cooldown"
    );
}

/// While a valid-on-the-wire access token exists, a tombstone must not block
/// auth() (the tombstone is about the refresh token, not the bearer).
#[tokio::test]
async fn tombstone_does_not_block_wire_valid_bearer() {
    let (_d, m) = mgr();
    // Inside threshold (refresh due) but not hard-expired.
    m.hot_swap(session("at-usable", "rt-dead", 7200, 3000));
    m.record_permanent_failure(
        "rt-dead".into(),
        RefreshTokenFailedReason::RefreshTokenRejected.into(),
    );
    let auth = m.auth().await.unwrap();
    assert_eq!(auth.key, "at-usable");
}

// ── Persistence: file fallback + keyring ────────────────────────────────

#[tokio::test]
async fn update_persists_to_file_when_keyring_disabled() {
    let (dir, m) = mgr();
    m.update(session("at-1", "rt-1", 3600, 3600)).await.unwrap();
    let store = read_auth_json(&dir.path().join("auth.json")).unwrap();
    let entry = store.get(KIMI_CODE_OAUTH_SCOPE).expect("scope entry");
    assert_eq!(entry.key, "at-1");
    assert_eq!(entry.refresh_token.as_deref(), Some("rt-1"));
}

#[tokio::test]
async fn remove_scope_deletes_file_entry_and_memory() {
    let (dir, m) = mgr();
    m.update(session("at-1", "rt-1", 3600, 3600)).await.unwrap();
    m.clear().unwrap();
    assert!(m.current_or_expired().is_none());
    assert!(
        !dir.path().join("auth.json").exists(),
        "last scope removed → file deleted"
    );
    let (auth, state) = m.read_disk_auth_with_state();
    assert!(auth.is_none());
    assert_eq!(state, DiskAuthState::FileMissing);
}

#[cfg(any(target_os = "macos", windows))]
mod keyring_integration {
    use super::*;
    use crate::auth::storage::{
        disable_mock_keyring_for_test, enable_mock_keyring_for_test, keyring_read_session,
    };

    struct MockKeyringGuard;
    impl MockKeyringGuard {
        fn enable() -> Self {
            enable_mock_keyring_for_test();
            Self
        }
    }
    impl Drop for MockKeyringGuard {
        fn drop(&mut self) {
            disable_mock_keyring_for_test();
        }
    }

    /// With the keyring available, update() writes the session there (not
    /// the file), reads come back from the keyring, and logout removes it.
    #[tokio::test]
    #[serial_test::serial(kigi_keyring)]
    async fn update_prefers_keyring_and_logout_clears_it() {
        let _guard = MockKeyringGuard::enable();
        let (dir, m) = mgr();
        m.update(session("at-kr", "rt-kr", 3600, 3600))
            .await
            .unwrap();
        assert!(
            !dir.path().join("auth.json").exists(),
            "session must NOT land in the plaintext file when the keyring is available"
        );
        assert!(matches!(
            keyring_read_session(),
            crate::auth::storage::KeyringRead::Found(_)
        ));
        let (auth, state) = m.read_disk_auth_with_state();
        assert_eq!(auth.map(|a| a.key), Some("at-kr".into()));
        assert_eq!(state, DiskAuthState::Ok);

        // A fresh manager (same process) loads from the keyring.
        let sibling = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
        assert_eq!(sibling.current().map(|a| a.key), Some("at-kr".into()));

        // Logout removes the keyring entry.
        m.clear().unwrap();
        assert!(matches!(
            keyring_read_session(),
            crate::auth::storage::KeyringRead::Missing
        ));
    }

    /// A stale file copy left from fallback days is stripped on the next
    /// keyring write, and the keyring copy wins on reads.
    #[tokio::test]
    #[serial_test::serial(kigi_keyring)]
    async fn keyring_write_strips_stale_file_copy() {
        let (dir, m) = mgr();
        // Keyring disabled: first write lands in the file.
        m.update(session("at-file", "rt-file", 3600, 3600))
            .await
            .unwrap();
        assert!(dir.path().join("auth.json").exists());

        // Keyring becomes available: the next write moves the credential.
        let _guard = MockKeyringGuard::enable();
        m.update(session("at-kr2", "rt-kr2", 3600, 3600))
            .await
            .unwrap();
        assert!(
            !dir.path().join("auth.json").exists(),
            "stale plaintext copy must be stripped after the keyring write"
        );
        assert_eq!(
            m.read_disk_auth().map(|a| a.key),
            Some("at-kr2".into()),
            "keyring copy is authoritative"
        );
    }
}

// ── Sibling adoption + disk reload ──────────────────────────────────────

#[tokio::test]
async fn pick_up_sibling_token_adopts_different_valid_token() {
    let (dir, m) = mgr();
    m.hot_swap(session("at-mine", "rt-mine", 3600, -10));

    let sibling = Arc::new(AuthManager::new(dir.path(), KimiCodeConfig::default()));
    sibling
        .update(session("at-sibling", "rt-sibling", 3600, 3600))
        .await
        .unwrap();

    m.pick_up_sibling_token();
    assert_eq!(m.current().map(|a| a.key), Some("at-sibling".into()));
}

#[test]
fn force_reload_drops_credentials_on_readable_entry_missing() {
    let (dir, m) = mgr();
    m.hot_swap(session("at-mine", "rt-mine", 3600, 3600));
    // A readable auth.json without our scope = trustworthy logout signal.
    let store = AuthStore::new();
    crate::auth::storage::write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
    // Non-empty map required for EntryMissing (empty map is still readable).
    m.force_reload_from_disk();
    assert!(
        m.current_or_expired().is_none(),
        "scope absent on readable store must drop in-memory credentials"
    );
}

#[test]
fn force_reload_retains_refresh_token_on_disk_anomaly() {
    let (dir, m) = mgr();
    m.hot_swap(session("at-mine", "rt-mine", 3600, 3600));
    // No auth.json at all (FileMissing anomaly): the in-memory refresh token
    // may be the only copy — retain it.
    assert!(!dir.path().join("auth.json").exists());
    m.force_reload_from_disk();
    assert_eq!(
        m.current_or_expired().map(|a| a.key),
        Some("at-mine".into()),
        "disk anomaly must not discard a live refresh token"
    );
}

// ── Proactive tick (loop body) ──────────────────────────────────────────

#[tokio::test]
async fn proactive_tick_skips_above_threshold() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-fresh", "rt", 7200, 7000));
    let calls = install_ok_refresher(&m);
    m.proactive_tick(false).await;
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        0,
        "above the refresh threshold no refresh must run"
    );
}

#[tokio::test]
async fn proactive_tick_refreshes_inside_threshold() {
    let (_d, m) = mgr();
    // 7200s lifetime, 3000s left → inside max(300, 3600) threshold.
    m.hot_swap(session("at-aging", "rt", 7200, 3000));
    let calls = install_ok_refresher(&m);
    m.proactive_tick(false).await;
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(m.current().map(|a| a.key), Some("at-refreshed".into()));
}

/// Sleep/wake force: a forced tick refreshes even a token comfortably above
/// the threshold (kimi-cli `refreshing()` parity).
#[tokio::test]
async fn proactive_tick_force_refreshes_valid_token() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-fresh", "rt", 7200, 7000));
    let calls = install_ok_refresher(&m);
    m.proactive_tick(true).await;
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        1,
        "force must bypass the threshold check"
    );
    assert_eq!(m.current().map(|a| a.key), Some("at-refreshed".into()));
}

#[tokio::test]
async fn proactive_tick_skips_non_refreshable_types() {
    let (_d, m) = mgr();
    m.hot_swap(KimiAuth {
        key: "sk-key".into(),
        auth_mode: AuthMode::ApiKey,
        ..KimiAuth::test_default()
    });
    let calls = install_ok_refresher(&m);
    m.proactive_tick(true).await;
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
}

#[tokio::test]
async fn proactive_tick_respects_tombstone_cooldown() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-dead", 3600, -10));
    m.record_permanent_failure(
        "rt-dead".into(),
        RefreshTokenFailedReason::RefreshTokenRejected.into(),
    );
    let calls = install_ok_refresher(&m);
    m.proactive_tick(false).await;
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        0,
        "tombstone cooldown must gate the proactive tick"
    );
}

// ── Sleep gate integration ──────────────────────────────────────────────

#[tokio::test]
async fn refresh_chain_defers_when_sleep_imminent() {
    let (_d, m) = mgr();
    m.hot_swap(session("at-old", "rt-old", 3600, -10));
    let calls = install_ok_refresher(&m);
    m.set_system_sleep_imminent(true);
    let err = m
        .refresh_chain(m.token_type(), RefreshReason::PreRequest)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::Refresh(crate::auth::error::RefreshTokenError::Transient(_))
        ),
        "sleep-gated refresh must defer transiently: {err:?}"
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    m.set_system_sleep_imminent(false);
    m.auth().await.unwrap();
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        1,
        "wake resumes refresh"
    );
}

// ── Idempotency guards ──────────────────────────────────────────────────

#[tokio::test]
async fn start_proactive_refresh_is_idempotent_per_arc() {
    let (_d, m) = mgr();
    let cancel = CancellationToken::new();
    m.start_proactive_refresh(cancel.clone());
    m.start_proactive_refresh(cancel.clone());
    assert_eq!(m.proactive_start_count(), 1, "second start must be a no-op");
    cancel.cancel();
}

#[test]
fn configure_refresher_is_idempotent() {
    let (_d, m) = mgr();
    assert!(m.configure_refresher(), "first call installs");
    assert!(!m.configure_refresher(), "second call is a no-op");
    assert!(m.has_refresher_attached());
}
