//! Tests for the `/usage` view.

use super::*;

/// Text of the last System block in agent 0's scrollback. Panics if the
/// last block is not a System message.
fn last_system_text(app: &AppView) -> String {
    let agent = app.agents.get(&AgentId(0)).unwrap();
    let idx = agent.scrollback.len() - 1;
    match &agent.scrollback.entry(idx).unwrap().block {
        crate::scrollback::block::RenderBlock::System(b) => b.text.clone(),
        other => panic!("expected System block, got {other:?}"),
    }
}

// ── /usage dispatch tests ───────────────────────────────────

#[test]
fn show_usage_returns_fetch_usage_effect() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert_eq!(effects.len(), 1, "got: {effects:?}");
    assert!(
        matches!(&effects[0], Effect::FetchUsage { agent_id } if *agent_id == AgentId(0)),
        "effect should be FetchUsage for the active agent, got: {effects:?}"
    );
}

#[test]
fn usage_fetched_renders_rows_with_percent_left_and_reset_hint() {
    use kigi_shell::extensions::billing::UsageRow;
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch(
        Action::TaskComplete(TaskResult::UsageFetched {
            agent_id: AgentId(0),
            result: Ok(vec![
                UsageRow {
                    label: "Weekly limit".into(),
                    used: 250,
                    limit: 1000,
                    reset_hint: Some("resets in 2d 1h".into()),
                },
                UsageRow {
                    label: "5h limit".into(),
                    used: 20,
                    limit: 50,
                    reset_hint: None,
                },
            ]),
        }),
        &mut app,
    );
    assert_eq!(agent_scrollback_len(&app), before + 1);
    let text = last_system_text(&app);
    assert!(text.contains("API Usage"), "got: {text}");
    assert!(
        text.contains("Weekly limit") && text.contains("75% left"),
        "summary row shows remaining percent: {text}"
    );
    assert!(
        text.contains("(resets in 2d 1h)"),
        "reset hint kept: {text}"
    );
    assert!(
        text.contains("5h limit") && text.contains("60% left"),
        "limit row shows remaining percent: {text}"
    );
}

#[test]
fn usage_fetched_zero_limit_row_renders_zero_percent_left() {
    use kigi_shell::extensions::billing::UsageRow;
    let mut app = test_app_with_agent();
    dispatch(
        Action::TaskComplete(TaskResult::UsageFetched {
            agent_id: AgentId(0),
            result: Ok(vec![UsageRow {
                label: "RPM".into(),
                used: 12,
                limit: 0,
                reset_hint: None,
            }]),
        }),
        &mut app,
    );
    let text = last_system_text(&app);
    assert!(text.contains("0% left"), "no invented percentage: {text}");
}

#[test]
fn usage_fetched_empty_rows_shows_no_data_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch(
        Action::TaskComplete(TaskResult::UsageFetched {
            agent_id: AgentId(0),
            result: Ok(vec![]),
        }),
        &mut app,
    );
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert_eq!(last_system_text(&app), "No usage data available.");
}

#[test]
fn usage_fetched_error_pushes_error_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    dispatch(
        Action::TaskComplete(TaskResult::UsageFetched {
            agent_id: AgentId(0),
            result: Err("Usage endpoint not available. Try Kimi for Coding.".into()),
        }),
        &mut app,
    );
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert_eq!(
        last_system_text(&app),
        "Couldn't fetch usage: Usage endpoint not available. Try Kimi for Coding."
    );
}

/// Regression: genuinely unknown (non-restricted) commands keep the
/// PassThrough behavior shell/ACP commands rely on.
#[test]
fn unknown_non_restricted_command_still_passes_through() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .set_restricted_commands(&["imagine".to_string()]);

    let effects = dispatch(Action::SendPrompt("/frobnicate arg".into()), &mut app);

    assert_eq!(effects.len(), 1);
    assert!(
        matches!(&effects[0], Effect::SendPrompt { text, .. } if text == "/frobnicate arg"),
        "unknown command must still pass through: {effects:?}"
    );
    assert!(
        app.agents[&id].question_view.is_none(),
        "no upsell for genuinely unknown commands"
    );
}
