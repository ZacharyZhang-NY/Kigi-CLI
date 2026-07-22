//! `AuthManager` -- single source of truth for `auth.json` + the
//! in-memory bearer cache. Mutations go through `refresh_chain` or `update`; lock
//! and lock/sleep-gate helpers live in submodules.

use chrono::Duration;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio_util::sync::CancellationToken;

#[path = "manager/lock.rs"]
mod lock;
pub(crate) use lock::try_lock_auth_file_nonblocking;
#[path = "manager/sleep_gate.rs"]
mod sleep_gate;

use lock::try_lock_auth_file_async;
use sleep_gate::{GateRaise, InFlightGuard, SleepGate};

use crate::auth::config::{KIMI_CODE_OAUTH_SCOPE, KimiCodeConfig};
use crate::auth::error::AuthError;
use crate::auth::token_type::TokenType;

use super::model::{KimiAuth, is_expired, is_expired_with_buffer, lookup_auth, token_suffix};
use super::refresh::{RefreshOutcome, TokenRefresher, resolve_refresh_credential};
use super::storage::{
    AuthFileLock, KeyringRead, keyring_delete_session, keyring_enabled, keyring_read_session,
    keyring_write_session, read_auth_json, read_auth_json_or_empty_recovering_corrupt,
    write_auth_json,
};

#[cfg(test)]
use super::model::AuthStore;
#[cfg(test)]
use super::storage::read_auth_json_or_empty;

/// Why a token refresh is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    /// Pre-request check. Return cached token if still valid.
    PreRequest,
    /// Server returned 401/403. Must obtain a different token.
    ServerRejected,
}

/// Timeout for acquiring the advisory `auth.json.lock` file lock.
/// Used by advisory (non-critical) lock sites: `flow.rs`,
/// `recovery.rs`.
pub(crate) const AUTH_LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// Longer timeout for `refresh_chain` — the critical path that must
/// hold the file lock across the IdP call to prevent refresh-token
/// reuse.  Must exceed `EXTERNAL_REFRESH_TIMEOUT` (30 s) so followers
/// wait for the leader to finish rather than timing out and retrying.
const REFRESH_LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(45);

/// Fixed cadence of the background refresh check (PRD F1: every 60s).
pub(crate) const PROACTIVE_REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(60);

/// A tick whose wall-clock gap exceeds `interval × this` indicates the
/// machine slept through timer ticks; the next tick forces a refresh
/// (kimi-cli `refreshing()` parity).
const SLEEP_WAKE_FORCE_FACTOR: u32 = 2;

/// How long to wait after a file lock timeout before re-reading disk,
/// giving the lock holder time to finish writing.
const LOCK_TIMEOUT_WAIT: StdDuration = StdDuration::from_secs(2);

/// `force_reload_from_disk` re-read budget. A single `auth.json` read can
/// return `NotFound`/unreadable for reasons unrelated to logout — most
/// notably the first read right after wake-from-sleep, where the filesystem
/// briefly resolves the path to `ENOENT`. Retrying a few times absorbs that
/// transient; a genuine deletion/logout stays missing across the budget.
const RELOAD_RETRY_TRIES: usize = 3;

/// Backoff between `force_reload_from_disk` re-reads. Short enough to keep the
/// (sync) caller responsive, long enough to outlast a wake-time FS settle.
/// Only paid on the disk-anomaly branch, never on a healthy read.
const RELOAD_RETRY_BACKOFF: StdDuration = StdDuration::from_millis(50);

/// Refresh-rejection tombstone (PRD F1), scoped to the refresh-token value
/// the OAuth host rejected (`refresh_token_key`). The scope is what makes
/// invalidation automatic: once the persisted refresh token differs (another
/// process rotated it, or a fresh login landed), the tombstone reads through
/// as "no failure" without manual clearing.
struct ScopedRefreshFailure {
    /// The rejected refresh-token value.
    refresh_token_key: String,
    error: crate::auth::error::RefreshTokenFailedError,
    /// Two-clock timestamp (see [`GateRaise`]): the cooldown below is *real*
    /// time, so it must keep counting across a system sleep. The monotonic
    /// clock pauses during suspend — with it alone, a failure cached just
    /// before sleep would still short-circuit `auth()` for a further
    /// [`PERMANENT_FAILURE_TTL`] of *awake* time after wake, exactly when the
    /// user comes back and expects a recovered session.
    recorded_at: GateRaise,
}

/// Tombstone cooldown (PRD F1): a rejected refresh token is not re-sent for
/// this long; afterwards a retry is allowed (the server stays the authority).
/// Measured on both clocks — expires once *either* the monotonic or the wall
/// clock passes the bound, so it means "5 real minutes", not "5 awake
/// minutes" (a suspend doesn't extend it).
const PERMANENT_FAILURE_TTL: StdDuration = StdDuration::from_secs(300);

/// Single source of truth for `auth.json` + the in-memory bearer.
///
/// Lock order: `refresh_lock` (async) -> the sync locks (`inner` / `refresher`
/// / `permanent_failure`), never co-held; `permanent_failure()`
/// reads `permanent_failure` first and only then `inner` (via
/// `attempted_tombstone_key`, when a tombstone is stored), never co-held. Never hold
/// a `parking_lot` guard across `.await`. Refreshers return [`RefreshOutcome`]
/// for `refresh_chain` to apply.
/// Whether a manager rooted at `path` may use the OS keyring for `scope`.
///
/// The keyring entry (`service kigi / oauth/kimi-code`) is global per OS
/// user, so exactly one auth.json location can own it: the default install
/// path. Everything else (tempdir tests, `KIGI_SHARE_DIR` profiles,
/// `KIGI_AUTH_PATH` overrides) is file-scoped. The dynamic
/// [`keyring_enabled`] gate (env kill-switch, cfg(test) mock toggle) layers
/// on top at each call site.
fn keyring_path_scoped_for(path: &Path, scope: &str) -> bool {
    #[cfg(test)]
    if TEST_FORCE_KEYRING_PATH_SCOPE.with(|flag| flag.get()) {
        return scope == KIMI_CODE_OAUTH_SCOPE;
    }
    scope == KIMI_CODE_OAUTH_SCOPE && path == kigi_config::default_kigi_home().join("auth.json")
}

// Test seam: pretend managers on this thread are rooted at the default
// install so keyring behavior can be exercised against the mock keyring
// from a tempdir. Thread-local for the same reason as the mock-keyring
// toggle: no leakage into concurrently running persistence tests.
#[cfg(test)]
thread_local! {
    static TEST_FORCE_KEYRING_PATH_SCOPE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_test_force_keyring_path_scope(on: bool) {
    TEST_FORCE_KEYRING_PATH_SCOPE.with(|flag| flag.set(on));
}

pub struct AuthManager {
    /// In-memory bearer. Mutate via [`Self::with_inner_write`] or
    /// [`Self::refresh_chain`]; the closure helpers' sync return type
    /// enforces "no `.await` while holding the lock".
    inner: Arc<RwLock<Option<KimiAuth>>>,
    path: PathBuf,
    scope: String,
    /// Whether THIS manager may touch the OS keyring. The keyring entry is
    /// global per user, so it can only mirror the credential of the default
    /// install (`~/.kigi/auth.json`). Managers rooted anywhere else —
    /// integration-test tempdirs, alternate profiles, `KIGI_AUTH_PATH` —
    /// must never read, write, or DELETE it: before this guard, every
    /// `cargo test` run wiped the developer's real login via
    /// `remove_scope`'s keyring delete. [`keyring_enabled`] (env
    /// kill-switch / cfg(test) mock toggle) still gates dynamically on top.
    keyring_path_scoped: bool,
    kimi_code_config: KimiCodeConfig,
    refresher: RwLock<Option<Arc<dyn TokenRefresher>>>,
    /// Idempotency guard for `configure_refresher` so double-calls
    /// don't reset internal state.
    refresher_configured: std::sync::atomic::AtomicBool,
    /// Idempotency guard for `start_proactive_refresh` so we don't
    /// spawn competing refresh loops on the same Arc.
    proactive_started: std::sync::atomic::AtomicBool,
    /// Serializes concurrent refresh attempts (async, held across .await).
    refresh_lock: tokio::sync::Mutex<()>,
    permanent_failure: RwLock<Option<ScopedRefreshFailure>>,
    /// Loop-body iteration count -- catches busy-loops where the
    /// back-off gate fails to fire.
    #[cfg(test)]
    proactive_iter_count: std::sync::atomic::AtomicU32,
    /// `tokio::spawn` count -- catches idempotency-guard regressions
    /// (orthogonal to `proactive_iter_count`).
    #[cfg(test)]
    proactive_starts: std::sync::atomic::AtomicU32,
    /// Notified after every successful token refresh (key changed).
    /// Used by `ModelsManager` to trigger model catalog recovery
    /// after sleep/wake without relying on the file watcher.
    refresh_notify: Arc<tokio::sync::Notify>,
    /// Last state `read_disk_auth` observed for this manager's scope.
    /// Drives transition-level unified logging: hot retry loops read the
    /// disk every few seconds, so per-read logging would flood and no
    /// logging leaves auth.json loss invisible in production captures.
    disk_state: RwLock<Option<DiskAuthState>>,
    sleep_gate: SleepGate,
    /// Count of in-flight IdP refreshes (the network call only), so a
    /// sleep-imminent transition can wait for a refresh straddling suspend to
    /// finish before acknowledging sleep. Maintained by [`InFlightGuard`].
    refresh_in_flight: std::sync::atomic::AtomicU32,
    /// Pairs with `refresh_drain_cv` to let `set_system_sleep_imminent` (called
    /// on the OS power-listener thread) block until `refresh_in_flight` reaches
    /// zero. A plain `Mutex`/`Condvar` rather than the async `refresh_notify`
    /// because the power callback is synchronous and runs off any runtime.
    refresh_drain_lock: parking_lot::Mutex<()>,
    /// Condvar signaled by [`InFlightGuard::drop`] when the in-flight count hits
    /// zero; waited on by `hold_sleep_ack_until_refresh_drains`.
    refresh_drain_cv: parking_lot::Condvar,
    /// Idempotency guard for `start_system_power_listener`.
    power_listener_started: std::sync::atomic::AtomicBool,
    /// Keeps the OS power listener alive for this manager's lifetime; dropping
    /// it stops the listener. `None` until started (or if unavailable).
    power_listener: parking_lot::Mutex<Option<kigi_system_power::SystemPowerListener>>,
    /// When the current unbroken run of dark-wake refresh deferrals began, on
    /// two clocks (see [`GateRaise`]); `None` outside such a run. Bounds the
    /// deferral to [`sleep_gate::DARK_WAKE_DEFER_MAX`] so a machine stuck
    /// reporting dark wake can't defer refresh forever — see
    /// [`AuthManager::should_defer_for_dark_wake`].
    dark_wake_defer_since: parking_lot::RwLock<Option<GateRaise>>,
    /// Test-only override for [`AuthManager::is_dark_wake`]. `Some(_)` forces
    /// the dark-wake decision so the refresh-deferral path is unit-testable
    /// without a real macOS dark wake. `None` = consult the OS.
    #[cfg(test)]
    dark_wake_override: parking_lot::Mutex<Option<bool>>,
}

/// Discriminated outcome of a disk read, for transition logging.
/// `Ok` = entry present (possibly expired); the rest explain *why*
/// `read_disk_auth` returned `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskAuthState {
    /// auth.json readable and the scope entry exists.
    Ok,
    /// auth.json does not exist.
    FileMissing,
    /// auth.json readable but has no usable entry for this scope
    /// (scope removed, or only a skipped legacy WebLogin entry).
    EntryMissing,
    /// auth.json exists but could not be read (corrupt JSON, permission
    /// or I/O error).
    Unreadable,
}

