//! `x.ai/share_session` extension handler.
//!
//! Loads a local session, exports it, and asks the backend for a public
//! share URL.

use agent_client_protocol as acp;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::remote::client::BackendClient;
use crate::session::export::ExportedSession;
use crate::session::info::Info as SessionInfo;
use crate::session::persistence::list_summaries;
use crate::session::share::{ShareSessionRequest, ShareSessionResponse};

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/share_session" => {
            tracing::info!("handling share session request");
            handle_share_session(agent, args).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_share_session(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: ShareSessionRequest = parse_params(args)?;

    // Get auth - required for sharing.
    let auth = require_xai_auth_for_share(&agent.auth_manager)?;

    // Remote settings / feature-flag gate: sharing_enabled defaults to false
    // and is only enabled for eligible accounts.
    let sharing_enabled = agent
        .cfg
        .borrow()
        .remote_settings
        .as_ref()
        .and_then(|rs| rs.sharing_enabled)
        .unwrap_or(false);
    if !sharing_enabled {
        return Err(
            acp::Error::invalid_params().data("Session sharing is not available for your account.")
        );
    }

    // Find session info by searching through summaries
    let summaries = list_summaries(None).await.map_err(|e| {
        acp::Error::internal_error().data(format!("Failed to list sessions: {}", e))
    })?;

    let summary = summaries
        .iter()
        .find(|s| s.info.id.0.as_ref() == request.session_id.as_str())
        .ok_or_else(|| acp::Error::resource_not_found(Some("Session not found".into())))?;

    let info = SessionInfo {
        id: acp::SessionId::new(request.session_id.clone()),
        cwd: summary.info.cwd.clone(),
    };

    // Load and export session
    let exported = ExportedSession::from_local_session(&info)
        .await
        .map_err(|e| acp::Error::internal_error().data(format!("Failed to load session: {}", e)))?;

    // Check for empty session
    if exported.messages.is_empty() {
        return Err(acp::Error::invalid_params().data("No messages to share yet"));
    }

    // Upload to backend and get share URL.
    let client = BackendClient::new().with_auth_manager(agent.auth_manager.clone());
    let agent_id = crate::util::agent_id::agent_id();
    let share_url = client
        .share_session(&exported, &agent_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to share session with backend");
            acp::Error::internal_error().data(format!("Failed to share session: {}", e))
        })?;

    let response = ShareSessionResponse { share_url };
    to_raw_response(&response)
}

fn require_xai_auth_for_share(
    auth_manager: &crate::auth::AuthManager,
) -> Result<crate::auth::KimiAuth, acp::Error> {
    super::auth_gate::require_xai_auth(
        auth_manager,
        "Authentication required to share session",
        "Share session is disabled. Run `grok login` to authenticate.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::KimiCodeConfig;
    use crate::auth::{AuthMode, KimiAuth};
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_auth_manager_with_token_expiring_in(
        ttl: Duration,
    ) -> (Arc<crate::auth::AuthManager>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir for share auth test");
        let mgr = Arc::new(crate::auth::AuthManager::new(
            dir.path(),
            KimiCodeConfig::default(),
        ));

        let expires_at = Utc::now() + ttl;

        // We must explicitly set oidc_issuer to a first-party xAI issuer.
        // Only OIDC tokens against https://auth.x.ai (or the local-dev equivalent)
        // return true from is_xai_auth(). This is required for the share tests to
        // exercise the happy path through require_xai_auth_for_share.
        let auth = KimiAuth {
            auth_mode: AuthMode::OAuth,
            key: "test-key".into(),
            expires_at: Some(expires_at),
            create_time: Utc::now() - Duration::hours(1),
            ..Default::default()
        };
        mgr.hot_swap(auth);
        (mgr, dir)
    }

    #[test]
    fn share_works_outside_the_5m_early_invalidation_window() {
        let (mgr, _dir) = make_auth_manager_with_token_expiring_in(Duration::minutes(10));
        assert!(mgr.current().is_some());
        assert!(require_xai_auth_for_share(&mgr).is_ok());
    }

    #[test]
    fn share_succeeds_inside_the_5m_early_invalidation_window() {
        let (mgr, _dir) = make_auth_manager_with_token_expiring_in(Duration::seconds(1));
        // This is exactly the state that triggered the user bug:
        assert!(
            mgr.current().is_none(),
            "current() drops the token inside the buffer"
        );
        assert!(mgr.expired_auth().is_some());

        // Now that we use current_or_expired(), this passes.
        let res = require_xai_auth_for_share(&mgr);
        assert!(
            res.is_ok(),
            "require_xai_auth_for_share must succeed for a still-valid buffered xAI token"
        );
    }

    #[test]
    fn share_fails_with_no_auth_at_all() {
        let dir = tempdir().expect("tempdir");
        let mgr = Arc::new(crate::auth::AuthManager::new(
            dir.path(),
            KimiCodeConfig::default(),
        ));
        assert!(require_xai_auth_for_share(&mgr).is_err());
    }

    #[test]
    fn share_rejects_non_xai_auth_with_actionable_grok_login_message() {
        let dir = tempdir().expect("tempdir");
        let mgr = Arc::new(crate::auth::AuthManager::new(
            dir.path(),
            KimiCodeConfig::default(),
        ));

        // API key is the simplest non-xAI credential (External and enterprise OIDC
        // are also rejected the same way).
        let non_xai = KimiAuth {
            auth_mode: AuthMode::ApiKey,
            key: "xai-test-key".into(),
            create_time: Utc::now(),
            ..Default::default()
        };
        mgr.hot_swap(non_xai);

        let err = require_xai_auth_for_share(&mgr)
            .expect_err("non-xAI accounts (API key, External, enterprise IdP) must be rejected");

        // This is the key assertion the review asked for: we must test the *exact*
        // actionable data string for the non-xAI path (distinct from the generic
        // "Authentication required to share session" path).
        let serialized =
            serde_json::to_value(&err).expect("acp::Error serializes to JSON-RPC shape");
        let data = serialized
            .get("data")
            .and_then(|v| v.as_str())
            .expect("auth_required error carries a data string");

        assert_eq!(
            data,
            "Share session is disabled. Run `grok login` to authenticate."
        );
    }
}
