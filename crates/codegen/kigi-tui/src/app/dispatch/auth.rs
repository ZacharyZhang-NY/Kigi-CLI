//! Login, logout, account switching, and auth-code submission dispatchers.

use super::ctx::{restore_auth_return_view, show_welcome};
use super::queue::maybe_drain_queue;
use super::router::dispatch;
use super::session::lifecycle::{clear_startup_actions, drain_startup_actions};
use crate::app::actions::{Action, Effect};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView, AuthMode, AuthState, PlatformLogin};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;

// ---------------------------------------------------------------------------
// Auth dispatch
// ---------------------------------------------------------------------------

/// `/logout` -- ask the shell to clear auth, then return to the login screen.
pub(super) fn dispatch_logout(_app: &mut AppView) -> Vec<Effect> {
    vec![Effect::Logout]
}

/// Ensure `login_method_id` is populated from stored auth methods.
/// On the eager-auth path (cached token), login_method_id is never set
/// because the user skipped the login screen.
///
/// Does **not** invent `kimi-code` when no interactive method is advertised
/// (e.g. `preferred_method=api_key` with no key — empty `auth_methods`).
/// Callers already surface "No login method available" when this leaves
/// `login_method_id` unset.
pub(super) fn ensure_login_method(app: &mut AppView) {
    if app.login_method_id.is_some() {
        return;
    }
    let (label, method_id, start_mode) =
        crate::acp::find_interactive_login_method(&app.auth_methods);
    if let Some(id) = method_id {
        app.login_label = label;
        app.login_method_id = Some(id);
        app.auth_start_mode = match start_mode {
            crate::acp::AuthStartMode::Pending => AuthMode::Pending,
            crate::acp::AuthStartMode::Command => AuthMode::Command,
        };
    }
    // No interactive method: leave login_method_id unset (fail-closed).
}

/// Error when no interactive login method is available (empty auth_methods).
fn no_login_method_error(_app: &AppView) -> String {
    "No login method available".to_string()
}

/// Log out, then start a new login flow in a single sequential task.
pub(super) fn dispatch_switch_account(app: &mut AppView) -> Vec<Effect> {
    ensure_login_method(app);

    let Some(method_id) = app.login_method_id.clone() else {
        app.auth_state = AuthState::Pending {
            error: Some(no_login_method_error(app)),
        };
        return vec![];
    };

    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_in_flight_method = Some(method_id.clone());
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: app.auth_start_mode,
    };

    vec![
        Effect::SwitchAccount {
            request_seq,
            method_id,
            use_oauth: app.auth_use_oauth,
        },
        Effect::PollAuthUrl { request_seq },
    ]
}

/// Scan the trailing run of session-event / system blocks for a
/// [`SessionEvent::ReAuthRequired`] prompt. Used by the `PromptResponse`
/// handler to suppress the redundant "Turn failed" block after a 401 — the
/// re-auth prompt is pushed by the `RetryState` handler, which runs first.
pub(super) fn scrollback_has_recent_reauth_prompt(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(ev.event, SessionEvent::ReAuthRequired) {
                    return true;
                }
            }
            // Tolerate interleaved system messages in the trailing run.
            Some(RenderBlock::System(_)) => {}
            // Stop at the first substantive block: any re-auth prompt for
            // this turn lives in the trailing events pushed just before the
            // PromptResponse arrived.
            _ => break,
        }
    }
    false
}

/// True if the trailing run of session/system blocks contains a terminal
/// context-overflow block ([`SessionEvent::ContextTooLarge`] or `CompactionFailed`).
/// Lets `PromptResponse` suppress the redundant `TurnFailed`, mirroring reauth.
pub(super) fn scrollback_has_recent_context_too_large(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(
                    ev.event,
                    SessionEvent::ContextTooLarge | SessionEvent::CompactionFailed { .. }
                ) {
                    return true;
                }
            }
            // Tolerate interleaved system messages in the trailing run.
            Some(RenderBlock::System(_)) => {}
            // Stop at the first substantive block.
            _ => break,
        }
    }
    false
}

/// Strip the trailing run of auth-error blocks — the `ReAuthRequired`
/// prompt plus any stale `RetryFailed` / `TurnFailed` — from an agent's
/// scrollback. Called after a successful mid-session re-auth so the prompt
/// disappears once the user returns to the session. Mirrors the
/// credit-limit upsell's stale-block strip.
pub(super) fn strip_trailing_auth_error_blocks(agent: &mut AgentView) {
    use crate::scrollback::block::RenderBlock;
    let mut to_remove = Vec::new();
    for idx in (0..agent.scrollback.len()).rev() {
        match agent.scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev))
                if matches!(
                    &ev.event,
                    SessionEvent::ReAuthRequired
                        | SessionEvent::RetryFailed { .. }
                        | SessionEvent::TurnFailed { .. }
                ) =>
            {
                to_remove.push(idx);
            }
            // Skip over other trailing session-event / system blocks.
            Some(RenderBlock::SessionEvent(_) | RenderBlock::System(_)) => continue,
            // Stop at the first substantive block.
            _ => break,
        }
    }
    for idx in to_remove {
        agent.scrollback.remove_from(idx);
    }
}

