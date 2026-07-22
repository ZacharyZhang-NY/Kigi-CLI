mod generic_refresher;
mod kimi_refresher;

use std::sync::Arc;

use crate::auth::manager::AuthManager;
pub(crate) use crate::auth::manager::RefreshReason;
use crate::auth::model::KimiAuth;

pub(crate) use generic_refresher::GenericDeviceRefresher;
pub(crate) use kimi_refresher::KimiRefresher;

/// Read-only view of `AuthManager` for refreshers. Enforces the
/// no-mutation contract on *credential* state at the type level: refreshers
/// hold `Arc<dyn AuthSnapshot>` and physically cannot call `update()`,
/// `clear()`, `hot_swap()`, or `refresh_chain()`.
pub(crate) trait AuthSnapshot: Send + Sync {
    /// Read the current in-memory bearer outside the refresh threshold.
    fn current(&self) -> Option<KimiAuth>;
    /// Read the expired in-memory bearer (for its `refresh_token`).
    fn expired_auth(&self) -> Option<KimiAuth>;
    /// Re-read the persisted credential (keyring → file) for the configured
    /// scope. Read-only w.r.t. credentials, but may advance disk-observation
    /// state and emit transition telemetry (not credential mutation).
    fn read_disk_auth(&self) -> Option<KimiAuth>;
    /// Whether the in-memory bearer is expired.
    fn is_expired(&self) -> bool;
}

impl AuthSnapshot for AuthManager {
    fn current(&self) -> Option<KimiAuth> {
        self.current()
    }
    fn expired_auth(&self) -> Option<KimiAuth> {
        self.expired_auth()
    }
    fn read_disk_auth(&self) -> Option<KimiAuth> {
        self.read_disk_auth()
    }
    fn is_expired(&self) -> bool {
        self.is_expired()
    }
}

/// The credential a refresh would send to the OAuth host: persisted
/// refresh-token first, then the expired in-mem bearer, then current (only on
/// `ServerRejected`). Single source of truth shared by
/// [`KimiRefresher::refresh`] (the attempt) and
/// `AuthManager::attempted_tombstone_key` (the tombstone scope), so the two
/// can't drift. The caller supplies the persisted read: the tombstone path
/// passes a side-effect-free read, the refresher the observing one.
pub(crate) fn resolve_refresh_credential(
    snap: &dyn AuthSnapshot,
    disk_auth: Option<KimiAuth>,
    reason: RefreshReason,
) -> Option<KimiAuth> {
    disk_auth
        .filter(|a| a.refresh_token.is_some())
        .or_else(|| snap.expired_auth())
        .or_else(|| {
            (reason == RefreshReason::ServerRejected)
                .then(|| snap.current())
                .flatten()
        })
}

/// Outcome of a refresh attempt. Data only -- `refresh_chain` handles mutations.
#[derive(Debug)]
#[must_use = "RefreshOutcome encodes a state transition; route it through refresh_chain"]
pub(crate) enum RefreshOutcome {
    /// Authority returned a fresh token. Caller persists via `update()`.
    Success(Box<KimiAuth>),
    /// Terminal failure (401/403 from the OAuth host). Caller records a
    /// tombstone scoped to the rejected refresh token; the 300s cooldown (or
    /// a rotated persisted refresh token) clears it.
    PermanentFailure {
        error: crate::auth::error::RefreshTokenFailedError,
        /// The refresh-token value the refresher actually sent, so
        /// `refresh_chain` scopes the tombstone to it. `None` when the
        /// attempt never reached the wire.
        rejected_refresh_token: Option<String>,
    },
    /// Transient / unknown failure. Caller may retry later. Message-only: the
    /// underlying cause is logged structurally at the refresher, then flattened
    /// here (the retry decision needs recoverability, not the source chain).
    TransientFailure { message: String },
}

impl RefreshOutcome {
    /// A fresh credential from the authority (hides the `Box`).
    pub(crate) fn success(auth: KimiAuth) -> Self {
        Self::Success(Box::new(auth))
    }

    /// Terminal failure for an already-classified reason against the
    /// refresh token actually sent to the OAuth host.
    pub(crate) fn permanent(
        reason: crate::auth::error::RefreshTokenFailedReason,
        rejected_refresh_token: Option<String>,
    ) -> Self {
        Self::PermanentFailure {
            error: reason.into(),
            rejected_refresh_token,
        }
    }

    /// A retryable failure carrying a diagnostic message.
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::TransientFailure {
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait TokenRefresher: Send + Sync {
    /// Attempt to obtain a fresh token from the authority.
    ///
    /// Implementations MUST NOT call auth_manager.update(), clear(),
    /// hot_swap(), or any other state-mutating method. Return the
    /// result and let refresh_chain handle all mutations.
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome;
}

/// Build the production refresher for this manager's scope. A scope that maps
/// to a generic device-code [`kigi_models::OAuthConfig`] (xai-grok) gets the
/// [`GenericDeviceRefresher`]; every other scope — Kimi Code, whose registry
/// `oauth` field is `None` by design — gets the bespoke [`KimiRefresher`]
/// against `kigi_env::oauth_host()`.
pub(crate) fn build_refresher(auth_manager: Arc<AuthManager>) -> Arc<dyn TokenRefresher> {
    match kigi_models::oauth_config_for_scope_key(auth_manager.scope()) {
        Some(cfg) => {
            let snapshot: Arc<dyn AuthSnapshot> = auth_manager;
            Arc::new(GenericDeviceRefresher::new(snapshot, cfg))
        }
        None => {
            let snapshot: Arc<dyn AuthSnapshot> = auth_manager;
            Arc::new(KimiRefresher::new(snapshot, kigi_env::oauth_host()))
        }
    }
}
