use crate::auth::model::{AuthMode, KimiAuth};

/// What kind of bearer is loaded right now. Dispatch key for
/// `auth()`, `unauthorized_recovery()`, and proactive refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenType {
    /// Kimi Code OAuth session with a refresh_token available.
    OAuthSession,
    /// OAuth session without a refresh_token (cannot be silently renewed).
    SessionNoRefresh,
    /// Plain API key (no refresh possible).
    ApiKey,
    /// No credentials loaded.
    None,
}

impl TokenType {
    /// Classify the loaded credential (pure; no manager state).
    pub(crate) fn from_auth(auth: Option<&KimiAuth>) -> Self {
        match auth {
            None => Self::None,
            Some(a) => match a.auth_mode {
                AuthMode::OAuth if a.refresh_token.is_some() => Self::OAuthSession,
                AuthMode::OAuth => Self::SessionNoRefresh,
                AuthMode::ApiKey => Self::ApiKey,
            },
        }
    }

    /// `true` for types that can be silently refreshed.
    pub(crate) fn is_refreshable(self) -> bool {
        matches!(self, Self::OAuthSession)
    }
}

#[cfg(test)]
mod tests {
    //! Per-variant matrix for `is_refreshable` and classification.
    use super::*;

    #[test]
    fn is_refreshable_matrix() {
        assert!(TokenType::OAuthSession.is_refreshable());
        assert!(!TokenType::SessionNoRefresh.is_refreshable());
        assert!(!TokenType::ApiKey.is_refreshable());
        assert!(!TokenType::None.is_refreshable());
    }

    #[test]
    fn from_auth_classifies_by_mode_and_refresh_token() {
        assert_eq!(TokenType::from_auth(None), TokenType::None);
        let with_rt = KimiAuth {
            refresh_token: Some("rt".into()),
            ..KimiAuth::test_default()
        };
        assert_eq!(
            TokenType::from_auth(Some(&with_rt)),
            TokenType::OAuthSession
        );
        let no_rt = KimiAuth::test_default();
        assert_eq!(
            TokenType::from_auth(Some(&no_rt)),
            TokenType::SessionNoRefresh
        );
        let api = KimiAuth {
            auth_mode: AuthMode::ApiKey,
            ..KimiAuth::test_default()
        };
        assert_eq!(TokenType::from_auth(Some(&api)), TokenType::ApiKey);
    }
}
