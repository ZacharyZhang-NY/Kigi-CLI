//! Session status, privacy, usage, and info dispatchers.

use super::ctx::get_active_agent;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView};
use crate::notifications::{NotificationEvent, NotificationEventKind};
use crate::scrollback::block::RenderBlock;

/// Show session info: fetch via kigi/session/info and display in scrollback.
///
/// Produces Effect::ShowSessionInfo which spawns an async ACP ext request.
/// On completion, TaskResult::SessionInfoComplete shows the formatted info.
pub(super) fn dispatch_show_session_info(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        // No active session — error should have been caught by slash command,
        // but guard here just in case.
        return vec![];
    };

    vec![Effect::ShowSessionInfo {
        agent_id: id,
        session_id,
        show_resolved_model: app.show_resolved_model,
    }]
}

/// Scrub an untrusted error string for toast display. Substitutes a
/// generic placeholder when the input exceeds 120 chars or contains
/// control / bidi-override characters (prevents escape-sequence
/// injection and visual spoofing). Full error stays in tracing logs.
pub(super) fn scrub_error_for_toast(error: &str) -> String {
    const MAX_TOAST_ERROR_LEN: usize = 120;
    if error.len() > MAX_TOAST_ERROR_LEN
        || error
            .chars()
            .any(crate::render::line_utils::is_unsafe_display_char)
    {
        "server error (see logs for details)".to_string()
    } else {
        error.to_string()
    }
}

/// Show context info: fetch via kigi/session/info and display rich breakdown.
///
/// Produces Effect::ShowContextInfo which spawns an async ACP ext request.
/// On completion, TaskResult::ContextInfoComplete shows the formatted info.
pub(super) fn dispatch_show_context_info(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    vec![Effect::ShowContextInfo {
        agent_id: id,
        session_id,
    }]
}

/// `/usage` — fetch Kimi usage/quota rows and display them inline.
///
/// Produces [`Effect::FetchUsage`], which asks the shell's `kigi/billing`
/// extension (`GET {base}/usages`); [`handle_usage_fetched`] renders the
/// rows as a system block in scrollback.
pub(super) fn dispatch_show_usage(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    vec![Effect::FetchUsage { agent_id: id }]
}

/// Render the `/usage` result: the fetched quota rows (kimi-cli
/// `usage.py` semantics — label, remaining-quota bar, percent left, reset
/// hint), "No usage data available." for an empty list, or the error.
pub(super) fn handle_usage_fetched(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<Vec<kigi_shell::extensions::billing::UsageRow>, String>,
) -> Vec<Effect> {
    let msg = match &result {
        Ok(rows) if rows.is_empty() => "No usage data available.".to_string(),
        Ok(rows) => format_usage_rows(rows),
        Err(e) => format!("Couldn't fetch usage: {e}"),
    };
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.scrollback.push_block(RenderBlock::system(msg));
    }
    vec![]
}

/// Width of the remaining-quota bar, matching kimi-cli's usage panel.
const USAGE_BAR_WIDTH: usize = 20;