/// On-disk outcome of [`AuthManager::remove_scope_impl`], emitted as the
/// `disk_mutation` field of the `auth: scope removed from auth.json` event so a
/// deliberate removal stays distinguishable from accidental credential loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeRemoval {
    /// Scope entry dropped; other scopes remain.
    EntryRemoved,
    /// Last scope dropped; auth.json deleted.
    FileDeleted,
    /// Lock unavailable (held by another process); disk left untouched.
    SkippedLockUnavailable,
    /// Lock held but auth.json was unreadable; disk left untouched.
    SkippedUnreadable,
}

impl ScopeRemoval {
    /// Stable telemetry label for the `disk_mutation` field.
    fn label(self) -> &'static str {
        match self {
            Self::EntryRemoved => "entry removed",
            Self::FileDeleted => "file deleted (no scopes left)",
            Self::SkippedLockUnavailable => "skipped (lock unavailable)",
            Self::SkippedUnreadable => "skipped (auth.json unreadable)",
        }
    }
}

/// Outcome of [`AuthManager::acquire_refresh_lock_or_adopt`] and
/// [`AuthManager::revalidate_lock_or_reacquire`]: the `auth.json` file lock is
/// proven live (or re-acquired) before the irreversible IdP call, so the RAII
/// guard outlives the exchange and no refresh token is double-spent; `Adopted`
/// means a sibling's freshly rotated token landed and the caller should return
/// it without refreshing.
enum LockOutcome {
    Held(AuthFileLock),
    Adopted(Box<KimiAuth>),
}

// ── Construction + builders ──────────────────────────────────────────

impl AuthManager {
    pub fn new(kigi_home: &Path, kimi_code_config: KimiCodeConfig) -> Self {
        let scope = kimi_code_config.auth_scope();

        kigi_log::unified_log::info(
            "AuthManager::new",
            None,
            Some(serde_json::json!({
                "scope": &scope,
                "kigi_home": kigi_home.display().to_string(),
                "HOME": std::env::var("HOME").unwrap_or_else(|_| "(unset)".into()),
                "KIGI_SHARE_DIR": std::env::var("KIGI_SHARE_DIR").unwrap_or_else(|_| "(unset)".into()),
                "KIGI_AUTH_PATH": std::env::var("KIGI_AUTH_PATH").unwrap_or_else(|_| "(unset)".into()),
                "KIGI_AUTH": std::env::var("KIGI_AUTH").map(|_| "(set)".to_string()).unwrap_or_else(|_| "(unset)".into()),
                "keyring_enabled": keyring_enabled(),
            })),
        );

        // KIGI_AUTH: inline JSON credentials (highest priority, read-only).
        if let Ok(inline_json) = std::env::var("KIGI_AUTH") {
            if let Ok(auth) = serde_json::from_str::<KimiAuth>(&inline_json) {
                return Self::assemble(
                    Some(auth),
                    kigi_home.join("auth.json"),
                    scope,
                    kimi_code_config,
                    None,
                );
            }
            tracing::warn!("KIGI_AUTH set but failed to parse as JSON, falling back to file");
        }

        // KIGI_AUTH_PATH: custom file path (overrides default $KIGI_SHARE_DIR/auth.json).
        let path = std::env::var("KIGI_AUTH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| kigi_home.join("auth.json"));

        // Keyring first (PRD F1): the session credential's primary store.
        if keyring_path_scoped_for(&path, &scope)
            && keyring_enabled()
            && let KeyringRead::Found(auth) = keyring_read_session()
        {
            kigi_log::unified_log::info(
                "AuthManager::new loaded session from system keyring",
                None,
                Some(serde_json::json!({
                    "key_prefix": token_suffix(&auth.key),
                    "is_expired": is_expired(&auth),
                })),
            );
            return Self::assemble(
                Some(*auth),
                path,
                scope,
                kimi_code_config,
                Some(DiskAuthState::Ok),
            );
        }

        let (auth, auth_read_detail, initial_disk_state) = match read_auth_json(&path) {
            Ok(map) => {
                let found = lookup_auth(&map, &scope);
                let detail = serde_json::json!({
                    "read": "ok",
                    "resolved_path": path.display().to_string(),
                    "scopes_on_disk": map.keys().collect::<Vec<_>>(),
                    "target_scope": &scope,
                    "found": found.is_some(),
                    "auth_mode": found.as_ref().map(|a| format!("{:?}", a.auth_mode)),
                    "is_expired": found.as_ref().map(is_expired),
                    "key_prefix": found.as_ref().map(|a| token_suffix(&a.key).to_owned()),
                });
                let state = if found.is_some() {
                    DiskAuthState::Ok
                } else {
                    DiskAuthState::EntryMissing
                };
                (found, detail, state)
            }
            Err(e) => {
                let detail = serde_json::json!({
                    "read": "error",
                    "error": e.to_string(),
                    "path": path.display().to_string(),
                    "path_exists": path.exists(),
                });
                let state = if e.kind() == std::io::ErrorKind::NotFound {
                    DiskAuthState::FileMissing
                } else {
                    DiskAuthState::Unreadable
                };
                (None, detail, state)
            }
        };
        kigi_log::unified_log::info(
            "AuthManager::new auth.json load result",
            None,
            Some(auth_read_detail),
        );

        Self::assemble(
            auth,
            path,
            scope,
            kimi_code_config,
            Some(initial_disk_state),
        )
    }

    /// Build a manager for a GENERIC device-code OAuth provider (xai-grok),
    /// scoped to `oauth.scope_key`. Unlike [`Self::new`] this path is
    /// file-store only (no keyring — that is gated to the default Kimi install)
    /// and ignores the Kimi-specific `KIGI_AUTH` inline-credential env; it
    /// otherwise shares the same multi-scope `auth.json` (honoring
    /// `KIGI_AUTH_PATH`). The refresher is selected from the scope by
    /// [`super::refresh::build_refresher`].
    pub(crate) fn new_oauth_provider(
        kigi_home: &Path,
        oauth: &'static kigi_models::OAuthConfig,
    ) -> Self {
        let scope = oauth.scope_key.to_owned();
        let path = std::env::var("KIGI_AUTH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| kigi_home.join("auth.json"));

        let (auth, disk_state) = match read_auth_json(&path) {
            Ok(map) => {
                let found = lookup_auth(&map, &scope);
                let state = if found.is_some() {
                    DiskAuthState::Ok
                } else {
                    DiskAuthState::EntryMissing
                };
                (found, state)
            }
            Err(e) => {
                let state = if e.kind() == std::io::ErrorKind::NotFound {
                    DiskAuthState::FileMissing
                } else {
                    DiskAuthState::Unreadable
                };
                (None, state)
            }
        };
        kigi_log::unified_log::info(
            "AuthManager::new_oauth_provider",
            None,
            Some(serde_json::json!({
                "scope": &scope,
                "found": auth.is_some(),
                "is_expired": auth.as_ref().map(is_expired),
            })),
        );
        Self::assemble(
            auth,
            path,
            scope,
            KimiCodeConfig::default(),
            Some(disk_state),
        )
    }

    /// Single field-assembly point for [`Self::new`]'s two construction paths
    /// (inline `KIGI_AUTH` vs. on-disk `auth.json`), which differ only in the
    /// threaded fields. One literal means a newly added field can't be silently
    /// dropped from one branch.
    fn assemble(
        inner: Option<KimiAuth>,
        path: PathBuf,
        scope: String,
        kimi_code_config: KimiCodeConfig,
        disk_state: Option<DiskAuthState>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(inner)),
            keyring_path_scoped: keyring_path_scoped_for(&path, &scope),
            path,
            scope,
            kimi_code_config,
            refresher: RwLock::new(None),
            refresher_configured: std::sync::atomic::AtomicBool::new(false),
            proactive_started: std::sync::atomic::AtomicBool::new(false),
            refresh_lock: tokio::sync::Mutex::new(()),
            permanent_failure: RwLock::new(None),
            #[cfg(test)]
            proactive_iter_count: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            proactive_starts: std::sync::atomic::AtomicU32::new(0),
            refresh_notify: Arc::new(tokio::sync::Notify::new()),
            disk_state: RwLock::new(disk_state),
            sleep_gate: SleepGate::default(),
            refresh_in_flight: std::sync::atomic::AtomicU32::new(0),
            refresh_drain_lock: parking_lot::Mutex::new(()),
            refresh_drain_cv: parking_lot::Condvar::new(),
            power_listener_started: std::sync::atomic::AtomicBool::new(false),
            power_listener: parking_lot::Mutex::new(None),
            dark_wake_defer_since: parking_lot::RwLock::new(None),
            #[cfg(test)]
            dark_wake_override: parking_lot::Mutex::new(None),
        }
    }