/// Start an interactive login flow. Triggered by pressing 'l' on the
/// welcome screen or by the `/login` slash command.
///
/// When invoked mid-session (the active view is an agent/dashboard rather
/// than the welcome screen), the auth UI — including the external auth
/// provider's sign-in URL and status — is only rendered by the welcome
/// view. We therefore stash the caller's view in `auth_return_view` and
/// switch to `Welcome` so the flow is actually visible; the prior view is
/// restored once auth completes or is cancelled. Without this, `/login`
/// with an external auth provider configured appeared to do nothing.
pub(super) fn dispatch_login(app: &mut AppView) -> Vec<Effect> {
    dispatch_login_with(app, None)
}

/// `/login`: land on the provider picker (welcome `Pending` state) instead
/// of auto-starting a flow. Connected providers show their badge; the user
/// picks a row, which dispatches `LoginWith` / key entry as usual. From
/// inside a session the current view is stashed exactly like
/// [`dispatch_login`], so Esc/q (`CancelLogin`) returns to it.
pub(super) fn dispatch_open_login_picker(app: &mut AppView) -> Vec<Effect> {
    if !matches!(app.active_view, ActiveView::Welcome) {
        app.auth_return_view = Some(app.active_view);
        show_welcome(app);
    }
    // Drop any stale in-flight auth result and open a fresh picker.
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.welcome_menu_index = None;
    app.welcome_menu_scroll = 0;
    app.auth_state = AuthState::Pending { error: None };
    vec![]
}

/// Start an interactive login flow with an explicitly chosen method (a
/// provider row on the login picker). `None` keeps the historical behavior:
/// re-use the current method or resolve the first interactive one.
///
/// The explicit id is resolved against the shell-advertised `auth_methods`
/// and FAILS CLOSED when absent — silently falling back to the first method
/// is exactly the bug that sent every provider row to the Kimi flow.
pub(super) fn dispatch_login_with(
    app: &mut AppView,
    method_id: Option<agent_client_protocol::AuthMethodId>,
) -> Vec<Effect> {
    if let Some(id) = method_id {
        let Some(method) = app.auth_methods.iter().find(|m| *m.id() == id) else {
            app.auth_state = AuthState::Pending {
                error: Some(format!("Login method not available: {}", id.0)),
            };
            return vec![];
        };
        // Mirror `find_interactive_login_method`: external auth providers
        // start in Command mode, everything else Pending (the mode firms up
        // when the auth URL arrives).
        let is_provider = method
            .meta()
            .as_ref()
            .and_then(|v| v.get("external_provider"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        app.login_label = Some(method.name().to_string());
        app.login_method_id = Some(method.id().clone());
        app.auth_start_mode = if is_provider {
            AuthMode::Command
        } else {
            AuthMode::Pending
        };
    }
    ensure_login_method(app);
    let Some(method_id) = app.login_method_id.clone() else {
        app.auth_state = AuthState::Pending {
            error: Some(no_login_method_error(app)),
        };
        return vec![];
    };

    // Surface the auth UI when triggered from inside a session. `show_welcome`
    // resets ephemeral state here, covering the AuthComplete / cancel-login
    // fallbacks too (`auth_return_view` is only ever set here).
    if !matches!(app.active_view, ActiveView::Welcome) {
        app.auth_return_view = Some(app.active_view);
        show_welcome(app);
    }

    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_in_flight_method = Some(method_id.clone());
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: app.auth_start_mode,
    };

    vec![
        Effect::Authenticate {
            request_seq,
            method_id,
            use_oauth: app.auth_use_oauth,
            force_interactive: true,
        },
        Effect::PollAuthUrl { request_seq },
    ]
}

/// Cancel a login that was started from inside a session and restore the
/// caller's view. Only meaningful when `auth_return_view` is set (a
/// mid-session `/login` or 401 re-auth prompt). Any in-flight auth task is
/// left to finish in the background — its `AuthComplete`/`AuthFailed`
/// result is ignored because we move `auth_state` out of `Authenticating`
/// here (the request-seq/state guard in those handlers drops stale results)
/// and bump the seq so a fresh login does not collide.
pub(super) fn dispatch_cancel_login(app: &mut AppView) -> Vec<Effect> {
    let Some(return_view) = app.auth_return_view.take() else {
        return vec![];
    };
    app.next_auth_request_seq += 1;
    app.auth_state = AuthState::Done;
    app.auth_show_raw_url = false;
    app.auth_code_input.clear();
    app.auth_in_flight_method = None;
    restore_auth_return_view(app, return_view);
    // The user bailed out of re-auth — drop stashed prompts and strip the
    // stale re-auth prompt from scrollback (on all agents: the login may
    // have been started from the dashboard). Clearing the stash alone is
    // not enough: a leftover `ReAuthRequired` block would let a later
    // `PromptResponse` re-detect it via `scrollback_has_recent_reauth_prompt`
    // and re-stash the prompt, so a subsequent unrelated login could
    // silently resubmit it. Mirrors the strip in the `AuthComplete` path.
    for agent in app.agents.values_mut() {
        agent.reauth_stashed_prompt = None;
        strip_trailing_auth_error_blocks(agent);
    }
    vec![]
}

/// User submitted a manually-pasted auth token in loopback mode.
pub(super) fn dispatch_submit_auth_code(app: &mut AppView, code: String) -> Vec<Effect> {
    let request_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => *request_seq,
        _ => return vec![],
    };

    vec![Effect::SubmitAuthCode { request_seq, code }]
}

