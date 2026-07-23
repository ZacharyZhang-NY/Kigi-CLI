use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("Not logged in. Run `kigi login`.")]
    NotLoggedIn,

    #[error("Token expired. Run `kigi login` to re-authenticate.")]
    TokenExpiredNoRefresh,

    /// Server rejected the token (401) with no recovery path.
    #[error("Authentication rejected by server. Run `kigi login` to re-authenticate.")]
    ServerRejectedNoRecovery,

    #[error("Auth recovery exhausted; re-authentication required.")]
    RecoveryExhausted,

    /// Outcome of a refresh-authority attempt. Recoverability (and, for
    /// permanent failures, the reason) lives in [`RefreshTokenError`].
    #[error(transparent)]
    Refresh(#[from] RefreshTokenError),
}

/// Recoverability axis of a token-refresh attempt. Deliberately total (no
/// `#[non_exhaustive]`): "permanent vs transient" is a closed decision every
/// caller must make, so a future third state should break consumers loudly.
#[derive(Debug, Error)]
pub enum RefreshTokenError {
    /// The credential was rejected; the tombstone cooldown gates re-attempts.
    #[error(transparent)]
    Permanent(#[from] RefreshTokenFailedError),
    /// Network / 5xx / unknown blip; safe to retry later. Carries the cause.
    #[error(transparent)]
    Transient(RefreshTransientError),
}

/// A retryable refresh failure, wrapping its cause. No public `From`:
/// construct only via [`AuthError::transient`] /
/// [`AuthError::transient_source`], so a stray `?` on some error can't
/// silently classify a permanent failure as retryable. Display frames the
/// cause as an auth-refresh failure so internal messages (lock timeout,
/// sleep defer) don't surface bare.
#[derive(Debug, Error)]
#[error("auth refresh failed: {0}")]
pub struct RefreshTransientError(#[source] Box<dyn std::error::Error + Send + Sync>);

/// A terminal refresh failure. `reason` is machine-readable; the user-facing
/// copy is derived from it via [`RefreshTokenFailedReason::user_message`], so
/// the two can never drift.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", .reason.user_message())]
#[non_exhaustive]
pub struct RefreshTokenFailedError {
    pub reason: RefreshTokenFailedReason,
}

impl From<RefreshTokenFailedReason> for RefreshTokenFailedError {
    fn from(reason: RefreshTokenFailedReason) -> Self {
        Self { reason }
    }
}

/// Why a token refresh terminally failed. Both reasons carry the same
/// tombstone semantics (PRD F1): a 300s cooldown scoped to the rejected
/// refresh token, auto-cleared when the persisted refresh token differs
/// (another process rotated) or a fresh login lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshTokenFailedReason {
    /// The OAuth host answered 401/403 — the refresh token is no longer
    /// valid (expired, reused, or revoked).
    RefreshTokenRejected,
    /// Non-retryable terminal failure that isn't an explicit rejection
    /// (malformed payload, unexpected 4xx).
    Other,
}

impl RefreshTokenFailedReason {
    /// User-facing copy for a terminal refresh failure; the raw wire detail
    /// stays in logs.
    pub(crate) fn user_message(self) -> &'static str {
        match self {
            Self::RefreshTokenRejected => {
                "Your session has expired. Run `kigi login` to sign in again."
            }
            Self::Other => {
                "Authentication could not be refreshed. Run `kigi login` to sign in again."
            }
        }
    }
}

impl AuthError {
    /// A retryable refresh failure with a message-only cause, for the genuinely
    /// message-only sites (lock timeout, sleep/dark-wake defer, no refresher);
    /// use [`Self::transient_source`] when a real error is in hand.
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::transient_source(message.into())
    }

    /// A retryable refresh failure that preserves `source` in the error chain
    /// (`Transient` carries the cause), so callers with a real error don't
    /// flatten it to a string.
    pub(crate) fn transient_source(
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        AuthError::Refresh(RefreshTokenError::Transient(RefreshTransientError(
            source.into(),
        )))
    }

    /// A terminal refresh failure for an already-classified `reason`.
    pub(crate) fn permanent(reason: RefreshTokenFailedReason) -> Self {
        AuthError::Refresh(RefreshTokenError::Permanent(reason.into()))
    }
}