    // ── State mutation (clear, hot_swap, update) ──────────────────────

    pub(crate) fn clear(&self) -> std::io::Result<()> {
        self.remove_scope(&self.scope)
    }

    /// Remove a scope entry from auth.json. When `scope == self.scope`, also
    /// drops in-memory auth so a later `auth()` reports `NotLoggedIn`, not stale
    /// `invalid_grant` (the scoped verdict reads inert with no credential).
    /// Empties auth.json by deleting the file.
    ///
    /// Best-effort: takes a non-blocking lock and skips the disk write if
    /// another process holds it (the stale entry is cleaned up on next launch).
    pub(crate) fn remove_scope(&self, scope: &str) -> std::io::Result<()> {
        self.remove_scope_impl(scope)
    }

    fn remove_scope_impl(&self, scope: &str) -> std::io::Result<()> {
        // Session credentials also live in the system keyring (primary
        // store); drop that copy first so a file-side failure can't leave
        // the token behind. Gated on `keyring_scoped`: only the default
        // install owns the (global) keyring entry — a tempdir-rooted
        // manager deleting it would log the real user out.
        if scope == KIMI_CODE_OAUTH_SCOPE
            && self.keyring_path_scoped
            && let Err(e) = keyring_delete_session()
        {
            tracing::warn!(error = %e, "auth: failed to remove session credential from keyring");
        }
        let disk_mutation = if let Some(_lock) = lock::try_lock_auth_file_nonblocking(&self.path) {
            self.write_scope_removal(scope)? // lock released on drop
        } else {
            ScopeRemoval::SkippedLockUnavailable
        };
        // Intentional removal must be attributable from unified.jsonl:
        // downstream, a deliberately deleted auth.json is indistinguishable
        // from accidental loss (corruption, external deletion).
        kigi_log::unified_log::warn(
            "auth: scope removed from auth.json",
            None,
            Some(serde_json::json!({
                "scope": scope,
                "is_current_scope": scope == self.scope,
                "disk_mutation": disk_mutation.label(),
                "path": self.path.display().to_string(),
            })),
        );
        if scope == self.scope {
            self.clear_inner();
        }
        Ok(())
    }

