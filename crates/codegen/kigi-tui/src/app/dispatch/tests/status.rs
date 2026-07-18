//! Tests for session status and sharing dispatchers.

use super::*;

/// Regression (leader-mode turn-end race): when this client is briefly Idle
/// (`is_turn_running() == false`, `current_prompt_id` cleared) but the server
/// still has queued prompts — visible as a non-empty `shared_queue` mirror —
/// a newly-sent prompt must route to the SERVER (immediate-send), NOT be
/// locally drained as a phantom running turn. The failure mode: a
/// `send_route_plain immediate=false is_turn_running=false shared_queue_len=5`
/// path taking `local_drain`, leaving the prompt shown running on the sender
/// while it was actually queued behind the existing entries on the leader and
/// every other client.
#[test]
fn send_while_idle_with_nonempty_shared_queue_routes_to_server() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    // Two prompts already queued on the server (as a broadcast would leave
    // things): populate the authoritative map AND mirror it into the agent.
    app.push_optimistic_prompt_echo("test-session", "q1", "a", "prompt");
    app.push_optimistic_prompt_echo("test-session", "q2", "b", "prompt");
    {
        let snapshot = app.shared_prompt_queue("test-session").cloned().unwrap();
        let agent = app.agents.get_mut(&id).unwrap();
        // Turn-end window: locally Idle with no current prompt, but the
        // server's queue (mirrored from the last broadcast) still has work.
        agent.session.state = AgentState::Idle;
        agent.session.current_prompt_id = None;
        agent.shared_queue = snapshot;
        assert!(agent.session.pending_prompts.is_empty());
    }

    let effects = dispatch(Action::SendPrompt("c".into()), &mut app);

    // Routed to the server (immediate-send), keyed by a fresh prompt_id.
    let pid = effects
        .iter()
        .find_map(|e| match e {
            Effect::SendPrompt {
                text, prompt_id, ..
            } if text == "c" => Some(prompt_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected immediate SendPrompt for 'c', got {effects:?}"));
    // Did NOT start a local turn or adopt "c" as the running prompt.
    assert!(
        !app.agents[&id].session.state.is_turn_running(),
        "must not promote 'c' to a local running turn"
    );
    assert!(
        app.agents[&id].session.current_prompt_id.is_none(),
        "must not set current_prompt_id locally for a server-queued prompt"
    );
    // Echoed into the shared queue BEHIND the existing entries (position 3).
    let q = app
        .shared_prompt_queue("test-session")
        .expect("optimistic echo present");
    assert_eq!(q.len(), 3, "c queued behind q1, q2");
    assert_eq!(q.last().map(|e| e.id.as_str()), Some(pid.as_str()));
    assert_eq!(q.last().map(|e| e.text.as_str()), Some("c"));
}

/// Direct unit test of the `scrub_error_for_toast` helper —
/// pins the threshold and the fallback string against drift.
#[test]
fn scrub_error_for_toast_unit() {
    // Empty + short messages pass through.
    assert_eq!(scrub_error_for_toast(""), "");
    assert_eq!(scrub_error_for_toast("ok"), "ok");
    assert_eq!(scrub_error_for_toast("network timeout"), "network timeout");
    // At-threshold (120 chars) still passes through.
    let len_120 = "x".repeat(120);
    assert_eq!(scrub_error_for_toast(&len_120), len_120);
    // Over-threshold (121 chars) triggers scrub.
    let len_121 = "x".repeat(121);
    assert_eq!(
        scrub_error_for_toast(&len_121),
        "server error (see logs for details)"
    );
    // Control chars trigger scrub even at short lengths.
    assert_eq!(
        scrub_error_for_toast("hi\nthere"),
        "server error (see logs for details)"
    );
    assert_eq!(
        scrub_error_for_toast("hi\rthere"),
        "server error (see logs for details)"
    );
    // Format-category (Cf) chars also trigger scrub — bidi
    // overrides, zero-width joiner / space, BOM. Prevents
    // Trojan-Source-style visual spoofing
    // where a toast READS as one thing but bytes encode
    // another via embedded RIGHT-TO-LEFT-OVERRIDE.
    assert_eq!(
        scrub_error_for_toast("opt\u{202E}-out"),
        "server error (see logs for details)",
        "RIGHT-TO-LEFT OVERRIDE (U+202E) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("opt\u{200B}out"),
        "server error (see logs for details)",
        "ZERO WIDTH SPACE (U+200B) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("\u{FEFF}leading BOM"),
        "server error (see logs for details)",
        "BOM (U+FEFF) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("zwj\u{200D}joiner"),
        "server error (see logs for details)",
        "ZERO WIDTH JOINER (U+200D) must be scrubbed",
    );
}

#[test]
fn dispatch_rename_session_updates_display_name_locally() {
    let mut app = test_app_with_agent();
    let effects = dispatch_rename_session(&mut app, "renamed via slash".into());
    assert_eq!(effects.len(), 1);
    assert_eq!(
        app.agents[&AgentId(0)].display_name.as_deref(),
        Some("renamed via slash"),
        "/rename must also update local display_name cache"
    );
}

/// `ConfirmResetSetting { choice: Reset }` on a SHARED Bool
/// target restores the Settings modal AND fires the typed
/// `Action::SetCompactMode(default)` via recursive dispatch —
/// the `Effect::PersistSetting` is the externally-observable
/// signal. Also asserts the ui_snapshot was
/// refreshed to the new (post-reset) value (symmetric with the
/// Cancel test's snapshot assertion).
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_bool() {
    use crate::settings::SettingValue;
    use crate::views::modal::{ActiveModal, ResetSettingsResult};
    let mut app = test_app_with_agent();
    // Flip compact_mode to true so we can observe the reset back
    // to its default (false).
    let _ = dispatch(Action::SetCompactMode(true), &mut app);
    assert!(app.current_ui.compact_mode);

    setup_reset_confirm_open(&mut app, "compact_mode");

    let effects = dispatch(
        Action::ConfirmResetSetting {
            choice: ResetSettingsResult::Reset,
        },
        &mut app,
    );

    // Recursive dispatch into Action::SetCompactMode(false) emits
    // the persist effect.
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::PersistSetting { key, value, .. } => {
            assert_eq!(*key, "compact_mode");
            assert_eq!(value, &SettingValue::Bool(false));
        }
        other => panic!("expected PersistSetting, got {other:?}"),
    }
    // In-memory state is reset to the default.
    assert!(!app.current_ui.compact_mode);
    // Modal is restored AND ui_snapshot reflects the new value
    // (symmetric with the Cancel test).
    let agent = app.agents.get(&AgentId(0)).expect("agent must exist");
    match &agent.active_modal {
        Some(ActiveModal::Settings { state }) => {
            assert!(
                !state.ui_snapshot.compact_mode,
                "ui_snapshot must reflect the post-reset value"
            );
        }
        _ => panic!("Reset branch must restore the Settings modal"),
    }
}

/// `ConfirmResetSetting { choice: Reset }` on a SHARED Enum
/// target (`theme`) dispatches `Action::SetTheme(default)` via
/// recursive dispatch — verifies the action_for_reset Enum arm.
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_enum() {
    use crate::settings::SettingValue;
    use crate::views::modal::ResetSettingsResult;
    let mut app = test_app_with_agent();
    // Flip theme to a non-default first.
    let _ = dispatch(Action::SetTheme("tokyonight".to_string()), &mut app);
    assert_eq!(app.current_ui.theme.as_deref(), Some("tokyonight"));

    setup_reset_confirm_open(&mut app, "theme");

    let effects = dispatch(
        Action::ConfirmResetSetting {
            choice: ResetSettingsResult::Reset,
        },
        &mut app,
    );

    // Reset → SetTheme("kiginight") (the registered default).
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::PersistSetting { key, value, .. } => {
            assert_eq!(*key, "theme");
            assert_eq!(value, &SettingValue::Enum("kiginight"));
        }
        other => panic!("expected PersistSetting, got {other:?}"),
    }
    assert_eq!(app.current_ui.theme.as_deref(), Some("kiginight"));
}

#[test]
fn show_usage_on_welcome_screen_is_noop() {
    let mut app = test_app();
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(
        effects.is_empty(),
        "ShowUsage with no active agent should be a no-op"
    );
}

// ── Minimal update-notice tests ──────────────────────────────────────

#[test]
fn minimal_update_notice_commits_a_system_block() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    commit_minimal_update_notice(&mut app, "9.9.9");
    assert_eq!(agent_scrollback_len(&app), before + 1);
    let text = last_system_text(&app, AgentId(0));
    assert!(text.contains("Update available: v9.9.9"), "got: {text:?}");
    assert!(text.contains("restart to apply"), "got: {text:?}");
}

#[test]
fn minimal_update_notice_no_active_agent_is_noop() {
    let mut app = test_app();
    // Must not panic and must not require an agent.
    commit_minimal_update_notice(&mut app, "9.9.9");
}
