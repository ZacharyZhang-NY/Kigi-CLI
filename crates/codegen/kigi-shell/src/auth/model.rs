//! Kimi Code auth data model: the persisted token set + expiry policy.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fallback TTL for credentials without a server-provided expiry
/// (plain API keys).
pub(crate) const TOKEN_TTL: Duration = Duration::days(30);

/// Minimum refresh threshold (PRD F1): refresh when the remaining lifetime
/// drops below `max(300, expires_in × 0.5)` seconds.
const DEFAULT_EARLY_INVALIDATION_SECS: u64 = 300;

/// Fraction of `expires_in` that drives the dynamic refresh threshold.
const REFRESH_THRESHOLD_RATIO: f64 = 0.5;

/// auth.json scope key for plain API key auth (`kigi login --api-key`, F2).
pub const API_KEY_SCOPE: &str = "kigi::api_key";

/// How this credential was obtained.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Kimi Code subscription OAuth (device-code flow).
    #[serde(rename = "oauth")]
    OAuth,
    /// Plain API key.
    ApiKey,
}

/// The Kimi Code credential: the OAuth token set (or a bare API key) plus
/// local bookkeeping. The Kimi token response carries no user info; `user_id`
/// / `email` stay empty until a later feature surfaces account info.
#[derive(Clone, Serialize, Deserialize)]
pub struct KimiAuth {
    /// The bearer sent on API calls (`Authorization: Bearer {key}`):
    /// the OAuth access token, or the API key in `ApiKey` mode.
    pub key: String,
    pub auth_mode: AuthMode,
    pub create_time: DateTime<Utc>,
    /// Account id — the Kimi token response has none; empty until a later
    /// feature surfaces it.
    #[serde(default)]
    pub user_id: String,
    /// Account email — the Kimi token response has none; `None` until a
    /// later feature surfaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// OAuth refresh token; `None` for API keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// `create_time + expires_in`, computed when the token was minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Server-reported token lifetime in seconds; drives the dynamic
    /// refresh threshold `max(300, expires_in × 0.5)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    /// OAuth scope string as returned by the token endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Token type as returned by the token endpoint (e.g. "bearer").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
}

impl std::fmt::Debug for KimiAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KimiAuth")
            .field("key", &token_suffix(&self.key))
            .field("auth_mode", &self.auth_mode)
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_deref().map(token_suffix),
            )
            .finish_non_exhaustive()
    }
}

impl KimiAuth {
    /// Seconds since this credential was minted. Negative when the local
    /// clock stepped back past `create_time` (NTP correction, VM restore).
    pub(crate) fn mint_age_seconds(&self) -> i64 {
        Utc::now()
            .signed_duration_since(self.create_time)
            .num_seconds()
    }

    /// `true` for a refreshable subscription session (vs a bare API key).
    pub fn is_session_auth(&self) -> bool {
        self.auth_mode == AuthMode::OAuth
    }
}

impl Default for KimiAuth {
    fn default() -> Self {
        Self {
            key: String::new(),
            auth_mode: AuthMode::OAuth,
            create_time: Utc::now(),
            user_id: String::new(),
            email: None,
            refresh_token: None,
            expires_at: None,
            expires_in: None,
            scope: None,
            token_type: None,
        }
    }
}

#[cfg(test)]
impl KimiAuth {
    /// A `KimiAuth` with sensible defaults for tests. Override fields with
    /// struct update syntax:
    /// ```ignore
    /// KimiAuth { key: "my-key".into(), ..KimiAuth::test_default() }
    /// ```
    pub fn test_default() -> Self {
        Self {
            key: "test-key".into(),
            user_id: "test-user".into(),
            ..Default::default()
        }
    }
}

pub(crate) type AuthStore = BTreeMap<String, KimiAuth>;

/// Last 12 chars of a token string, safe for diagnostic logging. Uses the
/// tail because token prefixes are shared across a family; the tail is
/// unique per token and makes `key_changed` diagnostics meaningful.
pub(crate) fn token_suffix(t: &str) -> &str {
    let len = t.len();
    if len > 12 { &t[len - 12..] } else { t }
}

/// Look up auth from the store by scope key.
pub fn lookup_auth(map: &AuthStore, scope: &str) -> Option<KimiAuth> {
    map.get(scope).cloned()
}