/// Format usage rows as aligned text lines (kimi-cli `_format_row`
/// parity): `label  [bar]  N% left  (reset hint)`. The percentage is
/// derived from `used`/`limit` only — a row without a positive limit
/// renders as 0% left with an empty bar, exactly like kimi-cli.
fn format_usage_rows(rows: &[kigi_shell::extensions::billing::UsageRow]) -> String {
    let label_width = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(6);
    let mut lines = vec!["API Usage".to_string()];
    for row in rows {
        let ratio = if row.limit <= 0 {
            0.0
        } else {
            (row.limit - row.used).clamp(0, row.limit) as f64 / row.limit as f64
        };
        let filled = (ratio * USAGE_BAR_WIDTH as f64).round() as usize;
        let filled = filled.min(USAGE_BAR_WIDTH);
        let bar: String = "\u{2588}".repeat(filled) + &"\u{2591}".repeat(USAGE_BAR_WIDTH - filled);
        let mut line = format!(
            "  {:<width$}  [{bar}]  {:.0}% left",
            row.label,
            ratio * 100.0,
            width = label_width,
        );
        if let Some(hint) = &row.reset_hint {
            line.push_str(&format!("  ({hint})"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Commit a one-line "update available" notice into the active agent's
/// scrollback. Minimal mode has no welcome screen (the full TUI's update
/// surface), so the background update check's result is shown here instead
/// No-op when there is no active agent.
pub(crate) fn commit_minimal_update_notice(app: &mut AppView, latest_version: &str) {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.scrollback.push_block(RenderBlock::system(format!(
            "Update available: v{latest_version} — restart to apply."
        )));
    }
}

/// `/queue` — commit a read-only list of the queued prompts as a system block.
/// The text is built by [`crate::app::status_blocks::queue_block_text`]; this
/// just resolves the active agent and pushes it. Works in every render mode; the
/// primary inspection surface in minimal, which has no interactive `QueuePane`.
pub(super) fn dispatch_show_queue(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::queue_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// `/tasks` — commit a read-only list of background tasks, subagents, and
/// scheduled (`/loop`) tasks as a system block. The text is built by
/// [`crate::app::status_blocks::tasks_block_text`]; this just resolves the
/// active agent and pushes it. Works in every render mode; the primary snapshot
/// surface in minimal, which has no interactive `TasksPane`.
pub(super) fn dispatch_show_tasks(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::tasks_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// Open the hidden `/gboom` easter egg as a modal over the active agent
/// view. Requires a graphics-capable terminal (kitty protocol or iTerm2);
/// otherwise a toast explains why nothing happened. On session-less
/// surfaces (dashboard, welcome) this is a silent no-op.
///
/// Targets the top-level agent view (where the prompt lives), not a
/// focused subagent view: the modal's tick/draw plumbing runs on the
/// top-level view, mirroring the video viewer.
pub(super) fn dispatch_open_gboom(app: &mut AppView) -> Vec<Effect> {
    use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if detect_graphics_protocol() == GraphicsProtocol::None {
        agent.show_toast(
            "No demons here \u{2014} GBOOM needs a graphics-capable terminal \
             (kitty, Ghostty, WezTerm, iTerm2)",
        );
        return vec![];
    }
    // Close other media modals: they share the kitty placement id. Drop the
    // image viewer's in-flight loader too (its close path clears both —
    // a leaked rx would mis-feed the next image viewer's poll loop).
    agent.image_viewer = None;
    agent.image_load_rx = None;
    agent.video_viewer = None;
    agent.gboom = Some(crate::gboom::GboomState::new());
    vec![]
}

/// Emit a `SessionReady` notification for the given agent.
///
/// Takes `&NotificationService` separately from `&AgentView` to avoid
/// borrow-checker conflicts when `agent` is borrowed from `app.agents`.
pub(super) fn notify_session_ready(
    notification_service: &crate::notifications::NotificationService,
    agent: &AgentView,
) {
    notification_service.notify(NotificationEvent {
        kind: NotificationEventKind::SessionReady,
        title: "Kigi".into(),
        body: NotificationEventKind::SessionReady.as_str().into(),
        session_id: agent.session.session_id.as_ref().map(|s| s.0.to_string()),
    });
}

// TaskResult handlers.

pub(super) fn handle_context_info_complete(
    app: &mut AppView,
    agent_id: AgentId,
    info: Box<kigi_shell::session::SessionInfoResponse>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let model = info.data.model.as_deref().unwrap_or("unknown").to_string();
        // Take ownership of the snapshot once, hand a clone to the
        // agent's running counters, then move the original into the
        // scrollback block (which keeps it for theme-reactive
        // re-rendering). This still costs one clone but reads as
        // "the agent needs a copy" rather than "the block needs a
        // copy", which matches the lifetime story.
        let snapshot = info.data.context;
        agent.apply_full_context_info(snapshot.clone());
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::context_info(
                snapshot, model,
            ));
    }
    vec![]
}

// Action handlers.

pub(super) fn dispatch_copy_session_id(app: &mut AppView, index: usize) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    // Try agent modal first, then fall back to app fields (welcome screen).
    let id = get_active_agent(app)
        .and_then(|agent| {
            if let Some(ActiveModal::SessionPicker {
                entries: Some(ref e),
                ..
            }) = agent.active_modal
            {
                e.get(index).map(|entry| entry.id.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            app.session_picker_entries
                .as_ref()
                .and_then(|s| s.get(index))
                .map(|e| e.id.clone())
        });
    if let Some(id) = id {
        let r = crate::clipboard::copy_text(&id);
        app.show_toast(r.message);
    }
    vec![]
}

pub(super) fn dispatch_show_release_notes(
    app: &mut AppView,
    title: String,
    content: String,
) -> Vec<Effect> {
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_modal = Some(crate::views::modal::ActiveModal::DocViewer {
                    title,
                    content,
                    scroll: 0,
                    window: crate::views::modal_window::ModalWindowState::new(),
                    cached_lines: None,
                    previous_palette: None,
                    standalone: true,
                });
            }
        }
        ActiveView::Welcome => {
            app.welcome_doc_viewer = Some(crate::views::modal::ActiveModal::DocViewer {
                title,
                content,
                scroll: 0,
                window: crate::views::modal_window::ModalWindowState::new(),
                cached_lines: None,
                previous_palette: None,
                standalone: true,
            });
        }
        _ => {}
    }
    vec![]
}