/// A Moonshot row was selected on the welcome login picker: switch the
/// welcome screen into the API-key paste box for that platform.
pub(super) fn dispatch_begin_platform_key_entry(
    app: &mut AppView,
    target: PlatformLogin,
) -> Vec<Effect> {
    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: AuthMode::ApiKeyEntry(target),
    };
    vec![]
}

/// Esc in the API-key paste box: back to the login picker (no error line).
/// Bumps the request seq so any stale in-flight auth result is dropped by
/// the `AuthComplete`/`AuthFailed` guards.
pub(super) fn dispatch_cancel_platform_key_entry(app: &mut AppView) -> Vec<Effect> {
    if !matches!(
        app.auth_state,
        AuthState::Authenticating {
            mode: AuthMode::ApiKeyEntry(_),
            ..
        }
    ) {
        return vec![];
    }
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_state = AuthState::Pending { error: None };
    vec![]
}

/// Enter with a non-empty key in the API-key paste box: persist the key to
/// auth.json (platform-id scope), then authenticate with the platform's
/// method id (one sequential background task — see the effect handler).
/// The screen shows the connecting state while the key is validated; a
/// failure lands back on the picker with the error line (`AuthFailed`).
pub(super) fn dispatch_submit_platform_api_key(app: &mut AppView, key: String) -> Vec<Effect> {
    let (request_seq, target) = match &app.auth_state {
        AuthState::Authenticating {
            request_seq,
            mode: AuthMode::ApiKeyEntry(target),
            ..
        } => (*request_seq, *target),
        _ => return vec![],
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        return vec![];
    }
    app.auth_in_flight_method = Some(target.method_id());
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };
    vec![Effect::PersistPlatformApiKeyAndAuthenticate {
        request_seq,
        target,
        key,
    }]
}

/// Stamp `_meta.connected` on the advertised method `id` (the TUI-side
/// mirror of the shell's initialize-time stamping — see
/// `kigi_shell::agent::auth_method::stamp_connected_meta`).
fn mark_method_connected(
    methods: &mut [agent_client_protocol::AuthMethod],
    id: &agent_client_protocol::AuthMethodId,
) {
    use kigi_shell::agent::auth_method::CONNECTED_META_KEY;
    for method in methods.iter_mut() {
        if let agent_client_protocol::AuthMethod::Agent(agent) = method
            && agent.id == *id
        {
            let mut meta = agent.meta.take().unwrap_or_default();
            meta.insert(
                CONNECTED_META_KEY.to_string(),
                serde_json::Value::Bool(true),
            );
            agent.meta = Some(meta);
        }
    }
}

// TaskResult handlers.