/// Minimum refresh-threshold component. Override with
/// `KIGI_AUTH_EARLY_INVALIDATION_SECS` for testing (e.g. `=5` to shrink the
/// buffer to 5 seconds).
pub(super) fn early_invalidation() -> Duration {
    std::env::var("KIGI_AUTH_EARLY_INVALIDATION_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| Duration::seconds(s as i64))
        .unwrap_or_else(|| Duration::seconds(DEFAULT_EARLY_INVALIDATION_SECS as i64))
}

/// Dynamic refresh threshold (PRD F1): `max(min_threshold, expires_in × 0.5)`
/// where `min_threshold` defaults to 300s. Credentials without a positive
/// `expires_in` use the minimum alone.
pub(crate) fn refresh_threshold(auth: &KimiAuth) -> Duration {
    let min = early_invalidation();
    match auth.expires_in {
        Some(expires_in) if expires_in > 0 => {
            let ratio = Duration::seconds((expires_in as f64 * REFRESH_THRESHOLD_RATIO) as i64);
            std::cmp::max(min, ratio)
        }
        _ => min,
    }
}

/// Whether the credential is inside its refresh threshold (i.e. should be
/// treated as expiring-soon for refresh scheduling).
pub(crate) fn is_expired(auth: &KimiAuth) -> bool {
    is_expired_with_buffer(auth, refresh_threshold(auth))
}

/// Like [`is_expired`] but with an explicit pre-expiry buffer. Pass
/// `Duration::zero()` for actual (hard) expiry — the instant the token would
/// really be rejected on the wire.
pub(crate) fn is_expired_with_buffer(auth: &KimiAuth, buffer: Duration) -> bool {
    if let Some(expires_at) = auth.expires_at {
        Utc::now() >= (expires_at - buffer)
    } else {
        let age = Utc::now().signed_duration_since(auth.create_time);
        age >= (TOKEN_TTL - buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with_lifetime(expires_in: i64, remaining_secs: i64) -> KimiAuth {
        KimiAuth {
            expires_in: Some(expires_in),
            expires_at: Some(Utc::now() + Duration::seconds(remaining_secs)),
            refresh_token: Some("rt".into()),
            ..KimiAuth::test_default()
        }
    }

    /// PRD threshold math: `max(300, expires_in × 0.5)`.
    #[test]
    fn refresh_threshold_is_max_of_min_and_half_life() {
        // Short-lived token: the 300s floor wins (600 × 0.5 = 300 → tie; 400 × 0.5 = 200 < 300).
        let short = auth_with_lifetime(400, 400);
        assert_eq!(refresh_threshold(&short).num_seconds(), 300);
        // Long-lived token: half the lifetime wins (7200 × 0.5 = 3600).
        let long = auth_with_lifetime(7200, 7200);
        assert_eq!(refresh_threshold(&long).num_seconds(), 3600);
        // No expires_in: the floor alone.
        let bare = KimiAuth::test_default();
        assert_eq!(refresh_threshold(&bare).num_seconds(), 300);
        // Non-positive expires_in must not produce a negative threshold.
        let broken = KimiAuth {
            expires_in: Some(-5),
            ..KimiAuth::test_default()
        };
        assert_eq!(refresh_threshold(&broken).num_seconds(), 300);
    }

    /// A token past its dynamic threshold counts as expiring-soon while a
    /// token comfortably before it does not.
    #[test]
    fn is_expired_uses_dynamic_threshold() {
        // 7200s lifetime → threshold 3600s. 3000s remaining < 3600 → expiring.
        assert!(is_expired(&auth_with_lifetime(7200, 3000)));
        // 5000s remaining > 3600 → fresh.
        assert!(!is_expired(&auth_with_lifetime(7200, 5000)));
        // Hard expiry ignores the buffer entirely.
        assert!(!is_expired_with_buffer(
            &auth_with_lifetime(7200, 3000),
            Duration::zero()
        ));
        assert!(is_expired_with_buffer(
            &auth_with_lifetime(7200, -1),
            Duration::zero()
        ));
    }

    /// Credentials without `expires_at` (API keys) age out via the 30-day TTL.
    #[test]
    fn no_expiry_falls_back_to_token_ttl() {
        let fresh = KimiAuth::test_default();
        assert!(!is_expired(&fresh));
        let old = KimiAuth {
            create_time: Utc::now() - Duration::days(31),
            ..KimiAuth::test_default()
        };
        assert!(is_expired(&old));
    }

    #[test]
    fn lookup_auth_finds_scope_entry() {
        let mut map = AuthStore::new();
        map.insert("oauth/kimi-code".into(), KimiAuth::test_default());
        assert!(lookup_auth(&map, "oauth/kimi-code").is_some());
        assert!(lookup_auth(&map, "other").is_none());
    }

    #[test]
    fn debug_redacts_tokens() {
        let auth = KimiAuth {
            key: "super-secret-access-token".into(),
            refresh_token: Some("super-secret-refresh-token".into()),
            ..KimiAuth::test_default()
        };
        let debug = format!("{auth:?}");
        assert!(!debug.contains("super-secret-access-token"));
        assert!(!debug.contains("super-secret-refresh-token"));
    }

    #[test]
    fn serde_roundtrip_preserves_token_set() {
        let auth = KimiAuth {
            key: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(Utc::now()),
            expires_in: Some(3600),
            scope: Some("kimi-code".into()),
            token_type: Some("bearer".into()),
            ..KimiAuth::test_default()
        };
        let json = serde_json::to_string(&auth).unwrap();
        assert!(
            json.contains("\"oauth\""),
            "wire spelling is \"oauth\": {json}"
        );
        let back: KimiAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(back.key, "at");
        assert_eq!(back.refresh_token.as_deref(), Some("rt"));
        assert_eq!(back.expires_in, Some(3600));
        assert_eq!(back.scope.as_deref(), Some("kimi-code"));
        assert_eq!(back.token_type.as_deref(), Some("bearer"));
        assert_eq!(back.auth_mode, AuthMode::OAuth);
    }
}