    /// Drop `scope` from auth.json and persist, deleting the file when the last
    /// scope is gone. Caller holds the `auth.json` lock (taken by
    /// [`Self::remove_scope_impl`]).
    fn write_scope_removal(&self, scope: &str) -> std::io::Result<ScopeRemoval> {
        let Ok(mut auth_store) = read_auth_json(&self.path) else {
            return Ok(ScopeRemoval::SkippedUnreadable);
        };
        auth_store.remove(scope);
        if auth_store.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            Ok(ScopeRemoval::FileDeleted)
        } else {
            write_auth_json(&self.path, &auth_store)?;
            Ok(ScopeRemoval::EntryRemoved)
        }
    }

    /// Drop the in-memory auth. The sticky permanent-failure verdict is scoped
    /// to a credential key, so an empty cache reads through as "no failure"
    /// without explicit clearing.
    fn clear_inner(&self) {
        *self.inner.write() = None;
    }

    /// Re-read `auth.json` and reconcile the in-memory cache with it.
    ///
    /// A disk read returning "no usable token" has very different meanings that
    /// must not be conflated:
    ///
    /// * [`DiskAuthState::EntryMissing`] — the file is readable but our scope is
    ///   gone. This is the trustworthy "logged out / scope removed" signal, so
    ///   the in-memory credentials (and any cached permanent_failure) are
    ///   dropped together.
    /// * [`DiskAuthState::FileMissing`] / [`DiskAuthState::Unreadable`] — a
    ///   *disk anomaly*. The classic case is the first read after wake-from-
    ///   sleep transiently resolving `auth.json` to `ENOENT`. This is **not**
    ///   proof the credentials are gone, so we retry briefly and — if it
    ///   persists — retain a still-live in-memory refresh token rather than
    ///   discard the only copy. The server (a 401 driving `permanent_failure`)
    ///   stays the authority on whether the token is actually dead.
    pub(crate) fn force_reload_from_disk(&self) {
        self.force_reload_from_disk_with(RELOAD_RETRY_TRIES, RELOAD_RETRY_BACKOFF);
    }

    /// Inner of [`force_reload_from_disk`] with the retry budget injectable so
    /// the disk-anomaly branch is unit-testable without real sleeps.
    fn force_reload_from_disk_with(&self, tries: usize, backoff: StdDuration) {
        let mut last_state = DiskAuthState::FileMissing;
        for attempt in 0..tries.max(1) {
            if attempt > 0 && !backoff.is_zero() {
                std::thread::sleep(backoff);
            }
            let (auth, state) = self.read_disk_auth_with_state();
            last_state = state;
            match state {
                // Healthy entry on disk: regular swap. Do NOT clear the
                // permanent_failure here -- the token may just be a re-export
                // of the same broken refresh_token.
                DiskAuthState::Ok => {
                    *self.inner.write() = auth;
                    return;
                }
                // File readable, our scope genuinely absent: the trustworthy
                // logout / scope-removed signal.
                DiskAuthState::EntryMissing => {
                    self.drop_in_memory_credentials("scope absent on readable auth.json");
                    return;
                }
                // Disk anomaly: a transient (e.g. wake-time ENOENT) heals on a
                // retry; a real loss persists across the budget.
                DiskAuthState::FileMissing | DiskAuthState::Unreadable => {}
            }
        }

        // Persistent disk anomaly. Discarding a live refresh token here is the
        // step that turns a transient disk blip into irreversible credential
        // loss (the RT may exist nowhere else), so retain it unless it is
        // already known-dead (a cached permanent_failure) or there is nothing
        // to protect.
        let in_mem = self.current_or_expired();
        let retain = in_mem.as_ref().is_some_and(|a| a.refresh_token.is_some())
            && self.permanent_failure().is_none();
        if let Some(a) = in_mem.filter(|_| retain) {
            kigi_log::unified_log::warn(
                "auth: disk anomaly, retaining in-memory credentials",
                None,
                Some(serde_json::json!({
                    "disk_state": format!("{last_state:?}"),
                    "retained_key_prefix": token_suffix(&a.key),
                    "was_expired": is_expired(&a),
                })),
            );
            // In-memory credentials kept as-is.
        } else {
            self.drop_in_memory_credentials(
                "disk anomaly; no live refresh token to retain (missing RT or permanent failure)",
            );
        }
    }

    /// Drop the in-memory credentials, loudly. Logs the discard (with `reason`)
    /// before routing through [`clear_inner`] so the cached permanent_failure
    /// (if any) goes with them. Centralizes the "credentials gone" telemetry.
    fn drop_in_memory_credentials(&self, reason: &str) {
        if let Some(d) = self.current_or_expired() {
            kigi_log::unified_log::warn(
                "auth: in-memory credentials dropped (disk reload found none)",
                None,
                Some(serde_json::json!({
                    "reason": reason,
                    "dropped_key_prefix": token_suffix(&d.key),
                    "had_refresh_token": d.refresh_token.is_some(),
                    "was_expired": is_expired(&d),
                    "disk_state": (*self.disk_state.read()).map(|s| format!("{s:?}")),
                })),
            );
        }
        self.clear_inner();
    }

    // ── Read methods ─────────────────────────────────────────────────
    //
    // | wire-bound bearer            | `auth().await` / `get_valid_token().await` |
    // | cached, no refresh           | `current()` (5-min buffer) |
    // | any in-memory bearer         | `current_or_expired()` |
    // | expired entry (for its RT)   | `expired_auth()` |
    // | "have credentials at all?"   | `is_expired()` |
    // | bypass memory, read disk     | `read_disk_auth()` |

    /// Cached in-memory token if outside the refresh-threshold buffer.
    pub(crate) fn current(&self) -> Option<KimiAuth> {
        self.inner
            .read()
            .as_ref()
            .filter(|a| !self.is_token_expired(a))
            .cloned()
    }

    /// Closure-scoped write. Sync return type prevents `.await` while
    /// the lock is held. Prefer this over `self.inner.write()`.
    #[inline]
    pub(crate) fn with_inner_write<R>(&self, f: impl FnOnce(&mut Option<KimiAuth>) -> R) -> R {
        let mut guard = self.inner.write();
        f(&mut guard)
    }

    /// Closure-scoped read counterpart to [`Self::with_inner_write`].
    #[inline]
    pub(crate) fn with_inner_read<R>(&self, f: impl FnOnce(Option<&KimiAuth>) -> R) -> R {
        let guard = self.inner.read();
        f(guard.as_ref())
    }

    /// Returns true if credentials exist but have expired.
    pub(crate) fn is_expired(&self) -> bool {
        self.inner
            .read()
            .as_ref()
            .is_some_and(|a| self.is_token_expired(a))
    }

    /// In-memory bearer regardless of the refresh-threshold buffer.
    /// Prefer [`Self::auth`] when `.await` is available.
    pub(crate) fn current_or_expired(&self) -> Option<KimiAuth> {
        self.current().or_else(|| self.expired_auth())
    }

    /// Expired in-memory entry (for its `refresh_token`).
    pub(crate) fn expired_auth(&self) -> Option<KimiAuth> {
        self.inner
            .read()
            .as_ref()
            .filter(|a| self.is_token_expired(a))
            .cloned()
    }

    /// Expiry policy (PRD F1): expiring-soon once the remaining lifetime
    /// drops below `max(300, expires_in × 0.5)` seconds; credentials without
    /// `expires_at` fall back to `create_time + 30d`.
    fn is_token_expired(&self, auth: &KimiAuth) -> bool {
        is_expired(auth)
    }

    /// Actual (hard) expiry: the instant the server would actually reject the
    /// token, with no refresh-threshold margin. The export gate
    /// ([`Self::has_usable_token`]) uses this instead of [`Self::is_token_expired`]
    /// because a token still inside the threshold is sent — and accepted — on
    /// the wire via `current_or_expired()`, so it must not count as unusable.
    fn is_token_hard_expired(&self, auth: &KimiAuth) -> bool {
        is_expired_with_buffer(auth, Duration::zero())
    }

    // ── Persistence ───────────────────────────────────────────────────

    /// Persist rotated tokens (keyring → file fallback) + cache.
    ///
    /// Invariants:
    /// - **Persist before any further network I/O** (else a sibling process
    ///   can reuse the not-yet-rotated RT and the OAuth host rejects it).
    /// - **Caller holds the `auth.json` file lock** (production callers:
    ///   `refresh_chain` Success arm, `flow::run_auth_flow`).
    pub(crate) async fn update(self: &Arc<Self>, auth: KimiAuth) -> std::io::Result<KimiAuth> {
        let update_started = std::time::Instant::now();

        // Keyring first (PRD F1): the session credential's primary store.
        if self.keyring_path_scoped && keyring_enabled() {
            match keyring_write_session(&auth) {
                Ok(()) => {
                    let elapsed_ms = update_started.elapsed().as_millis() as u64;
                    kigi_log::unified_log::info(
                        "auth update written to system keyring",
                        None,
                        Some(serde_json::json!({
                            "rt_prefix": auth.refresh_token.as_deref().map(token_suffix),
                            "key_prefix": token_suffix(&auth.key),
                            "elapsed_ms": elapsed_ms,
                        })),
                    );
                    // Drop any stale plaintext copy left from a fallback-era
                    // write so the two stores can't diverge.
                    self.strip_scope_from_file_best_effort();
                    self.with_inner_write(|inner| *inner = Some(auth.clone()));
                    return Ok(auth);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "auth: keyring write failed, falling back to file");
                    kigi_log::unified_log::warn(
                        "auth update keyring write failed, using file fallback",
                        None,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
            }
        }

        let map = match read_auth_json_or_empty_recovering_corrupt(&self.path) {
            Ok(map) => map,
            Err(e) => {
                // Non-recoverable error (PermissionDenied, etc.) — keep conservative.
                tracing::warn!(error = %e, "auth: read failed, updating in-memory only");
                kigi_log::unified_log::warn(
                    "auth update skipped disk write (read failed)",
                    None,
                    Some(serde_json::json!({ "error": e.to_string() })),
                );
                self.with_inner_write(|inner| *inner = Some(auth.clone()));
                return Ok(auth);
            }
        };
        let mut map = map;
        // One entry per scope.
        tracing::debug!(scope = %self.scope, "auth: storing token");
        map.insert(self.scope.clone(), auth.clone());
        let write_result = write_auth_json(&self.path, &map);
        let elapsed_ms = update_started.elapsed().as_millis() as u64;
        match &write_result {
            Ok(()) => kigi_log::unified_log::info(
                "auth update disk written",
                None,
                Some(serde_json::json!({
                    "rt_prefix": auth.refresh_token.as_deref().map(token_suffix),
                    "key_prefix": token_suffix(&auth.key),
                    "elapsed_ms": elapsed_ms,
                })),
            ),
            Err(e) => kigi_log::unified_log::error(
                "auth update disk write failed",
                None,
                Some(serde_json::json!({
                    "error": e.to_string(),
                    "elapsed_ms": elapsed_ms,
                })),
            ),
        }
        // Always update in-memory, even if disk write failed. This lets the
        // current session work with fresh credentials while the user fixes the
        // filesystem (e.g. read-only disk). Without this, a disk failure leaves
        // the stale/dead token in memory and the user is completely stuck.
        self.with_inner_write(|inner| *inner = Some(auth.clone()));

        write_result?;
        Ok(auth)
    }

    /// Best-effort removal of this scope's entry from `auth.json` after a
    /// successful keyring write, so a stale plaintext copy can't shadow the
    /// keyring credential later. No lock escalation: callers already hold
    /// the auth-file lock on the mutation paths that matter.
    fn strip_scope_from_file_best_effort(&self) {
        let Ok(mut map) = read_auth_json(&self.path) else {
            return; // missing/corrupt file: nothing to strip
        };
        if map.remove(&self.scope).is_none() {
            return;
        }
        let result = if map.is_empty() {
            std::fs::remove_file(&self.path)
        } else {
            write_auth_json(&self.path, &map)
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "auth: failed to strip stale file copy after keyring write");
        } else {
            tracing::info!("auth: stripped stale file copy after keyring write");
        }
    }

    pub(crate) fn kimi_code_config(&self) -> &KimiCodeConfig {
        &self.kimi_code_config
    }

    /// The auth.json / keyring scope key this manager persists under
    /// (`oauth/kimi-code` for Kimi, `oauth/xai` for xai-grok, …). Drives the
    /// refresher selection in [`super::refresh::build_refresher`].
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    /// Handle notified after every successful token refresh.
    ///
    /// Used by [`ModelsManager`] to trigger model catalog recovery
    /// after sleep/wake, bypassing the FSEvents file watcher which
    /// can silently die on macOS after resume.
    pub fn refresh_notifier(&self) -> Arc<tokio::sync::Notify> {
        self.refresh_notify.clone()
    }

    /// Wait up to `timeout` for another consumer (proactive refresh task,
    /// main request path) to refresh the token.  Returns `true` if the
    /// in-memory token changed during the wait.
    ///
    /// Background consumers (signals sync, turn deltas) use this to defer
    /// to the primary refresh path instead of driving their own
    /// `ServerRejected` recovery, avoiding concurrent refresh storms that
    /// amplify 401 bursts at CCP.
    pub async fn wait_for_token_refresh(&self, timeout: std::time::Duration) -> bool {
        let pre_key = self.current().map(|a| a.key.clone());
        tokio::select! {
            _ = self.refresh_notify.notified() => {}
            _ = tokio::time::sleep(timeout) => {}
        }
        let post_key = self.current().map(|a| a.key.clone());
        post_key != pre_key
    }

    /// Hot-swap credentials (called by config watcher). Does NOT write to disk.
    pub(crate) fn hot_swap(&self, new_auth: KimiAuth) {
        self.with_inner_write(|inner| *inner = Some(new_auth));
    }

    /// Clear in-memory credentials. Does NOT touch disk, and does NOT clear the
    /// permanent-failure verdict: that is credential-scoped and self-invalidates
    /// on the next lookup once the credential it targets is gone.
    pub(crate) fn clear_in_memory(&self) {
        self.clear_inner();
    }

    // ── Disk I/O helpers ──────────────────────────────────────────────

    /// Accept a sibling-rotated disk token. On `ServerRejected`, the
    /// disk key must differ from in-memory (else no one refreshed).
    pub(crate) fn try_use_disk_token(
        &self,
        disk_auth: Option<&KimiAuth>,
        reason: RefreshReason,
    ) -> Option<KimiAuth> {
        let disk_auth = disk_auth?;
        if self.is_token_expired(disk_auth) {
            return None;
        }
        if reason == RefreshReason::ServerRejected {
            let current_key = self.inner.read().as_ref().map(|a| a.key.clone());
            if current_key.as_deref() == Some(&disk_auth.key) {
                tracing::info!("auth: disk token same as rejected token, skipping");
                return None;
            }
        }
        tracing::info!("auth: another process already refreshed, using disk token");
        self.hot_swap(disk_auth.clone());
        Some(disk_auth.clone())
    }

    /// Re-read disk and try to adopt a sibling-written token, emitting
    /// telemetry on success. Combines `read_disk_auth` +
    /// `try_use_disk_token` + the structured log that was previously
    /// duplicated at each callsite in `refresh_chain`.
    fn try_adopt_disk_token(&self, reason: RefreshReason, msg: &str) -> Option<KimiAuth> {
        let disk_auth = self.read_disk_auth();
        let refreshed = self.try_use_disk_token(disk_auth.as_ref(), reason)?;
        let adopted = token_suffix(&refreshed.key);
        let prev = self.expired_auth().map(|a| token_suffix(&a.key).to_owned());
        kigi_log::unified_log::info(
            msg,
            None,
            Some(serde_json::json!({
                "adopted_key_prefix": adopted,
                "prev_key_prefix": prev,
                "key_changed": prev.as_deref() != Some(adopted),
            })),
        );
        Some(refreshed)
    }

    /// Test-only hot_swap + disk write (file store only).
    /// Production persistence routes through `update()`.
    #[cfg(test)]
    fn persist_and_swap(&self, auth: KimiAuth) -> Option<KimiAuth> {
        self.hot_swap(auth.clone());
        let mut map = match read_auth_json_or_empty(&self.path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "auth: read failed in persist_and_swap, skipping disk write");
                return Some(auth);
            }
        };
        map.insert(self.scope.clone(), auth.clone());
        if let Err(e) = write_auth_json(&self.path, &map) {
            tracing::warn!(error = %e, "auth: failed to persist refreshed token to disk");
        }
        Some(auth)
    }

    /// `true` when a sibling process has rotated the refresh token on
    /// disk (disk RT differs from in-memory RT). Used by `refresh_chain`
    /// to demote a `PermanentFailure` to transient so the sibling's
    /// fresher token can be tried on the next attempt.
    fn sibling_has_different_refresh_token(&self) -> bool {
        let disk_auth = self.read_disk_auth();
        let Some(ref disk) = disk_auth else {
            return false;
        };
        // Expired AT = dead sibling, not a live one. Disk may have
        // diverged from memory due to failed writes (e.g. disk full)
        // while both RTs are revoked.
        if self.is_token_expired(disk) {
            return false;
        }
        let disk_rt = disk.refresh_token.as_deref();
        let Some(disk_rt) = disk_rt else {
            return false;
        };
        let mem_rt = self.expired_auth().and_then(|a| a.refresh_token);
        mem_rt.as_deref() != Some(disk_rt)
    }

    /// Re-read the persisted credential (keyring → file) without updating
    /// in-memory state.
    pub(crate) fn read_disk_auth(&self) -> Option<KimiAuth> {
        self.read_disk_auth_with_state().0
    }

    /// Persisted read for the configured scope with NO observation side
    /// effects (no `disk_state` write, no transition telemetry). For
    /// side-effect-free getters like [`Self::attempted_tombstone_key`]; prefer
    /// [`Self::read_disk_auth`] when the read should drive transition logging.
    fn read_disk_auth_silent(&self) -> Option<KimiAuth> {
        if self.scope == KIMI_CODE_OAUTH_SCOPE
            && let KeyringRead::Found(auth) = keyring_read_session()
        {
            return Some(*auth);
        }
        read_auth_json(&self.path)
            .ok()
            .and_then(|map| lookup_auth(&map, &self.scope))
    }

    /// Wire-valid token present in on-disk `auth.json`, judged by actual expiry
    /// ([`Self::is_token_hard_expired`]); never mutates in-memory state, unlike
    /// [`Self::force_reload_from_disk`].
    pub(crate) fn has_usable_disk_token(&self) -> bool {
        self.read_disk_auth()
            .is_some_and(|a| !self.is_token_hard_expired(&a))
    }

    /// Whether a wire-valid token is available in memory or on disk — a
    /// credential worth a real outbound attempt. Judged by actual expiry so it
    /// mirrors the `current_or_expired()` bearer the senders put on the wire; a
    /// token inside the early-invalidation buffer still counts.
    pub(crate) fn has_usable_token(&self) -> bool {
        self.current_or_expired()
            .is_some_and(|a| !self.is_token_hard_expired(&a))
            || self.has_usable_disk_token()
    }

    /// Like [`read_disk_auth`] but also returns the [`DiskAuthState`] so callers
    /// can tell a transient disk anomaly (`FileMissing`/`Unreadable`) apart from
    /// a genuine logout (`EntryMissing`). Observes the state for transition
    /// logging, exactly like `read_disk_auth`.
    pub(crate) fn read_disk_auth_with_state(&self) -> (Option<KimiAuth>, DiskAuthState) {
        // Keyring first (PRD F1): a hit is authoritative for the session
        // scope; a miss or an unavailable backend falls through to the file.
        if self.scope == KIMI_CODE_OAUTH_SCOPE
            && let KeyringRead::Found(auth) = keyring_read_session()
        {
            self.observe_disk_state(DiskAuthState::Ok, Some(&auth), None);
            return (Some(*auth), DiskAuthState::Ok);
        }
        let (auth, state, err_detail) = match read_auth_json(&self.path) {
            Ok(map) => {
                let found = lookup_auth(&map, &self.scope);
                let state = if found.is_some() {
                    DiskAuthState::Ok
                } else {
                    DiskAuthState::EntryMissing
                };
                (found, state, None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (None, DiskAuthState::FileMissing, None)
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "auth: failed to read auth.json"
                );
                (None, DiskAuthState::Unreadable, Some(e.to_string()))
            }
        };
        self.observe_disk_state(state, auth.as_ref(), err_detail);
        (auth, state)
    }

    /// Transition-level unified logging for the on-disk auth state:
    /// exactly one line per state change. Hot retry loops must produce
    /// neither a log flood nor silence — a single attributable event at
    /// the moment auth.json disappears (and one when it returns).
    fn observe_disk_state(
        &self,
        new_state: DiskAuthState,
        auth: Option<&KimiAuth>,
        err_detail: Option<String>,
    ) {
        let prev = {
            let mut guard = self.disk_state.write();
            let prev = *guard;
            *guard = Some(new_state);
            prev
        };
        if prev == Some(new_state) {
            return;
        }
        let ctx = serde_json::json!({
            "from": prev.map(|s| format!("{s:?}")),
            "to": format!("{new_state:?}"),
            "path": self.path.display().to_string(),
            "scope": &self.scope,
            "error": err_detail,
            "key_prefix": auth.map(|a| token_suffix(&a.key).to_owned()),
            "has_refresh_token": auth.map(|a| a.refresh_token.is_some()),
            "is_expired": auth.map(is_expired),
        });
        match new_state {
            // Recovery (or first observation in KIGI_AUTH mode).
            DiskAuthState::Ok => {
                kigi_log::unified_log::info("auth disk state: entry present", None, Some(ctx));
            }
            // Credential loss on disk — the line that answers "when did
            // auth.json disappear and what did this process see".
            DiskAuthState::FileMissing
            | DiskAuthState::EntryMissing
            | DiskAuthState::Unreadable => {
                kigi_log::unified_log::warn("auth disk state: entry lost", None, Some(ctx));
            }
        }
    }

    pub(crate) async fn try_lock_auth_file_async(
        &self,
        timeout: StdDuration,
    ) -> Option<AuthFileLock> {
        try_lock_auth_file_async(&self.path, timeout).await
    }

    // ── Refresher setup ─────────────────────────────────────────────

    /// Set up refresh capability. Call once per `Arc<AuthManager>` at
    /// startup; subsequent calls are no-op via an atomic guard (so
    /// per-session call sites don't reset refresher-internal state).
    /// Returns `true` if
    /// this call installed the refresher.
    pub fn configure_refresher(self: &Arc<Self>) -> bool {
        use std::sync::atomic::Ordering;
        // Idempotent: the AcqRel CAS publishes the subsequent
        // `refresher.write()` to any reader that observes
        // `refresher_configured == true`; the Acquire-failure pairs.
        if self
            .refresher_configured
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::debug!("auth: configure_refresher already wired; ignoring");
            return false;
        }
        let refresher = super::refresh::build_refresher(Arc::clone(self));
        *self.refresher.write() = Some(refresher);
        true
    }

    /// Test-only: inject a refresher, bypassing the idempotency guard.
    #[cfg(test)]
    pub(crate) fn set_refresher(&self, refresher: Arc<dyn TokenRefresher>) {
        use std::sync::atomic::Ordering;
        *self.refresher.write() = Some(refresher);
        self.refresher_configured.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn proactive_iteration_count(&self) -> u32 {
        self.proactive_iter_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn proactive_start_count(&self) -> u32 {
        self.proactive_starts
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `pub(super)` — for refresh dispatch only.
    pub(super) fn token_type(&self) -> TokenType {
        TokenType::from_auth(self.inner.read().as_ref())
    }

    // ── Pre-request dispatch ──────────────────────────────────────────

    /// Pre-request entry point: per-`TokenType` dispatch. For just the key:
    /// [`Self::get_valid_token`].
    #[tracing::instrument(skip(self), fields(token_type = tracing::field::Empty))]
    pub async fn auth(self: &Arc<Self>) -> Result<KimiAuth, AuthError> {
        self.auth_dispatch().await
    }

    async fn auth_dispatch(self: &Arc<Self>) -> Result<KimiAuth, AuthError> {
        // Snapshot inner ONCE for dispatch atomicity (closes a TOCTOU
        // where a concurrent `clear()` raced `token_type()` + `inner.read()`).
        let snapshot: Option<KimiAuth> = self.with_inner_read(|inner| inner.cloned());
        let token_type = TokenType::from_auth(snapshot.as_ref());
        tracing::Span::current().record("token_type", tracing::field::debug(token_type));

        // Fast path (before permanent_failure so a hot_swap after
        // re-login isn't blocked by a stale failure).
        if let Some(ref auth) = snapshot
            && !self.is_token_expired(auth)
        {
            return Ok(auth.clone());
        }

        if let Some(err) = self.permanent_failure() {
            // The verdict is about the *refresh* token; a cached access token
            // that is still wire-valid ([`Self::is_token_hard_expired`]) is
            // usable regardless (no IdP).
            if let Some(ref auth) = snapshot
                && !self.is_token_hard_expired(auth)
            {
                return Ok(auth.clone());
            }
            // A sibling process may have refreshed while we were in
            // PermanentFailure. Check the persisted store before giving up.
            if let Some(refreshed) = self.try_adopt_disk_token(
                RefreshReason::PreRequest,
                "auth: adopted sibling token during PermanentFailure in auth()",
            ) {
                return Ok(refreshed);
            }
            return Err(err);
        }

        match token_type {
            TokenType::None => Err(AuthError::NotLoggedIn),
            TokenType::ApiKey => {
                // The fast path above already returned for the valid case.
                // Reaching here means either the snapshot was empty (no
                // ApiKey loaded — surface NotLoggedIn) or the cached
                // api_key has aged past the 30-day TTL (surface
                // TokenExpiredNoRefresh so downstream consumers see
                // the same view as the UI's login screen, instead of
                // cloning the stale key and hitting 401 right after.
                if snapshot.is_some() {
                    Err(AuthError::TokenExpiredNoRefresh)
                } else {
                    Err(AuthError::NotLoggedIn)
                }
            }
            TokenType::SessionNoRefresh => {
                // Deliberate side effect: re-read the persisted store under
                // the assumption that a sibling process (`kigi login` from
                // another shell) may have refreshed the credential.
                // `pick_up_sibling_token` only mutates inner when the store
                // holds a *different valid* token, so the common cache-hit
                // case is a single read.
                self.pick_up_sibling_token();
                self.current().ok_or(AuthError::TokenExpiredNoRefresh)
            }
            TokenType::OAuthSession => {
                match self
                    .refresh_chain(token_type, RefreshReason::PreRequest)
                    .await
                {
                    Ok(auth) => Ok(auth),
                    Err(e) => {
                        // Grace: the refresh threshold is OUR conservative
                        // estimate, not the server's actual expiry. If the
                        // cached token is still wire-valid
                        // ([`Self::is_token_hard_expired`]), return it so a
                        // transient OAuth-host blip during the threshold
                        // window is invisible to the user.
                        if let Some(auth) = snapshot
                            && !self.is_token_hard_expired(&auth)
                        {
                            tracing::debug!(
                                "auth: refresh failed but token still valid (grace), using cached"
                            );
                            Ok(auth)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
        }
    }

    /// Return the current valid token string, or an error.
    pub(crate) async fn get_valid_token(self: &Arc<Self>) -> Result<String, AuthError> {
        self.auth().await.map(|a| a.key)
    }

    // ── Refresh chain (single mutation point) ─────────────────────────

    /// Acquire lock, double-check, try disk, then active refresh via injected refresher.
    ///
    /// This is the single place where auth state is mutated during refresh.
    /// The refresher returns data only (`RefreshOutcome`); all persistence,
    /// credential clearing, and permanent-failure recording happen here.
    ///
    /// Short-circuits with the cached permanent failure if a previous attempt
    /// has already recorded one for this credential, avoiding refresh requests
    /// we know will fail (e.g. from per-401 `unauthorized_recovery().next()`
    /// invocations that bypass `auth()`'s own permanent-failure check).
    #[tracing::instrument(skip(self), fields(?token_type, ?reason))]
    pub(crate) async fn refresh_chain(
        self: &Arc<Self>,
        token_type: TokenType,
        reason: RefreshReason,
    ) -> Result<KimiAuth, AuthError> {
        // 0. Sticky permanent-failure short-circuit, checked BEFORE acquiring
        //    the refresh lock so a backed-off chain doesn't block concurrent
        //    traffic. Mirrors `auth()` so callers routing through
        //    `unauthorized_recovery()` (skipping `auth()`) get the same backoff.
        //
        //    A sibling process may have refreshed while we were blocked, so try
        //    disk adoption first: a valid token changes the key, making the
        //    stale verdict read through as absent (no explicit clear). Breaks
        //    the retry storm where background consumers pile up 401s.
        if let Some(err) = self.permanent_failure() {
            if let Some(refreshed) = self.try_adopt_disk_token(
                reason,
                "auth: adopted sibling token during PermanentFailure short-circuit",
            ) {
                return Ok(refreshed);
            }
            // Debug, not warn: the verdict transition is already logged once by
            // `record_permanent_failure`; a 401-hammering consumer must not
            // flood warns on every short-circuited call.
            kigi_log::unified_log::debug(
                "auth: refresh_chain short-circuit on permanent failure",
                None,
                Some(serde_json::json!({
                    "token_type": format!("{token_type:?}"),
                    "reason": format!("{reason:?}"),
                    "failure": format!("{err}"),
                })),
            );
            return Err(err);
        }

        // Snapshot the token key before acquiring the lock so we can tell
        // whether another task refreshed while we were waiting.
        let pre_lock_key = self.current().map(|a| a.key.clone());

        let _guard = self.refresh_lock.lock().await;

        // 1. Double-check: another task may have refreshed while we waited.
        //    For ServerRejected we still check, but only return early if the
        //    token has *changed* (i.e. another task already refreshed it).
        //    If it is the same token that was rejected, we must proceed to
        //    the IdP to obtain one with fresh claims (e.g. after subscription
        //    purchase).
        if let Some(auth) = self.current()
            && (reason != RefreshReason::ServerRejected
                || pre_lock_key.as_deref() != Some(&auth.key))
        {
            return Ok(auth);
        }

        // 1b. Re-check the verdict under the lock: consumers that passed step 0
        //     before the leader recorded the failure would otherwise each hit
        //     the IdP with the dead credential. Caps a 401 burst at one call.
        if let Some(err) = self.permanent_failure() {
            return Err(err);
        }

        // 2. Acquire the exclusive file lock (or adopt a sibling token). The
        //    returned guard is held (via `file_lock` below) across the IdP call
        //    so only one participant ever spends a given refresh token.
        let file_lock = match self.acquire_refresh_lock_or_adopt(reason).await? {
            LockOutcome::Adopted(auth) => return Ok(*auth),
            LockOutcome::Held(lock) => lock,
        };

        // 3. Active refresh via authority.
        let refresher = self.refresher.read().clone();
        let Some(refresher) = refresher else {
            tracing::warn!("auth: no refresher configured");
            return Err(AuthError::transient("no refresher configured"));
        };

        // Fallback tombstone key, used only when the outcome carries no
        // `rejected_refresh_token`. Captured before the wire call so it
        // reflects the credential we resolved to send; see
        // [`Self::attempted_tombstone_key`].
        let attempted_key = self.attempted_tombstone_key(reason);

        // 3a. Pre-IdP deferral guards (sleep / dark wake).
        self.check_refresh_deferral(reason)?;

        // 3b. Re-validate (and if needed re-acquire) the live lock before the
        //     irreversible IdP call; adopt a sibling token if one landed.
        let file_lock = match self.revalidate_lock_or_reacquire(file_lock, reason).await? {
            LockOutcome::Adopted(auth) => return Ok(*auth),
            LockOutcome::Held(lock) => lock,
        };

        // 3c. Send the refresh token to the IdP and apply the outcome (the only
        //     mutation point). `file_lock` stays held across both.
        //
        // Let an in-flight call finish even if sleep becomes imminent: we do NOT
        // abort it. Once the refresh token is sent the IdP may already have
        // rotated it, so dropping the future would discard the response carrying
        // the new token, the exact revocation we guard against.
        //
        // To keep an in-flight refresh from *straddling* the suspend (the case
        // `auth.sleep.refresh_in_flight_at_suspend` records), the `WillSleep`
        // handler holds the OS sleep ack — macOS delays `IOAllowPowerChange`,
        // Linux holds its `delay` inhibitor — until `refresh_in_flight` drains
        // or `SLEEP_ACK_MAX_WAIT` elapses; see
        // `AuthManager::hold_sleep_ack_until_refresh_drains`.
        let outcome = {
            // Claim an in-flight slot, then do a final sleep-gate re-check
            // before the irreversible IdP call. A `WillSleep` may have raised
            // the gate after the step-3a check — e.g. while we awaited the file
            // lock in 3b. Claiming first and re-checking here narrows the race
            // to a few non-awaiting instructions: a sleep transition either
            // observes our slot (and its drain wait holds the ack for us) or we
            // observe its gate and back out, so the refresh does not start into
            // the suspend window the ack-hold protects.
            let _in_flight = InFlightGuard::new(self);
            if self.is_sleep_gated() {
                kigi_log::unified_log::warn(
                    "auth.sleep.refresh_deferred",
                    None,
                    Some(serde_json::json!({
                        "reason": format!("{reason:?}"),
                        "has_live_token": self.current().is_some(),
                        "stage": "pre_idp",
                    })),
                );
                return Err(AuthError::transient(
                    "refresh deferred: system sleep imminent",
                ));
            }
            refresher.refresh(reason).await
        };
        self.apply_refresh_outcome(outcome, reason, attempted_key, &file_lock)
            .await
    }

    /// Step 2: take the exclusive `auth.json` file lock. On timeout, wait then
    /// adopt a sibling's rotated token if one landed, else return transient: we
    /// *never* fall through unguarded (that "same RT used twice" race triggers
    /// invalid_grant + token-family revocation). With the lock held,
    /// adopt a freshly-written disk token if present. Returns the live guard so
    /// the caller keeps it across the IdP call.
    async fn acquire_refresh_lock_or_adopt(
        &self,
        reason: RefreshReason,
    ) -> Result<LockOutcome, AuthError> {
        let lock_started = std::time::Instant::now();
        let Some(file_lock) = self.try_lock_auth_file_async(REFRESH_LOCK_TIMEOUT).await else {
            tracing::warn!("auth: file lock timed out, waiting for sibling to finish");
            kigi_log::unified_log::warn(
                "auth.refresh.lock_timeout",
                None,
                Some(serde_json::json!({
                    "timeout_ms": lock_started.elapsed().as_millis() as u64,
                    "reason": format!("{reason:?}"),
                })),
            );
            tokio::time::sleep(LOCK_TIMEOUT_WAIT).await;
            if let Some(refreshed) = self.try_adopt_disk_token(
                reason,
                "auth: refresh adopted sibling token after lock timeout",
            ) {
                return Ok(LockOutcome::Adopted(Box::new(refreshed)));
            }
            tracing::warn!("auth: returning transient to avoid RT reuse");
            return Err(AuthError::transient(
                "could not acquire auth.json.lock within timeout; \
                 sibling may be mid-refresh",
            ));
        };
        if let Some(refreshed) = self.try_adopt_disk_token(reason, "auth: refresh used disk token")
        {
            return Ok(LockOutcome::Adopted(Box::new(refreshed)));
        }
        Ok(LockOutcome::Held(file_lock))
    }

    /// Step 3a: defer the not-yet-started refresh on sleep / dark wake. Safe and
    /// retryable because the refresh token was never sent.
    fn check_refresh_deferral(&self, reason: RefreshReason) -> Result<(), AuthError> {
        if self.is_sleep_gated() {
            // `has_live_token == false` is the dangerous defer: with no valid
            // token to fall back on, the caller's request 401s until the gate
            // clears, so make these greppable to distinguish harmless defers
            // (still-valid token) from the ones that surface as auth failures.
            let has_live_token = self.current().is_some();
            kigi_log::unified_log::warn(
                "auth.sleep.refresh_deferred",
                None,
                Some(serde_json::json!({
                    "reason": format!("{reason:?}"),
                    "has_live_token": has_live_token,
                })),
            );
            return Err(AuthError::transient(
                "refresh deferred: system sleep imminent",
            ));
        }

        // Dark wake (see `kigi_system_power::PowerState` for the canonical
        // explanation): defer the not-yet-started refresh. The refresh token
        // wasn't sent yet, so retrying on a later full wake is safe, whereas
        // starting the exchange now risks straddling the re-sleep and losing the
        // rotated successor token; no user is waiting, so deferring costs
        // nothing. `should_defer_for_dark_wake` bounds the deferral
        // (`DARK_WAKE_DEFER_MAX`) so a machine stuck reporting dark wake can't
        // defer forever and force a logout.
        if self.should_defer_for_dark_wake() {
            let has_live_token = self.current().is_some();
            kigi_log::unified_log::warn(
                "auth.dark_wake.refresh_deferred",
                None,
                Some(serde_json::json!({
                    "reason": format!("{reason:?}"),
                    "has_live_token": has_live_token,
                })),
            );
            return Err(AuthError::transient(
                "refresh deferred: dark wake (display off; system may re-sleep)",
            ));
        }
        Ok(())
    }

    /// Step 3b: re-validate that we still hold the *live* lock before the
    /// irreversible IdP call. A system suspend can freeze us long enough
    /// (> the stale-lock timeout) for a sibling to break our lock as "stuck"
    /// (unlink + fresh inode); our flock would then live on a now-deleted inode,
    /// and sending the refresh token would let two processes spend the same RT,
    /// the double-spend that trips IdP rotation reuse detection. If the lock was
    /// lost, re-acquire on the live inode (transient on timeout) and adopt a
    /// sibling's freshly-rotated token if one landed ([`LockOutcome::Adopted`]).
    async fn revalidate_lock_or_reacquire(
        &self,
        file_lock: AuthFileLock,
        reason: RefreshReason,
    ) -> Result<LockOutcome, AuthError> {
        if file_lock.still_live(&self.path) {
            return Ok(LockOutcome::Held(file_lock));
        }
        kigi_log::unified_log::warn(
            "auth.refresh.lock_lost_before_idp",
            None,
            Some(serde_json::json!({ "reason": format!("{reason:?}") })),
        );
        drop(file_lock);
        let Some(relock) = self.try_lock_auth_file_async(REFRESH_LOCK_TIMEOUT).await else {
            return Err(AuthError::transient(
                "refresh lock lost across suspend and re-acquire \
                 timed out; retrying avoids refresh-token double-spend",
            ));
        };
        if let Some(refreshed) = self.try_adopt_disk_token(
            reason,
            "auth: adopted sibling token after lock-loss revalidation",
        ) {
            return Ok(LockOutcome::Adopted(Box::new(refreshed)));
        }
        Ok(LockOutcome::Held(relock))
    }

    /// Step 3c outcome handling: the only mutation point, persisting on success
    /// and recording the tombstone on permanent failure. `attempted_key` is the
    /// fallback tombstone scope (used when the outcome carries no
    /// `rejected_refresh_token`).
    /// `_lock` is the held `auth.json` file lock: unused at runtime, threaded in
    /// to type-enforce that the persisting `update()` runs while the lock is held
    /// (so a future refactor can't drop it before persisting).
    async fn apply_refresh_outcome(
        self: &Arc<Self>,
        outcome: RefreshOutcome,
        reason: RefreshReason,
        attempted_key: Option<String>,
        _lock: &AuthFileLock,
    ) -> Result<KimiAuth, AuthError> {
        let pre_key_prefix = attempted_key.as_deref().map(token_suffix);
        match outcome {
            RefreshOutcome::Success(new_auth) => match self.update(*new_auth).await {
                Ok(auth) => {
                    let new_prefix = token_suffix(&auth.key);
                    kigi_log::unified_log::info(
                        "auth.refresh.success",
                        None,
                        Some(serde_json::json!({
                            "expires_at": auth.expires_at.map(|e| e.to_rfc3339()),
                            "old_key_prefix": pre_key_prefix,
                            "new_key_prefix": new_prefix,
                            "key_changed": pre_key_prefix != Some(new_prefix),
                        })),
                    );
                    tracing::info!(expires_at = ?auth.expires_at, "auth.refresh.success");
                    self.refresh_notify.notify_waiters();
                    Ok(auth)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "auth: failed to persist refreshed token");
                    kigi_log::unified_log::warn(
                        "auth.refresh.persist_failed",
                        None,
                        Some(serde_json::json!({ "error": format!("{e}") })),
                    );
                    Err(AuthError::transient_source(e))
                }
            },
            RefreshOutcome::PermanentFailure {
                error,
                rejected_refresh_token,
            } => {
                tracing::warn!(reason = ?error.reason, "auth.refresh.permanent_failure");
                kigi_log::unified_log::warn(
                    "auth.refresh.permanent_failure",
                    None,
                    Some(serde_json::json!({
                        "reason": format!("{:?}", error.reason),
                    })),
                );
                // A sibling may have successfully refreshed while we got a 401.
                // If the persisted store has a valid token, adopt it instead.
                if let Some(refreshed) = self.try_adopt_disk_token(
                    reason,
                    "auth: adopted sibling token after PermanentFailure",
                ) {
                    return Ok(refreshed);
                }
                if self.sibling_has_different_refresh_token() {
                    tracing::info!("auth: sibling-rotation detected; demoting to transient");
                    return Err(AuthError::transient(format!("sibling-rotation: {error}")));
                }
                // No clear: the tombstone (+ cooldown) gates re-attempts; the
                // dead bearer is dropped only on explicit logout. Key on the
                // refresh token the refresher actually sent, falling back to
                // our own resolution when the authority has no key.
                let failed_reason = error.reason;
                if let Some(key) = rejected_refresh_token.or(attempted_key) {
                    self.record_permanent_failure(key, error);
                }
                Err(AuthError::permanent(failed_reason))
            }
            RefreshOutcome::TransientFailure { message } => {
                tracing::warn!(%message, "auth.refresh.transient_failure");
                kigi_log::unified_log::warn(
                    "auth.refresh.transient_failure",
                    None,
                    Some(serde_json::json!({ "message": &message })),
                );
                Err(AuthError::transient(message))
            }
        }
    }

    /// Re-read the persisted credential and update the in-memory cache (used
    /// by the refresh chains). Non-destructive: only updates in-memory if the
    /// store has a different valid token (a sibling process wrote a fresher
    /// one).
    pub(crate) fn pick_up_sibling_token(&self) {
        let auth = self.read_disk_auth_silent();
        if let Some(ref a) = auth
            && !self.is_token_expired(a)
            && self.is_different_token(a)
        {
            tracing::info!("auth: picked up sibling-written token from disk");
            kigi_log::unified_log::info(
                "auth: pick_up_sibling_token adopted",
                None,
                Some(serde_json::json!({
                    "adopted_key_prefix": token_suffix(&a.key),
                    "expires_at": a.expires_at.map(|e| e.to_rfc3339()),
                    "rt_prefix": a.refresh_token.as_deref().map(token_suffix),
                })),
            );
            self.with_inner_write(|inner| *inner = Some(a.clone()));
        }
    }

    /// Check if a candidate auth has a different token than what's in memory.
    pub(crate) fn is_different_token(&self, candidate: &KimiAuth) -> bool {
        let current_key = self.inner.read().as_ref().map(|a| a.key.clone());
        current_key.as_deref() != Some(&candidate.key)
    }

    /// Record a refresh-rejection tombstone scoped to `refresh_token_key`
    /// (the rejected refresh-token value). PRD F1: 300s cooldown; auto-clears
    /// when the persisted refresh token differs.
    pub(crate) fn record_permanent_failure(
        &self,
        refresh_token_key: String,
        error: crate::auth::error::RefreshTokenFailedError,
    ) {
        kigi_log::unified_log::warn(
            "auth.tombstone.set",
            None,
            Some(serde_json::json!({
                "reason": format!("{:?}", error.reason),
                "message": error.reason.user_message(),
                "rt_prefix": token_suffix(&refresh_token_key),
                "cooldown_seconds": PERMANENT_FAILURE_TTL.as_secs(),
            })),
        );
        tracing::warn!(
            rt_prefix = token_suffix(&refresh_token_key),
            cooldown_secs = PERMANENT_FAILURE_TTL.as_secs(),
            "auth: refresh-rejection tombstone set"
        );
        *self.permanent_failure.write() = Some(ScopedRefreshFailure {
            refresh_token_key,
            error,
            recorded_at: GateRaise::now(),
        });
    }

    /// Refresh token the tombstone is scoped to: the one a refresh for
    /// `reason` would send, via the shared [`resolve_refresh_credential`] (so
    /// record and check can't drift). Does a synchronous persisted-store read;
    /// that read is load-bearing (it detects a sibling's freshly rotated
    /// token, so an in-memory-only check could leave a stale tombstone on a
    /// now-valid credential). Called from [`Self::permanent_failure`] (only
    /// when a tombstone is stored) and once per active `refresh_chain` as the
    /// fallback tombstone key; both are pre-wire paths where the read cost is
    /// bounded.
    fn attempted_tombstone_key(&self, reason: RefreshReason) -> Option<String> {
        resolve_refresh_credential(self, self.read_disk_auth_silent(), reason)
            .and_then(|a| a.refresh_token)
    }

    /// Live tombstone for the *attempted* refresh token, or `None` once the
    /// persisted refresh token changes (another process rotated it — the
    /// tombstone auto-clears) or the 300s cooldown elapses.
    ///
    /// Cooldown expiry is judged on *both* clocks (see [`GateRaise`]): the
    /// monotonic clock pauses during a system suspend, so a wall-clock arm is
    /// required for the cooldown to elapse across sleep. Without it, a
    /// rejection cached just before the lid closes would keep
    /// short-circuiting `auth()` — surfacing "run `kigi login`" — for up to 5
    /// *awake* minutes after wake, even though the cooldown is long over. A
    /// genuine revocation simply re-caches on the next refresh attempt, so
    /// expiring "early" costs one OAuth-host roundtrip.
    pub(crate) fn permanent_failure(&self) -> Option<AuthError> {
        let (refresh_token_key, reason) = {
            let guard = self.permanent_failure.read();
            let pf = guard.as_ref()?;
            let (mono, wall) = pf.recorded_at.elapsed();
            if mono >= PERMANENT_FAILURE_TTL || wall >= PERMANENT_FAILURE_TTL {
                tracing::info!("auth: refresh-rejection tombstone cooldown elapsed");
                return None;
            }
            (pf.refresh_token_key.clone(), pf.error.reason)
        };
        // Tombstone exists: confirm it still scopes to the refresh token a
        // refresh would attempt. Guard dropped above so `inner` isn't co-held.
        // Deliberately `ServerRejected` (the widest resolution) regardless of
        // the caller's reason, so the read never misses a stored tombstone.
        let attempted = self.attempted_tombstone_key(RefreshReason::ServerRejected)?;
        if attempted != refresh_token_key {
            tracing::info!(
                "auth: refresh-rejection tombstone cleared (persisted refresh token rotated)"
            );
            return None;
        }
        Some(AuthError::permanent(reason))
    }

    /// `true` iff [`Self::permanent_failure`] has a non-expired entry. Lets
    /// callers peek the IdP verdict without touching its `message` payload.
    pub(crate) fn has_permanent_failure(&self) -> bool {
        self.permanent_failure().is_some()
    }

    /// `true` iff a [`TokenRefresher`] is wired in. `false` for static-key
    /// or pre-`configure_refresher` managers.
    pub(crate) fn has_refresher_attached(&self) -> bool {
        self.refresher.read().is_some()
    }

    /// Test-only: age the cached `permanent_failure` past its TTL so
    /// the `permanent_failure()` getter treats it as expired.
    #[cfg(test)]
    pub(crate) fn force_permanent_failure_aged_out(&self) {
        if let Some(pf) = self.permanent_failure.write().as_mut() {
            let past_ttl = PERMANENT_FAILURE_TTL + StdDuration::from_secs(1);
            // `checked_sub`: a bare `Instant - Duration` panics on a machine
            // whose monotonic clock hasn't been up for `past_ttl` yet (fresh
            // boot / fresh VM). Falling back to `now` leaves the verdict live,
            // which the asserting test will surface loudly.
            let now_mono = std::time::Instant::now();
            let now_wall = std::time::SystemTime::now();
            pf.recorded_at = GateRaise {
                mono: now_mono.checked_sub(past_ttl).unwrap_or(now_mono),
                wall: now_wall.checked_sub(past_ttl).unwrap_or(now_wall),
            };
        }
    }

    /// Test-only: simulate a system suspend between recording and reading the
    /// cached `permanent_failure` — the monotonic clock stays fresh while the
    /// wall clock is rewound past the TTL (a suspend pauses the monotonic
    /// clock, so on wake `mono` reads short while `wall` reads long).
    #[cfg(test)]
    pub(crate) fn force_permanent_failure_wall_aged_out(&self) {
        if let Some(pf) = self.permanent_failure.write().as_mut() {
            let now = std::time::SystemTime::now();
            pf.recorded_at.wall = now
                .checked_sub(PERMANENT_FAILURE_TTL + StdDuration::from_secs(1))
                .unwrap_or(now);
        }
    }

    // ── 401 recovery entry point ──────────────────────────────────────

    /// 401 recovery state machine driven by the `rejected` credential. For
    /// one-shot recovery off the live bearer, use `try_recover_unauthorized()`.
    pub(crate) fn unauthorized_recovery(
        self: &Arc<Self>,
        rejected: Option<KimiAuth>,
    ) -> crate::auth::recovery::UnauthorizedRecovery {
        crate::auth::recovery::UnauthorizedRecovery::new(self.clone(), rejected)
    }

    /// One-shot 401 recovery off the live bearer, snapshotted once so the
    /// rejected key describes one credential.
    pub(crate) async fn try_recover_unauthorized(self: &Arc<Self>) -> bool {
        let cached = self.with_inner_read(|inner| inner.cloned());
        self.unauthorized_recovery(cached).next().await.is_ok()
    }

    // ── Proactive refresh ─────────────────────────────────────────────

    /// Spawn the background refresh task (PRD F1): a fixed
    /// [`PROACTIVE_REFRESH_INTERVAL`] (60s) tick that refreshes when the
    /// remaining lifetime drops below `max(300, expires_in × 0.5)` seconds
    /// (enforced by [`Self::current`]'s dynamic threshold), plus sleep/wake
    /// detection — a tick whose wall-clock gap exceeds twice the interval
    /// forces a refresh regardless of the threshold, like kimi-cli's
    /// `refreshing()`. Cancelled via `cancel`.
    ///
    /// Idempotent: a second call on the same `Arc` is a no-op (debug
    /// log + return).
    pub(crate) fn start_proactive_refresh(self: &Arc<Self>, cancel: CancellationToken) {
        use std::sync::atomic::Ordering;
        // AcqRel/Acquire publishes the spawned task's captured Arc to
        // any thread that observes the bool as `true`; SeqCst would also
        // be correct, just slower.
        if self
            .proactive_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::debug!("auth: start_proactive_refresh already running on this Arc, ignoring");
            return;
        }
        #[cfg(test)]
        self.proactive_starts.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let wall_before = std::time::SystemTime::now();
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::debug!("auth: proactive refresh task cancelled");
                        return;
                    }
                    _ = tokio::time::sleep(PROACTIVE_REFRESH_INTERVAL) => {}
                }

                #[cfg(test)]
                this.proactive_iter_count.fetch_add(1, Ordering::SeqCst);

                // Sleep/wake detection: a 60s timer that took far longer on
                // the wall clock means the machine was suspended; force a
                // refresh so the session recovers immediately on wake.
                let elapsed = wall_before.elapsed().unwrap_or_default();
                let force = elapsed > PROACTIVE_REFRESH_INTERVAL * SLEEP_WAKE_FORCE_FACTOR;
                if force {
                    tracing::info!(
                        elapsed_secs = elapsed.as_secs(),
                        "auth: detected possible sleep/wake, forcing token refresh"
                    );
                }

                this.proactive_tick(force).await;
            }
        });
    }

    /// One iteration of the proactive refresh loop. `force` bypasses the
    /// still-valid short-circuit (sleep/wake recovery).
    pub(crate) async fn proactive_tick(self: &Arc<Self>, force: bool) {
        // Back-off guards: skip ticks that cannot make progress.
        if self.permanent_failure().is_some() {
            // Tombstone live. A sibling may have rotated the credential —
            // adopt it; otherwise wait out the cooldown.
            if self
                .try_adopt_disk_token(
                    RefreshReason::PreRequest,
                    "auth: proactive refresh adopted sibling token during tombstone cooldown",
                )
                .is_none()
            {
                tracing::debug!("auth: skipping proactive refresh, tombstone cooldown active");
            }
            return;
        }
        if !self.token_type().is_refreshable() {
            tracing::debug!("auth: skipping proactive refresh, token type is not refreshable");
            return;
        }
        if self.refresher.read().is_none() {
            tracing::debug!("auth: skipping proactive refresh, no refresher configured");
            return;
        }
        if self.is_sleep_gated() || self.is_dark_wake() {
            tracing::debug!("auth: skipping proactive refresh, sleep gate / dark wake active");
            return;
        }

        // Check the persisted store first: a sibling process may have
        // already refreshed (its rotation is adopted instead of spending
        // our refresh token).
        self.pick_up_sibling_token();
        if !force && self.current().is_some() {
            // Remaining lifetime is still above the dynamic threshold.
            tracing::debug!("auth: proactive refresh not needed (above refresh threshold)");
            return;
        }

        tracing::info!(force, "auth: proactive refresh starting");
        let result = if force {
            // ServerRejected semantics = force: the refresh chain's
            // double-check only short-circuits when another task already
            // rotated the key, never on a merely-valid cached token.
            self.refresh_chain(self.token_type(), RefreshReason::ServerRejected)
                .await
        } else {
            self.auth().await
        };
        match result {
            Ok(auth) => {
                tracing::info!("auth: proactive refresh succeeded");
                kigi_log::unified_log::info(
                    "auth: proactive refresh completed",
                    None,
                    Some(serde_json::json!({
                        "result": "success",
                        "force": force,
                        "key_prefix": token_suffix(&auth.key),
                        "expires_at": auth.expires_at.map(|e| e.to_rfc3339()),
                    })),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth: proactive refresh failed");
                kigi_log::unified_log::warn(
                    "auth: proactive refresh completed",
                    None,
                    Some(serde_json::json!({
                        "result": "failed",
                        "force": force,
                        "error": format!("{e}"),
                    })),
                );
            }
        }
    }
}

