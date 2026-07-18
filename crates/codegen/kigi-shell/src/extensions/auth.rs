//! `kigi/auth/*` and legacy `kigi/{get,set}ApiKey` extension handlers.
//!
//! These methods let the client read/write the API key via the agent and
//! drive the OAuth login flow. The agent is the single source of truth for
//! `auth.json`.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::ExtMethodResult;

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "kigi/auth/getBearerToken" => handle_get_bearer_token(agent).await,
        "kigi/getApiKey" => handle_get_api_key(),
        "kigi/setApiKey" => handle_set_api_key(args),
        "kigi/auth/submit_code" => handle_submit_code(agent, args),
        "kigi/auth/get_url" => handle_get_url(agent).await,
        "kigi/auth/logout" => handle_logout(agent, args).await,
        "kigi/auth/info" => handle_info(agent),
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_get_bearer_token(agent: &MvpAgent) -> ExtResult {
    let token = match agent.auth_manager.get_valid_token().await {
        Ok(token) => Some(token),
        Err(_) => agent
            .sampling_config
            .borrow()
            .api_key
            .clone()
            .or_else(|| agent.auth_manager.current().map(|a| a.key)),
    };
    ExtMethodResult::success(serde_json::json!({ "token": token }))
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

fn handle_get_api_key() -> ExtResult {
    let key = crate::agent::auth_method::read_xai_api_key_env().ok();
    ExtMethodResult::success(serde_json::json!({ "key": key }))
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

fn handle_set_api_key(args: &acp::ExtRequest) -> ExtResult {
    let params: serde_json::Value = parse_params(args)?;
    let key = params.get("key").and_then(|v| v.as_str());
    let kigi_home = crate::util::kigi_home::kigi_home();
    if let Some(k) = key {
        if k.is_empty() {
            crate::auth::clear_api_key(&kigi_home)
                .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
            // SAFETY: ext_method is single-threaded per agent
            unsafe { std::env::remove_var("XAI_API_KEY") };
        } else {
            crate::auth::store_api_key(&kigi_home, k)
                .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
            // SAFETY: ext_method is single-threaded per agent
            unsafe { std::env::set_var("XAI_API_KEY", k) };
        }
    } else {
        crate::auth::clear_api_key(&kigi_home)
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        // SAFETY: ext_method is single-threaded per agent
        unsafe { std::env::remove_var("XAI_API_KEY") };
    }
    ExtMethodResult::success(serde_json::json!({ "ok": true }))
        .to_ext_response()
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

/// Handle auth code submission from TUI.
fn handle_submit_code(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(Deserialize)]
    struct SubmitCodeParams {
        code: String,
    }

    let params: SubmitCodeParams = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("invalid params: {e}")))?;

    let auth_code_tx = agent.auth_code_tx.borrow();
    if let Some(ref tx) = *auth_code_tx {
        tx.try_send(params.code).map_err(|e| {
            acp::Error::internal_error().data(format!("failed to submit auth code: {e}"))
        })?;
        to_raw_response(&serde_json::json!({ "submitted": true }))
    } else {
        Err(acp::Error::invalid_params().data("no pending auth session"))
    }
}

/// Awaits the auth URL from the oneshot channel (blocks until ready).
async fn handle_get_url(agent: &MvpAgent) -> ExtResult {
    let rx = agent.auth_url_rx.borrow_mut().take();
    // `None` when no URL was sent (cached creds, early error, second poll):
    // report mode as `null` rather than mislabeling it `loopback`.
    let (auth_url, mode) = match rx {
        Some(rx) => match rx.await {
            Ok(info) => (Some(info.url), Some(info.mode)),
            Err(_) => (None, None),
        },
        None => (None, None),
    };
    to_raw_response(&serde_json::json!({
        "auth_url": auth_url,
        // `external_provider` kept for older clients; `mode` is authoritative.
        "external_provider": false,
        "mode": mode.map(|m| m.as_wire_str()),
    }))
}

async fn handle_logout(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(Deserialize)]
    struct LogoutParams {
        scope: Option<String>,
    }

    let params: LogoutParams = serde_json::from_str(args.params.get())
        .map_err(|e| acp::Error::invalid_params().data(format!("invalid params: {e}")))?;

    let result = crate::auth::perform_logout(&agent.auth_manager, params.scope.as_deref())
        .map_err(|e| acp::Error::internal_error().data(format!("failed to logout: {e}")))?;
    // `auth.lifecycle` (not `auth`) avoids colliding with the pre-existing
    // per-request `AuthManager::auth()` `#[instrument]` span.
    tracing::info_span!("auth.lifecycle", action = "logout", success = true).in_scope(|| {});

    agent.models_manager.on_auth_changed().await;

    to_raw_response(&serde_json::json!({
        "ok": true,
        "was_logged_in": result.was_logged_in,
        "email": result.email,
        "api_key_still_set": result.api_key_still_set,
    }))
}

/// Returns current auth method ID and the account fields the Kimi flow
/// exposes (email/user id are empty until a later feature surfaces them).
fn handle_info(agent: &MvpAgent) -> ExtResult {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct AuthInfoResponse {
        method_id: Option<String>,
        email: Option<String>,
        user_id: Option<String>,
        auth_mode: Option<String>,
    }

    let method_id = agent
        .auth_method_id
        .load()
        .as_ref()
        .map(|m| m.0.to_string());
    let auth = agent.auth_manager.current();
    to_raw_response(&AuthInfoResponse {
        method_id,
        email: auth.as_ref().and_then(|a| a.email.clone()),
        user_id: auth
            .as_ref()
            .map(|a| a.user_id.clone())
            .filter(|id| !id.is_empty()),
        auth_mode: auth.as_ref().map(|a| format!("{:?}", a.auth_mode)),
    })
}
