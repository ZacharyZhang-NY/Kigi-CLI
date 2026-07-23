//! Minimal-mode welcome card.
//!
//! Minimal skips the full-screen welcome view, so a fresh session would
//! otherwise be invisible — you land straight at the prompt. This commits a
//! compact rounded card (logo, version, cwd, model, hint) into native
//! scrollback via [`kigi_ratatui_inline::Terminal::insert_before`], gated on an
//! `AppView` flag set at session creation and on `/new`, so it prints exactly
//! once per session.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use kigi_tui::app::PagerTerminal;
use kigi_tui::app::app_view::{ActiveView, AppView};
use kigi_tui::minimal_api;
use kigi_tui::theme::Theme;

/// Called at the top of the minimal draw, before `commit_active`, so the card
/// lands above the first conversation block in native scrollback.
pub fn maybe_commit_welcome(app: &mut AppView, terminal: &mut PagerTerminal) {
    if !minimal_api::minimal_welcome_pending(app) {
        return;
    }
    let width = terminal.viewport_area().width;
    // Too narrow for a bordered card — leave the flag pending and retry next
    // frame (e.g. during an initial 0-width probe).
    if width < 8 {
        return;
    }

    // Move the live viewport to row 0 and clear it so the card commits at the
    // top and the app owns the window; the viewport is not bottom-pinned, so
    // later commits flow downward from here. Pre-existing native scrollback is
    // untouched.
    let live_h = terminal.viewport_area().height;
    terminal.set_viewport_area(ratatui::layout::Rect {
        x: 0,
        y: 0,
        width,
        height: live_h,
    });
    let _ = terminal.clear();

    let theme = Theme::current();
    let version = kigi_version::VERSION;
    let (cwd, model) = match &app.active_view {
        ActiveView::Agent(id) => {
            let agent = app.agents.get(id);
            (
                agent
                    .map(|a| a.session.cwd.display().to_string())
                    .unwrap_or_default(),
                agent.and_then(|a| a.session.models.current_model_name()),
            )
        }
        _ => (app.cwd.display().to_string(), None),
    };

    let mut info: Vec<Line<'static>> = Vec::new();
    info.push(Line::from(vec![
        Span::styled(
            "Kigi",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  v{version}"), theme.muted()),
    ]));
    if !cwd.is_empty() {
        info.push(Line::from(Span::styled(cwd, theme.muted())));
    }
    if let Some(model) = model {
        info.push(Line::from(Span::styled(
            format!("Model · {model}"),
            theme.muted(),
        )));
    }
    info.push(Line::from(Span::styled("/help for commands", theme.dim())));

    let logo_lines = minimal_api::compact_logo_line_count();
    // The logo carries a blank separator row when present.
    let logo_block = if logo_lines > 0 { logo_lines + 1 } else { 0 };
    // Two border rows, one padding row above, the logo block, the info lines,
    // one padding row below.
    let height = 2 + 1 + logo_block + info.len() as u16 + 1;

    // Terminal-native themes carry no RGB to blend, so the border falls back to
    // the theme's own dim gray and the terminal default fg draws the chrome.
    let border_color = kigi_tui::render::color::blend_color(theme.bg_base, theme.gray_dim, 0.45)
        .unwrap_or(theme.gray_dim);

    let inserted = terminal.insert_before(height, move |buf| {
        let area = buf.area;
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .render(area, buf);

        let inner_x = area.x + 2;
        let inner_w = area.width.saturating_sub(4);
        // Top border + one row of vertical padding.
        let mut y = area.y + 2;

        if logo_lines > 0 {
            let logo_area = ratatui::layout::Rect {
                x: area.x + 1,
                y,
                width: area.width.saturating_sub(2),
                height: logo_lines,
            };
            minimal_api::render_compact_logo(logo_area, buf, &Theme::current());
            y += logo_lines + 1;
        }

        for line in &info {
            buf.set_line(inner_x, y, line, inner_w);
            y += 1;
        }
    });
    if inserted.is_err() {
        // Keep the flag pending so a failed terminal write retries on the next
        // frame instead of dropping the card forever.
        return;
    }
    minimal_api::set_minimal_welcome_pending(app, false);
    super::commit::insert_gap(terminal);
}