/// Bridges `Arc<AuthManager>` into the `ApiKeyProvider` trait used by
/// tool clients (web_search, embedding). Sync callers
/// get the buffered snapshot; async callers drive the refresh chain.
pub(crate) struct SharedAuthKeyProvider(pub Arc<AuthManager>);

impl kigi_tools::types::ApiKeyProvider for SharedAuthKeyProvider {
    fn current_api_key(&self) -> Option<String> {
        self.0.current_or_expired().map(|a| a.key)
    }

    fn current_api_key_async(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>> {
        let am = self.0.clone();
        Box::pin(async move { am.get_valid_token().await.ok() })
    }
}

/// Build a refreshing [`ApiKeyProvider`](kigi_tools::types::ApiKeyProvider)
/// from an `Arc<AuthManager>`.
///
/// This is the public, supported way for out-of-crate consumers (e.g. the
/// pager's voice channel) to obtain a bearer that follows the same refresh
/// chain as chat / tool traffic, rather than snapshotting a token at startup
/// (the static-snapshot bug class). The returned
/// provider resolves a fresh bearer per call via
/// [`current_api_key_async`](kigi_tools::types::ApiKeyProvider::current_api_key_async),
/// so it works for both OAuth/session (refreshes) and API-key auth.
pub fn shared_api_key_provider(
    auth_manager: Arc<AuthManager>,
) -> kigi_tools::types::SharedApiKeyProvider {
    Arc::new(SharedAuthKeyProvider(auth_manager))
}

/// Compile-time check that `AuthManager` is `Send + Sync` (so the
/// proactive refresh task and arbitrary `Arc<AuthManager>` consumers can
/// safely cross a multi-threaded executor / thread boundary). A future
/// refactor that adds a `!Send` field would otherwise fail to compile in
/// `tokio::spawn(... this.clone() ...)` with a confusing trait-bound
/// error far from the offending field.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AuthManager>();
};

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