pub(super) fn handle_auth_complete(
    app: &mut AppView,
    request_seq: u64,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    if let AuthState::Authenticating {
        request_seq: current_seq,
        ..
    } = &app.auth_state
        && *current_seq == request_seq
    {
        if let Some(meta_val) = meta.as_ref()
            && let Ok(auth_meta) =
                serde_json::from_value::<kigi_shell::auth::AuthMeta>(meta_val.clone())
        {
            app.apply_auth_meta(&auth_meta);
        }

        // The method that just authenticated is now connected — stamp the
        // advertised-methods copy so a later /login picker shows its badge
        // (initialize-time stamping only covers what was stored at startup).
        if let Some(id) = app.auth_in_flight_method.take() {
            mark_method_connected(&mut app.auth_methods, &id);
        }

        app.auth_state = AuthState::Done;
        app.auth_show_raw_url = false;
        app.welcome_prompt_focused = true;
        app.auth_code_input.clear();

        // Mid-session re-auth (`/login` or a 401 prompt): restore the
        // view the user was on instead of running the startup
        // load-session flow. The session state lives in `app.agents`,
        // independent of `active_view`, so it is preserved across the
        // auth detour.
        if let Some(return_view) = app.auth_return_view.take() {
            restore_auth_return_view(app, return_view);
            // Mid-session re-auth returns to the existing session, NOT
            // the startup flow, so discard any deferred startup stash
            // (e.g. an incidental `Ctrl+N` pressed during /login that the
            // chokepoint deferred) rather than leaving it to fire later.
            clear_startup_actions(app);
            // Re-auth succeeded — hide the now-stale re-auth prompt
            // (and any trailing error blocks) so the user returns to
            // a clean session. Mirrors the credit-limit upsell's
            // stale-block strip.
            // Auth is global, so handle every agent (the login may
            // have been started from the dashboard, not the agent
            // that 401'd).
            let mut retry_effects = Vec::new();
            for agent in app.agents.values_mut() {
                strip_trailing_auth_error_blocks(agent);
                // Auto-resubmit the prompt that failed on the expired
                // login so the user doesn't have to retype it. The
                // user couldn't have queued another prompt during the
                // auth detour, so a plain front-enqueue + drain is safe.
                if let Some(prompt) = agent.reauth_stashed_prompt.take() {
                    agent.scrollback.push_block(RenderBlock::system(
                        "Re-authenticated. Retrying\u{2026}".to_string(),
                    ));
                    agent.session.enqueue_in_flight_prompt_front(prompt);
                    retry_effects.extend(maybe_drain_queue(agent));
                }
            }
            let mut effects = dispatch(Action::RequestBundleStatus, app);
            effects.extend(retry_effects);
            return effects;
        }

        // status only; shell auto-syncs post-auth
        let mut effects = dispatch(Action::RequestBundleStatus, app);

        // Replay deferred session startup once BOTH gates are open. Auth
        // is now Done, so `session_startup_allowed()` here means "is trust
        // also resolved?" -- if trust is still Pending its question renders
        // next and its answer drains instead. Same predicate the trust
        // handlers use, so the deferred startup runs exactly once after
        // whichever gate resolves last.
        if app.session_startup_allowed() {
            effects.extend(drain_startup_actions(app));
        }
        return effects;
    }
    vec![]
}

pub(super) fn handle_auth_url_ready(
    app: &mut AppView,
    request_seq: u64,
    auth_url: Option<String>,
    external: bool,
    mode: Option<String>,
) -> Vec<Effect> {
    if let AuthState::Authenticating {
        request_seq: current_seq,
        auth_url: current_url,
        mode: current_mode,
        ..
    } = &mut app.auth_state
        && *current_seq == request_seq
    {
        *current_url = auth_url;
        // Prefer `mode`; fall back to `external` for older agents. An
        // old-agent device login lands on Loopback (harmless paste box;
        // the background poll still completes).
        *current_mode = match mode.as_deref() {
            Some("device") => AuthMode::Device,
            Some("command") => AuthMode::Command,
            Some("loopback") => AuthMode::Loopback,
            _ if external => AuthMode::Command,
            _ => AuthMode::Loopback,
        };
    }
    vec![]
}

pub(super) fn handle_mcp_auth_trigger_done(
    app: &mut AppView,
    agent_id: AgentId,
    server_name: String,
    result: Result<(), String>,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if let Some(ref mut modal) = agent.extensions_modal {
        modal.pending_action = None;
        modal.pending_entry_index = None;
        if let Err(e) = result {
            // String-match heuristic: directive vs name-embedded vs generic.
            // Brittle if the shell ever quotes a name shape that doesn't
            // match `server_name` here — replace with a structured
            // discriminator on McpAuthTriggerResponse if that happens.
            let msg = if e.starts_with("To authenticate") {
                format!("{server_name}: {e}")
            } else if e.contains(&server_name) {
                format!("Auth failed: {e}")
            } else {
                format!("{server_name} auth failed: {e}")
            };
            modal.modal_message = Some(crate::views::extensions_modal::ModalMessage::Error(msg));
            return vec![];
        }
    }
    // No toast on success: the row transition from the FetchMcpsList
    // refresh below is the confirmation.
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    vec![Effect::FetchMcpsList {
        agent_id,
        session_id,
        cache: false,
    }]
}
