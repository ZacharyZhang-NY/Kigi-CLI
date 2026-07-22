//! Welcome screen — the first thing users see.
//!
//! Layout (top to bottom):
//! - Top margin row (always preserved)
//! - Top bar: repo_root:branch (left), version (right)
//! - Vertically centered content: logo → gap → menu → gap → prompt
//! - Bottom margin

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Widget, Wrap};

use agent_client_protocol as acp;

use crate::app::app_view::{
    AuthMode, AuthState, PendingMenuItem, SessionPickerEntry, TrustState, pending_menu_items,
};
use crate::startup::StartupWarning;
use crate::theme::Theme;
use crate::views::prompt_widget::{PromptFlag, PromptInfo, PromptWidget};
mod hero_box;
pub(crate) mod logo;
mod menu;
mod prompt;
mod top_bar;

pub(crate) use logo::shimmer_frame;
use logo::{logo_line_count, render_logo};
use menu::render_menu;
pub(crate) use top_bar::location_line_at;
use top_bar::render_top_bar;

/// True for VS Code and xterm.js embeds (VS Code-family IDEs and Zed) where
/// quit is `Ctrl+D` (canonical: [`TerminalName::is_vscode_family`]).
fn welcome_in_vscode_family() -> bool {
    crate::terminal::terminal_context().brand.is_vscode_family()
}

/// Build the quit hint spans used in Authenticating sub-screens.
fn quit_hint_spans(theme: &Theme) -> Vec<Span<'static>> {
    let key = if welcome_in_vscode_family() {
        "ctrl+d"
    } else {
        "ctrl+q"
    };
    vec![
        Span::styled(
            key,
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  quit", Style::default().fg(theme.gray)),
    ]
}

/// Horizontal margin (left and right) in normal mode.
const H_MARGIN: u16 = 2;
/// Horizontal margin in compact mode.
const H_MARGIN_COMPACT: u16 = 1;

/// Minimum width for the menu section so it doesn't resize when the import row toggles.
/// Derivation: "[ " (2) + import-claude label (22) + gap (4) + "ctrl+i  [x]" (11) + " ]" (2) = 41.
/// Bumped to 51 for comfortable breathing room.
const MENU_MIN_WIDTH: u16 = 51;

/// Whether the welcome prompt is currently focused (accepting text input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WelcomePromptFocus {
    #[default]
    Unfocused,
    Focused,
}

/// Result of rendering the welcome screen.
#[derive(Default)]
pub struct WelcomeRenderResult {
    /// Cursor position (if the prompt wants a visible cursor).
    pub cursor_pos: Option<(u16, u16)>,
    /// Terminal image/cursor escapes paired with their ownership transition.
    pub post_flush_escapes: Option<crate::terminal::overlay::PostFlush>,
    /// Hit-test rects for each menu item (for click/hover).
    pub menu_rects: Vec<Rect>,
    /// Hit-test rect for the prompt input area (for click to start session).
    pub prompt_rect: Option<Rect>,
    /// Hit-test rect for the import-claude banner (for click to open import modal).
    pub import_banner_rect: Option<Rect>,
    /// Hit areas from the session picker (for mouse hit-testing).
    pub session_picker_hit_areas: Option<crate::views::picker::PickerHitAreas>,
    /// Hit-test rect for the auth copy line (click-to-copy during Authenticating).
    pub auth_url_rect: Option<Rect>,
    /// Hit-test rect for the "show full URL" fallback link.
    pub auth_fallback_rect: Option<Rect>,
    /// Login-picker menu scroll offset actually used this frame. Fed back
    /// into [`WelcomeRenderParams::menu_scroll`] next frame so the viewport
    /// only moves when the selection exits it (no jumpy re-centering).
    pub menu_scroll: usize,
}

use hero_box::HERO_BOX_MIN_WIDTH;

/// Prompt input height (shared across hero and stacked layout paths).
const PROMPT_HEIGHT: u16 = 3;
/// Gap between prompt and version line.
const VERSION_GAP: u16 = 1;

/// Computed areas for the welcome screen vertical layout.
pub(super) struct WelcomeLayout {
    pub(super) logo: Rect,
    pub(super) error: Rect,
    pub(super) menu: Rect,
    pub(super) tip: Rect,
    pub(super) prompt: Rect,
    pub(super) version: Rect,
    // Hero box sub-rects (all zero when hero box is inactive).
    pub(super) hero_box: Rect,
    pub(super) hero_logo: Rect,
    pub(super) hero_version: Rect,
    pub(super) hero_subtitle: Rect,
    pub(super) hero_menu: Rect,
}

/// Inputs to [`WelcomeLayout::compute`] / [`WelcomeLayout::compute_stacked`].
///
/// Bundled (and `Default`-able) so call sites name each field — in particular
/// the two distinct compaction flags can't be silently transposed.
#[derive(Default)]
struct WelcomeLayoutInput {
    content_area: Rect,
    /// Error/warning row height; 0 when there's nothing to show.
    error_height: u16,
    menu_height: u16,
    tip_height: u16,
    /// Vertical compaction (session picker visible): skip the logo.
    compact: bool,
    /// Horizontal-inset compaction (appearance setting) for the stacked slot.
    prompt_compact: bool,
}

impl WelcomeLayout {
    /// Whether the hero box (side-by-side logo + menu inside a border) is active.
    pub(super) fn has_hero_box(&self) -> bool {
        self.hero_box.width > 0 && self.hero_box.height > 0
    }

    pub(super) fn fixed_below(tip_height: u16) -> u16 {
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        tip_height + tip_gap + PROMPT_HEIGHT + VERSION_GAP + 1
    }

    /// Compute the welcome screen layout, allowing the wide hero-box variant.
    fn compute(input: WelcomeLayoutInput) -> Self {
        Self::compute_inner(input, true)
    }

    /// Compute the welcome screen layout, forced to the stacked variant.
    ///
    /// The blocked screens (login, ZDR gate) render through
    /// `render_welcome_blocked`, which only paints the stacked `logo`/`menu`
    /// rects. The hero-box layout zeroes those, so the blocked path must stay
    /// stacked regardless of terminal size.
    fn compute_stacked(input: WelcomeLayoutInput) -> Self {
        Self::compute_inner(input, false)
    }

    /// Compute the welcome screen layout.
    ///
    /// Picks hero vs stacked. `allow_hero_box` gates the wide variant;
    /// stacked-only callers pass `false`.
    fn compute_inner(input: WelcomeLayoutInput, allow_hero_box: bool) -> Self {
        let WelcomeLayoutInput {
            content_area,
            error_height,
            menu_height,
            tip_height,
            compact,
            prompt_compact,
        } = input;
        let _ = prompt_compact;
        let zero = Rect::default();
        let use_hero_box = allow_hero_box
            && !compact
            && content_area.width >= HERO_BOX_MIN_WIDTH
            && menu_height > 0
            && content_area.height
                >= hero_box::min_content_height(error_height, menu_height, tip_height);

        if use_hero_box {
            return hero_box::compute_hero_box(content_area, error_height, menu_height, tip_height);
        }

        // Stacked layout: skip the logo in compact mode (the session picker
        // needs the space); otherwise pick small/full/none by height.
        let logo_rows = if compact {
            0
        } else {
            logo_line_count(content_area.height)
        };

        let gap_after_logo = if error_height > 0 { 1 } else { 0 };
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        let fixed_below = Self::fixed_below(tip_height);
        let fixed_above = logo_rows + 1 + gap_after_logo + error_height; // +1 for gap after logo
        // Compute top_pad using the *default* menu height (4 items = 7 rows) so
        // the logo position stays constant regardless of picker/focus state.
        let top_pad = if compact {
            0
        } else {
            let default_menu_height = 4u16;
            let remaining = content_area.height.saturating_sub(fixed_above);
            remaining
                .saturating_sub(default_menu_height)
                .saturating_sub(fixed_below)
                / 3
        };
        let logo_gap = 1u16;
        let flex_gap = 1u16;
        // The menu is the only unbounded row group. With more items than the
        // content area holds, the constraint solver squeezes every `Length`
        // above it — clipping the moon — and silently truncates the menu.
        // Cap the menu to the rows genuinely left over; `render_menu`
        // scrolls its items inside the capped viewport.
        let menu_height = menu_height.min(
            content_area
                .height
                .saturating_sub(top_pad + fixed_above + fixed_below + flex_gap)
                .max(1),
        );
        let [_, logo, _, _, error, menu, _, tip, _, prompt, _, version] = Layout::vertical([
            Constraint::Length(top_pad),
            Constraint::Length(logo_rows),
            Constraint::Length(logo_gap), // gap after logo
            Constraint::Length(gap_after_logo),
            Constraint::Length(error_height),
            Constraint::Length(menu_height),
            Constraint::Min(flex_gap),
            Constraint::Length(tip_height),
            Constraint::Length(tip_gap),
            Constraint::Length(PROMPT_HEIGHT),
            Constraint::Length(VERSION_GAP),
            Constraint::Length(1), // version
        ])
        .areas(content_area);
        Self {
            logo,
            error,
            menu,
            tip,
            prompt,
            version,
            hero_box: zero,
            hero_logo: zero,
            hero_version: zero,
            hero_subtitle: zero,
            hero_menu: zero,
        }
    }
}

/// Controls what the version badge renders.
pub(super) enum VersionBadgeMode {
    /// Full badge: team | tier | api_key | **Kigi** VERSION+channel (right-aligned).
    Full,
    /// Hero footer: team | api_key | Kigi [channel] (right-aligned, gray).
    HeroFooter,
    /// Hero inline: **Kigi**  VERSION (left-aligned).
    HeroInline,
}

pub(super) fn render_version_badge(
    version_rect: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    team_name: Option<&str>,
    h_margin: u16,
    is_api_key_auth: bool,
    mode: VersionBadgeMode,
) {
    let version_area = Rect {
        width: version_rect.width.saturating_sub(h_margin),
        ..version_rect
    };
    let sep = Span::styled(
        "  \u{2502}  ",
        Style::default().fg(theme.gray).add_modifier(Modifier::DIM),
    );
    let mut spans = Vec::new();

    let (show_team, show_api_key, align) = match &mode {
        VersionBadgeMode::Full => (true, true, Alignment::Right),
        VersionBadgeMode::HeroFooter => (true, true, Alignment::Right),
        VersionBadgeMode::HeroInline => (false, false, Alignment::Left),
    };

    if show_team && let Some(team) = team_name {
        spans.push(Span::styled(team, Style::default().fg(theme.gray)));
        spans.push(sep.clone());
    }
    if show_api_key && is_api_key_auth {
        spans.push(Span::styled(
            "Logged in with API key",
            Style::default().fg(theme.gray),
        ));
        spans.push(sep);
    }

    let channel = kigi_update::channel_label();
    match &mode {
        VersionBadgeMode::Full => {
            spans.push(Span::styled(
                "Kigi  ",
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("{}{}", kigi_version::VERSION, channel),
                Style::default().fg(theme.gray),
            ));
        }
        VersionBadgeMode::HeroFooter => {
            let channel_display = if channel.is_empty() {
                "Kigi"
            } else {
                channel.trim()
            };
            spans.push(Span::styled(
                channel_display,
                Style::default().fg(theme.gray),
            ));
        }
        VersionBadgeMode::HeroInline => {
            spans.push(Span::styled(
                "Kigi  ",
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                kigi_version::VERSION,
                Style::default().fg(theme.gray),
            ));
        }
    }

    let version_line = Line::from(spans).alignment(align);
    Paragraph::new(version_line).render(version_area, buf);
}

/// Render the prompt box and version line (shared across welcome states).
///
/// When `skip_version` is true the version badge is not rendered (it was
/// already drawn inside the hero box).
#[allow(clippy::too_many_arguments)]
fn render_prompt_and_version(
    layout: &WelcomeLayout,
    content_width: u16,
    buf: &mut Buffer,
    theme: &Theme,
    focus: WelcomePromptFocus,
    prompt: &mut PromptWidget,
    info: &PromptInfo<'_>,
    tip: Option<&str>,
    team_name: Option<&str>,
    h_margin: u16,
    compact: bool,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    is_api_key_auth: bool,
    skip_version: bool,
) -> (
    Option<(u16, u16)>,
    Option<crate::terminal::overlay::PostFlush>,
) {
    let [_, prompt_centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(content_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(layout.prompt);

    if let Some(tip_text) = tip
        && layout.tip.height > 0
    {
        let [_, tip_centered, _] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(content_width),
            Constraint::Min(0),
        ])
        .flex(Flex::Center)
        .areas(layout.tip);
        let inset = prompt::prompt_inset(compact);
        let tip_inset = Rect {
            x: tip_centered.x + inset,
            y: tip_centered.y,
            width: tip_centered.width.saturating_sub(inset * 2),
            height: tip_centered.height,
        };
        crate::tips::render::render_tip(tip_inset, buf, tip_text);
    }
    let prompt_result =
        prompt::render_prompt(prompt_centered, buf, focus, prompt, info, 2, 2, compact);

    if let Some(pending) = &pending_hint {
        let key_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);
        let action_style = Style::default().fg(theme.gray);
        let key_text = pending.shortcut.display();
        let label = format!("press again to {}", pending.label);
        let line = Line::from(vec![
            Span::styled(format!("  {key_text}"), key_style),
            Span::styled(":", action_style),
            Span::styled(label, action_style),
        ]);
        buf.set_line(
            layout.version.x,
            layout.version.y,
            &line,
            layout.version.width,
        );
    } else if !skip_version {
        render_version_badge(
            layout.version,
            buf,
            theme,
            team_name,
            h_margin,
            is_api_key_auth,
            VersionBadgeMode::Full,
        );
    } else {
        render_version_badge(
            layout.version,
            buf,
            theme,
            team_name,
            h_margin,
            is_api_key_auth,
            VersionBadgeMode::HeroFooter,
        );
    }

    prompt_result
}

/// All display state for rendering the welcome screen.
pub struct WelcomeRenderParams<'a> {
    pub prompt_focus: WelcomePromptFocus,
    pub auth_state: &'a AuthState,
    /// Folder-trust state. When `Pending` (auth done, access granted), the
    /// welcome screen renders the trust question instead of the normal prompt.
    pub trust_state: &'a TrustState,
    /// Shell-advertised auth methods — the login picker lists the interactive
    /// ones (see [`pending_menu_items`]).
    pub auth_methods: &'a [acp::AuthMethod],
    pub login_label: Option<&'a str>,
    pub auth_code_input: &'a str,
    pub clipboard_copied: bool,
    pub show_raw_url: bool,
    pub tip: Option<&'a str>,
    pub model_name: &'a str,
    pub flags: &'a [PromptFlag<'a>],
    pub selected: Option<usize>,
    /// Login-picker menu scroll offset from the previous frame (minimal-
    /// scroll viewport; the frame's actual offset comes back in
    /// [`WelcomeRenderResult::menu_scroll`]).
    pub menu_scroll: usize,
    pub has_claude_import: bool,
    pub mouse_pos: Option<(u16, u16)>,
    pub session_picker: Option<&'a [SessionPickerEntry]>,
    pub session_picker_loading: bool,
    pub compact: bool,
    pub pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    pub startup_warnings: &'a [StartupWarning],
    pub pending_update_version: Option<&'a str>,
    /// Recent foreign session offered on ctrl+u, suppressed by a pending update.
    pub foreign_resume_hint: Option<&'a kigi_workspace::foreign_sessions::RecentForeignSession>,
    pub is_api_key_auth: bool,
    pub session_picker_content_results:
        Option<&'a [kigi_shell::extensions::session_search::SearchSessionHit]>,
    pub session_picker_content_loading: bool,
    /// The query the picker entries were server-fetched with (see
    /// [`crate::views::session_picker::effective_filter_query`]).
    pub session_picker_entries_query: Option<&'a str>,
    pub welcome_tick: u64,
    pub session_picker_grouped: bool,
    /// Source filter (local/remote/all) for the session picker.
    pub session_picker_source_filter: crate::views::session_picker::SourceFilter,
    /// Process-wide `--chat`: the picker lists backend conversations only, so
    /// the Local/Remote source filter and local deep search are hidden.
    pub chat_mode: bool,
    /// Live working directory (tracks `Effect::SetWorkingDir`), used to pin
    /// the current repo's session group to the top of the picker.
    pub cwd: &'a std::path::Path,
}

/// Render the welcome screen.
pub fn render_welcome(
    area: Rect,
    buf: &mut Buffer,
    params: &WelcomeRenderParams<'_>,
    prompt: &mut PromptWidget,
    session_picker_state: &mut crate::views::picker::PickerState,
) -> WelcomeRenderResult {
    let theme = Theme::current();
    let h_margin = if params.compact {
        H_MARGIN_COMPACT
    } else {
        H_MARGIN
    };
    let v_margin = 1u16;

    buf.set_style(area, Style::default().bg(theme.bg_base));

    // Top bar is always 1 row.
    let [_, top_bar_area, content_area, _] = Layout::vertical([
        Constraint::Length(v_margin),
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(v_margin),
    ])
    .areas(area);

    let top_bar_inner = Rect {
        x: top_bar_area.x + h_margin,
        y: top_bar_area.y,
        width: top_bar_area.width.saturating_sub(h_margin * 2),
        height: 1,
    };
    render_top_bar(top_bar_inner, buf, &theme);

    let mut result = match params.auth_state {
        AuthState::Pending { error } => {
            // Login picker: one row per interactive method + Quit.
            let items: Vec<PendingMenuItem> =
                pending_menu_items(params.auth_methods, params.login_label);
            let menu: Vec<(&str, &str)> = items
                .iter()
                .enumerate()
                .map(|(i, item)| (item.shortcut(i), item.label()))
                .collect();
            let msg = error.as_deref().map(|e| (e, theme.accent_error));
            let info = PromptInfo {
                model_name: params.model_name,
                flags: params.flags,
                multiline: false,
            };
            let (menu_rects, menu_scroll, post_flush_escapes) = render_welcome_blocked(
                content_area,
                buf,
                msg,
                &menu,
                params.selected,
                Some((prompt, &info)),
                h_margin,
                params.compact,
                params.menu_scroll,
            );
            WelcomeRenderResult {
                cursor_pos: None,
                post_flush_escapes,
                menu_rects,
                prompt_rect: None,
                session_picker_hit_areas: None,
                import_banner_rect: None,
                auth_url_rect: None,
                auth_fallback_rect: None,
                menu_scroll,
            }
        }
        AuthState::Authenticating { auth_url, mode, .. } => {
            let llc = logo_line_count(content_area.height);
            let (url_rect, fallback_rect) = render_welcome_authenticating(
                content_area,
                buf,
                &theme,
                llc,
                auth_url.as_deref(),
                *mode,
                params.auth_code_input,
                params.clipboard_copied,
                params.show_raw_url,
            );
            WelcomeRenderResult {
                cursor_pos: None,
                post_flush_escapes: None,
                menu_rects: vec![],
                prompt_rect: None,
                session_picker_hit_areas: None,
                import_banner_rect: None,
                auth_url_rect: url_rect,
                auth_fallback_rect: fallback_rect,
                menu_scroll: 0,
            }
        }
        // Folder-trust question: shown after auth, before any session is
        // created, when the cwd has untrusted repo-local config. Mirrors the
        // Pending login screen. The `if let` destructure makes the
        // `Pending`-only render structurally exhaustive (no `unreachable!`).
        AuthState::Done => {
            if let TrustState::Pending { workspace } = params.trust_state {
                render_welcome_trust(
                    content_area,
                    buf,
                    &theme,
                    workspace,
                    params.selected,
                    h_margin,
                    params.compact,
                )
            } else {
                render_welcome_done(
                    content_area,
                    buf,
                    &theme,
                    params,
                    prompt,
                    session_picker_state,
                    h_margin,
                )
            }
        }
    };
    if result.post_flush_escapes.is_none() {
        result.post_flush_escapes = crate::terminal::overlay::clear().map(Into::into);
    }
    result
}

/// Render a blocked welcome screen: logo + optional message + menu + version.
///
/// Used for both the login screen (Pending) and the ZDR gate. The layout is:
///   Logo
///   {message}
///   Menu items
///   {prompt}      (optional)
///   Version badge
#[allow(clippy::too_many_arguments)]
fn render_welcome_blocked(
    content_area: Rect,
    buf: &mut Buffer,
    message: Option<(&str, ratatui::style::Color)>,
    menu_items: &[(&str, &str)],
    selected: Option<usize>,
    prompt: Option<(&mut PromptWidget, &PromptInfo<'_>)>,
    h_margin: u16,
    compact: bool,
    menu_scroll: usize,
) -> (
    Vec<Rect>,
    usize,
    Option<crate::terminal::overlay::PostFlush>,
) {
    let theme = Theme::current();

    let msg_height = if message.is_some() { 2u16 } else { 0u16 };
    let menu_height = menu_items.len() as u16;
    // Force the stacked layout: this renderer only paints the stacked
    // logo/menu rects, which the hero-box layout would leave empty.
    let layout = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
        content_area,
        error_height: msg_height,
        menu_height,
        compact,
        prompt_compact: compact,
        ..Default::default()
    });

    render_logo(layout.logo, buf, &theme, content_area.height);

    if let Some((text, color)) = message {
        let line =
            Line::from(Span::styled(text, Style::default().fg(color))).alignment(Alignment::Center);
        Paragraph::new(line).render(layout.error, buf);
    }

    // Inset the menu the same as the input bar / post-auth menu so the actions
    // keep side spacing instead of touching the window edge on narrow terminals.
    let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(compact));
    let (menu_rects, menu_scroll) = render_menu(
        menu_area,
        buf,
        &theme,
        menu_items,
        selected,
        None,
        0,
        menu_scroll,
    );

    let post_flush_escapes = if let Some((prompt_widget, info)) = prompt {
        let [_, prompt_centered, _] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(content_area.width),
            Constraint::Min(0),
        ])
        .flex(Flex::Center)
        .areas(layout.prompt);
        prompt::render_prompt(
            prompt_centered,
            buf,
            WelcomePromptFocus::Unfocused,
            prompt_widget,
            info,
            2,
            2,
            compact,
        )
        .1
    } else {
        None
    };

    render_version_badge(
        layout.version,
        buf,
        &theme,
        None,
        h_margin,
        false,
        VersionBadgeMode::Full,
    );
    (menu_rects, menu_scroll, post_flush_escapes)
}

/// Render the folder-trust question. Mirrors [`render_welcome_blocked`]'s
/// stacked layout (logo + message + menu + version badge), but the message is a
/// multi-line block showing the workspace path and the warning that Kigi
/// may run or modify contents in this directory (a security risk). The y/N
/// answer is handled by the welcome input interceptor, so this only paints;
/// `menu_rects` are returned for parity with the other welcome arms.
fn render_welcome_trust(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    workspace: &std::path::Path,
    selected: Option<usize>,
    h_margin: u16,
    compact: bool,
) -> WelcomeRenderResult {
    let menu_items = [("y", "Yes, proceed"), ("n", "No, quit")];
    let lines = vec![
        Line::from(Span::styled(
            "Do you trust the contents of this directory?",
            Style::default().fg(theme.gray_bright),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            workspace.display().to_string(),
            Style::default().fg(theme.accent_user),
        ))
        .alignment(Alignment::Center),
        Line::default(),
        // Two lines so the warning never clips at narrow / compact widths
        // (a single ~78-char line would truncate "...posing security risks").
        Line::from(Span::styled(
            "Kigi may run or modify contents in this directory,",
            Style::default().fg(theme.gray),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            "posing security risks.",
            Style::default().fg(theme.gray),
        ))
        .alignment(Alignment::Center),
        // Spacer between the warning and the y/n menu.
        Line::default(),
    ];

    let msg_height = lines.len() as u16;
    let menu_height = menu_items.len() as u16;
    let layout = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
        content_area,
        error_height: msg_height,
        menu_height,
        compact,
        prompt_compact: compact,
        ..Default::default()
    });

    render_logo(layout.logo, buf, theme, content_area.height);
    Paragraph::new(lines).render(layout.error, buf);

    let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(compact));
    let (menu_rects, _) = render_menu(menu_area, buf, theme, &menu_items, selected, None, 0, 0);

    render_version_badge(
        layout.version,
        buf,
        theme,
        None,
        h_margin,
        false,
        VersionBadgeMode::Full,
    );

    // Only `menu_rects` are meaningful here; the rest are absent (no prompt,
    // picker, auth/gate links) -- `Default` keeps this honest without a 13-field
    // all-`None` literal.
    WelcomeRenderResult {
        menu_rects,
        ..Default::default()
    }
}

/// Header text shared by Loopback and Command auth modes.
const AUTH_HEADER: &str = "A browser window will open for authentication.";
/// Header text for the device-flow auth mode.
const DEVICE_AUTH_HEADER: &str = "Approve in your browser to finish signing in.";
/// Caption beneath the device code.
const DEVICE_CODE_CAPTION: &str = "Make sure your browser shows this code.";

/// Extract `user_code` from a device verification URL (`None` if absent or
/// malformed). Shown on-screen so the user can confirm it matches the browser
/// before approving (anti-phishing).
fn extract_user_code(url: &str) -> Option<&str> {
    let code = url
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|kv| kv.strip_prefix("user_code="))?;
    let valid = !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then_some(code)
}
/// Clickable copy prompt shared by Loopback and Command auth modes.
const AUTH_COPY_PREFIX: &str = "If it doesn't open, click ";
const AUTH_COPY_HERE: &str = "here";
const AUTH_COPY_SUFFIX: &str = " to copy.";

/// Build the "click here to copy" line with "here" underlined in accent color.
fn auth_copy_line(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(AUTH_COPY_PREFIX, Style::default().fg(theme.gray_bright)),
        Span::styled(
            AUTH_COPY_HERE,
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::UNDERLINED),
        ),
        Span::styled(AUTH_COPY_SUFFIX, Style::default().fg(theme.gray_bright)),
    ])
    .alignment(Alignment::Center)
}

/// Number of physical rows the header + blank occupy before the copy line.
fn auth_copy_preceding_rows(header: &str, inner_width: u16) -> u16 {
    let header_rows = (header.len() as u16).div_ceil(inner_width);
    header_rows + 1 // header + blank
}

/// Number of physical rows the copy line occupies when wrapped.
fn auth_copy_line_rows(inner_width: u16) -> u16 {
    let copy_len = AUTH_COPY_PREFIX.len() + AUTH_COPY_HERE.len() + AUTH_COPY_SUFFIX.len();
    (copy_len as u16).div_ceil(inner_width)
}

const AUTH_FALLBACK_TEXT: &str = "Copying not working? Click here to show full URL.";

/// Build the fallback "show full URL" link line.
fn auth_fallback_line(theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        AUTH_FALLBACK_TEXT,
        Style::default()
            .fg(theme.gray)
            .add_modifier(Modifier::UNDERLINED),
    ))
    .alignment(Alignment::Center)
}

/// Push the shared copy-prompt block: the "click here to copy" line, a "copied!"
/// slot (kept blank when not copied so the height is stable), and the
/// show-full-URL fallback link.
fn push_auth_copy_block(lines: &mut Vec<Line<'static>>, theme: &Theme, clipboard_copied: bool) {
    lines.push(Line::default());
    lines.push(auth_copy_line(theme));
    lines.push(Line::default());
    lines.push(if clipboard_copied {
        Line::from(Span::styled("copied!", Style::default().fg(theme.gray)))
            .alignment(Alignment::Center)
    } else {
        Line::default()
    });
    lines.push(Line::default());
    lines.push(auth_fallback_line(theme));
}

/// Rows occupied by [`push_auth_copy_block`].
fn auth_copy_block_rows(inner_width: u16) -> u16 {
    auth_copy_line_rows(inner_width) + 5
}

/// Click hit-rects for the copy line and fallback link. `header`'s wrapped row
/// count sets the copy line's vertical offset.
fn auth_hit_rects(
    msg_area: Rect,
    h_pad: u16,
    inner_width: u16,
    header: &str,
    preceding_extra: u16,
) -> (Option<Rect>, Option<Rect>) {
    let preceding = auth_copy_preceding_rows(header, inner_width) + preceding_extra;
    let copy_rows = auth_copy_line_rows(inner_width);
    let copy_rect = Rect {
        x: msg_area.x + h_pad,
        y: msg_area.y + preceding,
        width: inner_width,
        height: copy_rows,
    };
    // fallback line is after: copy_rows + blank + copied_slot + blank
    let fallback_y = msg_area.y + preceding + copy_rows + 3;
    let fb_rect = Rect {
        x: msg_area.x + h_pad,
        y: fallback_y,
        width: inner_width,
        height: 1,
    };
    (Some(copy_rect), Some(fb_rect))
}

/// Render the "raw URL" mode: shows the full URL with mouse capture disabled
/// so the user can select and copy it natively.
fn render_raw_url_mode(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    top_pad: u16,
    logo_line_count: u16,
    auth_url: Option<&str>,
) -> (Option<Rect>, Option<Rect>) {
    // Use full terminal width for the URL so the terminal wraps it
    // naturally without inserting spaces (important for copy-paste).
    let full_width = content_area.width.max(1);
    let url_lines = auth_url
        .map(|u| (u.len() as u16).div_ceil(full_width))
        .unwrap_or(0);
    let msg_height = 1 + 1 + url_lines; // hint + blank + URL
    let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(logo_line_count),
        Constraint::Length(2),
        Constraint::Length(msg_height),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(content_area);

    render_logo(logo_area, buf, theme, content_area.height);

    // Render hint above the URL.
    let hint = Line::from(Span::styled(
        "Select the URL below with your mouse and copy manually.",
        Style::default().fg(theme.gray),
    ))
    .alignment(Alignment::Center);
    Paragraph::new(hint).render(
        Rect {
            height: 1,
            ..msg_area
        },
        buf,
    );

    // Write the URL directly to the buffer character-by-character so the
    // terminal wraps naturally at the screen edge. Ratatui's Paragraph
    // wrap inserts spaces at break points which corrupts the URL on copy.
    //
    // When the URL fits on a single line, center it to match the rest of the
    // screen. When it's longer, keep it flush-left at the full terminal width
    // so the natural wrap preserves copy-paste (centering a wrapped URL would
    // inject leading spaces into the selection).
    if let Some(url) = auth_url {
        let url_style = Style::default().fg(theme.accent_user);
        let url_y = msg_area.y + 2; // after hint + blank
        // Control characters are skipped below to prevent terminal escape
        // injection, so measure the URL without them.
        let url_len = url.chars().filter(|c| !c.is_control()).count() as u16;
        let x_offset = if url_len <= full_width {
            (full_width - url_len) / 2
        } else {
            0
        };
        let buf_area = buf.area();
        let buf_max_col = buf_area.x + buf_area.width;
        let buf_max_row = buf_area.y + buf_area.height;
        for (i, ch) in url.chars().filter(|c| !c.is_control()).enumerate() {
            let col = msg_area.x + x_offset + (i as u16) % full_width;
            let row = url_y + (i as u16) / full_width;
            if row >= msg_area.y + msg_area.height {
                break;
            }
            // Guard against OOB access during resize races.
            if col >= buf_max_col || row >= buf_max_row {
                continue;
            }
            buf[(col, row)].set_char(ch).set_style(url_style);
        }
    }

    let hint_spans = vec![
        Span::styled(
            "ctrl+q",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  go back", Style::default().fg(theme.gray)),
    ];
    let hints = Line::from(hint_spans).alignment(Alignment::Center);
    Paragraph::new(hints).render(hint_area, buf);

    (None, None) // no click rects — mouse capture is disabled
}

/// Which "browser opened, now waiting" arm to render; owns the header,
/// waiting caption, and (for `Device`) the device-code derivation.
#[derive(Clone, Copy)]
enum BrowserStatusKind {
    /// External auth provider opened its own browser.
    Command,
    /// RFC 8628 device flow — also shows the device code.
    Device,
}

/// Render a "browser opened, now waiting" auth arm (Command + Device).
///
/// Shared status layout: logo, then a centered block of header, optional device
/// code + caption, optional copy/fallback links (when there's a URL), and the
/// waiting caption; finally quit hints.
#[allow(clippy::too_many_arguments)]
fn render_browser_status_arm(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    top_pad: u16,
    logo_line_count: u16,
    auth_url: Option<&str>,
    show_raw_url: bool,
    clipboard_copied: bool,
    kind: BrowserStatusKind,
) -> (Option<Rect>, Option<Rect>) {
    let h_pad: u16 = content_area.width / 6;
    let inner_width = content_area.width.saturating_sub(h_pad * 2).max(1);

    if show_raw_url {
        return render_raw_url_mode(content_area, buf, theme, top_pad, logo_line_count, auth_url);
    }

    // Device also parses the user code from the verification URL.
    let (header, waiting_text, user_code) = match kind {
        BrowserStatusKind::Command => (AUTH_HEADER, "Waiting for login to complete...", None),
        BrowserStatusKind::Device => (
            DEVICE_AUTH_HEADER,
            "Waiting for approval...",
            auth_url.and_then(extract_user_code),
        ),
    };

    let header_rows = (header.len() as u16).div_ceil(inner_width);
    let code_extra = if user_code.is_some() {
        let caption_rows = (DEVICE_CODE_CAPTION.len() as u16).div_ceil(inner_width);
        1 + 1 + 1 + caption_rows // blank + code + blank + caption
    } else {
        0
    };
    let copy_extra = if auth_url.is_some() {
        auth_copy_block_rows(inner_width)
    } else {
        0
    };
    let msg_height = header_rows + code_extra + copy_extra + 1 + 1; // blank + waiting

    let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(logo_line_count),
        Constraint::Length(2),          // gap
        Constraint::Length(msg_height), // status message
        Constraint::Min(1),             // gap
        Constraint::Length(1),          // hints
        Constraint::Min(0),
    ])
    .areas(content_area);

    render_logo(logo_area, buf, theme, content_area.height);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(header, Style::default().fg(theme.gray_bright)))
            .alignment(Alignment::Center),
    ];
    if let Some(code) = user_code {
        lines.push(Line::default());
        lines.push(
            Line::from(Span::styled(
                code.to_owned(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
        );
        lines.push(Line::default());
        lines.push(
            Line::from(Span::styled(
                DEVICE_CODE_CAPTION,
                Style::default().fg(theme.gray),
            ))
            .alignment(Alignment::Center),
        );
    }
    if auth_url.is_some() {
        push_auth_copy_block(&mut lines, theme, clipboard_copied);
    }
    lines.push(Line::default());
    lines.push(
        Line::from(Span::styled(waiting_text, Style::default().fg(theme.gray)))
            .alignment(Alignment::Center),
    );
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().padding(Padding::horizontal(h_pad)))
        .render(msg_area, buf);

    let (click_rect, fallback_rect) = if auth_url.is_some() {
        auth_hit_rects(msg_area, h_pad, inner_width, header, code_extra)
    } else {
        (None, None)
    };

    let hints = Line::from(quit_hint_spans(theme)).alignment(Alignment::Center);
    Paragraph::new(hints).render(hint_area, buf);

    (click_rect, fallback_rect)
}

/// Render the welcome screen during authentication (Authenticating state).
#[allow(clippy::too_many_arguments)]
fn render_welcome_authenticating(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    logo_line_count: u16,
    auth_url: Option<&str>,
    mode: AuthMode,
    auth_code_input: &str,
    clipboard_copied: bool,
    show_raw_url: bool,
) -> (Option<Rect>, Option<Rect>) {
    let top_pad = content_area.height.saturating_sub(logo_line_count) / 10;

    match mode {
        AuthMode::Loopback => {
            // Manual token paste: show copy prompt + input box
            let h_pad: u16 = content_area.width / 6;
            let inner_width = content_area.width.saturating_sub(h_pad * 2).max(1);

            if show_raw_url {
                return render_raw_url_mode(
                    content_area,
                    buf,
                    theme,
                    top_pad,
                    logo_line_count,
                    auth_url,
                );
            }

            let msg_height = if auth_url.is_some() {
                let header_rows = (AUTH_HEADER.len() as u16).div_ceil(inner_width);
                header_rows + auth_copy_block_rows(inner_width)
            } else {
                1u16
            };
            let [_, logo_area, _, msg_area, _, prompt_area, _, hint_area, _] = Layout::vertical([
                Constraint::Length(top_pad),
                Constraint::Length(logo_line_count),
                Constraint::Length(1),          // gap
                Constraint::Length(msg_height), // instruction + copy prompt
                Constraint::Min(1),             // gap
                Constraint::Length(5),          // prompt box
                Constraint::Length(1),          // gap
                Constraint::Length(1),          // hints
                Constraint::Min(0),
            ])
            .areas(content_area);

            render_logo(logo_area, buf, theme, content_area.height);

            // Instruction text
            let mut lines: Vec<Line> = Vec::new();
            if auth_url.is_some() {
                lines.push(
                    Line::from(Span::styled(
                        AUTH_HEADER,
                        Style::default().fg(theme.gray_bright),
                    ))
                    .alignment(Alignment::Center),
                );
                push_auth_copy_block(&mut lines, theme, clipboard_copied);
            } else {
                lines.push(
                    Line::from(Span::styled(
                        "Waiting for auth URL...",
                        Style::default().fg(theme.gray),
                    ))
                    .alignment(Alignment::Center),
                );
            }
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::horizontal(h_pad)))
                .render(msg_area, buf);

            let (click_rect, fallback_rect) = if auth_url.is_some() {
                auth_hit_rects(msg_area, h_pad, inner_width, AUTH_HEADER, 0)
            } else {
                (None, None)
            };

            // Prompt box with token input
            let prompt_width = content_area.width;
            let [_, prompt_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(prompt_width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(prompt_area);
            render_auth_input_box(
                prompt_centered,
                buf,
                theme,
                auth_code_input,
                "Paste your token here...",
            );

            // Hints
            let mut hint_spans = vec![
                Span::styled(
                    "enter",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  submit    ", Style::default().fg(theme.gray)),
            ];
            hint_spans.extend(quit_hint_spans(theme));
            let hints = Line::from(hint_spans).alignment(Alignment::Center);
            Paragraph::new(hints).render(hint_area, buf);

            (click_rect, fallback_rect)
        }

        AuthMode::ApiKeyEntry(target) => {
            // Platform API-key paste box: instruction + input + hints. No
            // auth-URL machinery — the key comes from the platform console.
            let h_pad: u16 = content_area.width / 6;
            let inner_width = content_area.width.saturating_sub(h_pad * 2).max(1);
            let instruction = format!(
                "Paste your {} API key (from {})",
                target.vendor(),
                target.console_host()
            );
            let msg_height = (instruction.len() as u16).div_ceil(inner_width);
            let [_, logo_area, _, msg_area, _, prompt_area, _, hint_area, _] = Layout::vertical([
                Constraint::Length(top_pad),
                Constraint::Length(logo_line_count),
                Constraint::Length(1),          // gap
                Constraint::Length(msg_height), // instruction
                Constraint::Min(1),             // gap
                Constraint::Length(5),          // prompt box
                Constraint::Length(1),          // gap
                Constraint::Length(1),          // hints
                Constraint::Min(0),
            ])
            .areas(content_area);

            render_logo(logo_area, buf, theme, content_area.height);

            let msg = Line::from(Span::styled(
                instruction,
                Style::default().fg(theme.gray_bright),
            ))
            .alignment(Alignment::Center);
            Paragraph::new(msg)
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::horizontal(h_pad)))
                .render(msg_area, buf);

            let [_, prompt_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(prompt_area);
            render_auth_input_box(
                prompt_centered,
                buf,
                theme,
                auth_code_input,
                "Paste your API key here...",
            );

            let hints = Line::from(vec![
                Span::styled(
                    "enter",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  submit    ", Style::default().fg(theme.gray)),
                Span::styled(
                    "esc",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  back", Style::default().fg(theme.gray)),
            ])
            .alignment(Alignment::Center);
            Paragraph::new(hints).render(hint_area, buf);

            (None, None)
        }

        AuthMode::Command => render_browser_status_arm(
            content_area,
            buf,
            theme,
            top_pad,
            logo_line_count,
            auth_url,
            show_raw_url,
            clipboard_copied,
            BrowserStatusKind::Command,
        ),

        AuthMode::Device => render_browser_status_arm(
            content_area,
            buf,
            theme,
            top_pad,
            logo_line_count,
            auth_url,
            show_raw_url,
            clipboard_copied,
            BrowserStatusKind::Device,
        ),

        AuthMode::Pending => {
            // Connecting: status text
            let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
                Constraint::Length(top_pad),
                Constraint::Length(logo_line_count),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(content_area);

            render_logo(logo_area, buf, theme, content_area.height);

            let msg = Line::from(Span::styled(
                "Connecting...",
                Style::default().fg(theme.gray_bright),
            ))
            .alignment(Alignment::Center);
            Paragraph::new(msg).render(msg_area, buf);

            let hints = Line::from(quit_hint_spans(theme)).alignment(Alignment::Center);
            Paragraph::new(hints).render(hint_area, buf);

            (None, None)
        }
    }
}

/// Shrink a rect by `inset` columns on the left and right (clamped at 0).
fn inset_horizontal(rect: Rect, inset: u16) -> Rect {
    Rect {
        x: rect.x + inset,
        width: rect.width.saturating_sub(inset * 2),
        ..rect
    }
}

/// Render the normal welcome screen (Done state -- already authenticated).
fn render_welcome_done(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    p: &WelcomeRenderParams<'_>,
    prompt: &mut PromptWidget,
    session_picker_state: &mut crate::views::picker::PickerState,
    h_margin: u16,
) -> WelcomeRenderResult {
    let show_picker = p.session_picker.is_some() || p.session_picker_loading;
    // Only use compact layout when the session picker is visible — it needs
    // the logo/centering space for its list. Plain compact mode keeps the
    // normal welcome layout.
    let welcome_compact = show_picker;

    let in_vscode_family = welcome_in_vscode_family();

    // Startup-warning hint height (multi-line aware).
    let hint_height = p.startup_warnings.first().map_or(0u16, |w| {
        let msg_lines = w.message.lines().count() as u16;
        let action_line = if w.action.is_some() { 1 } else { 0 };
        msg_lines + action_line + 1 // +1 for buffer spacing
    });
    let has_update_tip = p.pending_update_version.is_some();
    let has_resume_tip = !has_update_tip && p.foreign_resume_hint.is_some();
    let tip_height = if !show_picker {
        if has_update_tip || has_resume_tip {
            1u16 // update/resume tips are short, always 1 row
        } else if let Some(tip_text) = p.tip {
            let inset = prompt::prompt_inset(welcome_compact);
            let tip_width = content_area.width.saturating_sub(inset * 2);
            crate::tips::render::tip_height(tip_width, tip_text)
        } else {
            0
        }
    } else {
        0
    };

    let owned_menu;
    let menu_items: &[(&str, &str)] = {
        let (key_w, key_s, key_q, key_i_with_x) = (
            "ctrl+w",
            "ctrl+s",
            if in_vscode_family { "ctrl+d" } else { "ctrl+q" },
            "ctrl+i  [x]",
        );
        // Insert the import row at the top when there are pending `.claude/`
        // settings to import — it's the most actionable item right now.
        let mut items: Vec<(&str, &str)> = Vec::with_capacity(4);
        if p.has_claude_import {
            // The trailing "[x]" is a clickable dismiss affordance — the
            // welcome screen mouse handler treats clicks on the rightmost
            // 3 cells of this row as dismiss instead of open. Keyboard:
            // ctrl-shift-i. The key string is right-aligned by render_menu,
            // so [x] sits at the very end of the row.
            items.push((key_i_with_x, "Import Claude settings"));
        }
        items.push((key_w, "New worktree"));
        items.push((key_s, "Resume session"));
        items.push((key_q, "Quit"));
        owned_menu = items;
        owned_menu.as_slice()
    };

    let menu_height = if show_picker {
        0
    } else {
        menu_items.len() as u16
    };

    // Session picker height: 1 row per entry (no dividers), scrollable.
    let picker_count = p.session_picker.map_or(0, |s| s.len());
    let picker_height = if show_picker {
        if p.session_picker_loading {
            1
        } else {
            (picker_count as u16).min(15) + 3 // +3 for title + search + gap
        }
    } else {
        0
    };
    let content_height = menu_height + picker_height;
    let layout = WelcomeLayout::compute(WelcomeLayoutInput {
        content_area,
        error_height: hint_height,
        menu_height: content_height,
        tip_height,
        compact: welcome_compact,
        prompt_compact: p.compact,
    });

    // Render startup warning in the error area (same slot as auth errors).
    let import_banner_rect = render_startup_warnings(layout.error, buf, theme, p.startup_warnings);

    let (menu_rects, picker_close_button) = if show_picker {
        // Use the full area since logo/menu are hidden and shortcuts
        // are now rendered inside the picker content area.
        let picker_area = Rect {
            x: content_area.x,
            y: content_area.y,
            width: content_area.width,
            height: content_area.height,
        };
        let hit_areas = render_session_picker(
            picker_area,
            buf,
            theme,
            &mut SessionPickerRenderCtx {
                state: session_picker_state,
                sessions: p.session_picker,
                loading: p.session_picker_loading,
                pending_hint: p.pending_hint,
                shortcuts_area: None,
                content_results: p.session_picker_content_results,
                content_loading: p.session_picker_content_loading,
                entries_query: p.session_picker_entries_query,
                tick: p.welcome_tick,
                grouped: p.session_picker_grouped,
                source_filter: p.session_picker_source_filter,
                chat_mode: p.chat_mode,
                cwd: p.cwd,
            },
        );
        (vec![], Some(hit_areas))
    } else if layout.has_hero_box() {
        // Wide layout: render bordered hero box with logo left, version + menu right.
        let menu_rects =
            hero_box::render_hero_box(&layout, buf, theme, menu_items, p.selected, p.mouse_pos);
        (menu_rects, None)
    } else {
        // Narrow layout: stacked logo above, menu below. Inset the menu the
        // same as the input bar (`prompt_inset`) so it keeps side spacing
        // instead of touching the window edge on narrow terminals.
        render_logo(layout.logo, buf, theme, content_area.height);
        let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(p.compact));
        (
            render_menu(
                menu_area,
                buf,
                theme,
                menu_items,
                p.selected,
                p.mouse_pos,
                MENU_MIN_WIDTH,
                0,
            )
            .0,
            None,
        )
    };

    // Skip the prompt input when picker is visible to save space;
    // shortcuts are rendered inside the picker content area.
    let (cursor_pos, post_flush_escapes) = if show_picker {
        (None, None)
    } else {
        // When a background update is available, show the update
        // notification in the tip area instead of the random tip.

        // Render the update notification with accent styling when present.
        if let Some(ver) = p.pending_update_version
            && layout.tip.height > 0
        {
            let [_, tip_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(layout.tip);
            let inset = prompt::prompt_inset(p.compact);
            let tip_inset = Rect {
                x: tip_centered.x + inset,
                y: tip_centered.y,
                width: tip_centered.width.saturating_sub(inset * 2),
                height: tip_centered.height,
            };
            let key_name = "ctrl+u";
            let line = Line::from(vec![
                Span::styled(
                    "Update: ",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("v{ver} available \u{2014} press {key_name} to restart"),
                    Style::default().fg(theme.accent_user),
                ),
            ]);
            Paragraph::new(line)
                .style(Style::default().bg(theme.bg_base))
                .render(tip_inset, buf);
        }

        // Recent foreign session: offer a one-click resume in the tip area
        // (only when no update is pending — the update shares ctrl+u and wins).
        if p.pending_update_version.is_none()
            && let Some(hint) = p.foreign_resume_hint
            && layout.tip.height > 0
        {
            let [_, tip_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(layout.tip);
            let inset = prompt::prompt_inset(p.compact);
            let tip_inset = Rect {
                x: tip_centered.x + inset,
                y: tip_centered.y,
                width: tip_centered.width.saturating_sub(inset * 2),
                height: tip_centered.height,
            };
            let mins = hint.age.as_secs() / 60;
            let when = if mins == 0 {
                "moments ago".to_string()
            } else {
                format!("{mins}m ago")
            };
            let accent = Style::default().fg(theme.accent_user);
            let accent_bold = accent.add_modifier(Modifier::BOLD);
            let tool = crate::app::foreign_tool_display_label(hint.tool);
            let line = Line::from(vec![
                Span::styled("Coming from ", accent),
                Span::styled(tool, accent_bold),
                Span::styled(format!("? Resume your session from {when} using "), accent),
                Span::styled("ctrl+u", accent_bold),
            ]);
            Paragraph::new(line)
                .style(Style::default().bg(theme.bg_base))
                .render(tip_inset, buf);
        }

        let usage_info = PromptInfo {
            model_name: p.model_name,
            flags: p.flags,
            multiline: false,
        };

        render_prompt_and_version(
            &layout,
            content_area.width,
            buf,
            theme,
            p.prompt_focus,
            prompt,
            &usage_info,
            if p.pending_update_version.is_some() || p.foreign_resume_hint.is_some() {
                // Update/resume tip already rendered above with custom styling.
                None
            } else {
                p.tip
            },
            None,
            h_margin,
            p.compact,
            p.pending_hint,
            p.is_api_key_auth,
            layout.has_hero_box(),
        )
    };

    WelcomeRenderResult {
        cursor_pos,
        post_flush_escapes,
        menu_rects,
        prompt_rect: if show_picker {
            None
        } else {
            Some(layout.prompt)
        },
        session_picker_hit_areas: picker_close_button,
        import_banner_rect,
        auth_url_rect: None,
        auth_fallback_rect: None,
        menu_scroll: 0,
    }
}

/// Context for session picker rendering.
pub(crate) struct SessionPickerRenderCtx<'a> {
    pub(crate) state: &'a mut crate::views::picker::PickerState,
    pub(crate) sessions: Option<&'a [SessionPickerEntry]>,
    /// Live working directory (tracks `Effect::SetWorkingDir`), used to pin
    /// the current repo's group to the top.
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) loading: bool,
    pub(crate) pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    pub(crate) shortcuts_area: Option<Rect>,
    pub(crate) content_results:
        Option<&'a [kigi_shell::extensions::session_search::SearchSessionHit]>,
    pub(crate) content_loading: bool,
    /// The query `sessions` were server-fetched with (see
    /// [`crate::views::session_picker::effective_filter_query`]).
    pub(crate) entries_query: Option<&'a str>,
    pub(crate) tick: u64,
    /// When true, entries are grouped by `repo_name` with non-selectable headers.
    pub(crate) grouped: bool,
    /// Source filter (local/remote/all) for filtering session entries.
    pub(crate) source_filter: crate::views::session_picker::SourceFilter,
    /// Process-wide `--chat`: hides the source-filter chip and the
    /// deep-search/filter footer hints (see `WelcomeRenderParams::chat_mode`).
    pub(crate) chat_mode: bool,
}

/// Render the session picker list on the welcome screen.
///
/// Builds `PickerEntry` items from `SessionPickerEntry` data and delegates to
/// `render_picker`. Returns `PickerHitAreas` for mouse hit-testing.
pub(crate) fn render_session_picker(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    use crate::views::picker::{self, PickerConfig, PickerEntry, PickerField, PickerRow};
    use crate::views::session_picker::{
        SessionEntryData, build_grouped_picker_entries, build_session_entry_data,
    };

    let entries_data = match ctx.sessions {
        Some(s) => s,
        None => &[],
    };

    // Filter entries by query and source (shared helper). The same effective
    // query must drive filtering AND the content header/rows gates below, or
    // this render disagrees with `handle_welcome_input`'s `build_entry_map`
    // (which receives the effective query) on row indices.
    let filter_query =
        crate::views::session_picker::effective_filter_query(&ctx.state.query, ctx.entries_query);
    let filtered_indices =
        crate::app::app_view::filter_session_entries(ctx.sessions, filter_query, ctx.source_filter);

    let content_width = area.width; // approximate for truncation
    let built = build_session_entry_data(entries_data, &filtered_indices, ctx.state, content_width);

    // Build PickerEntry refs that borrow from `built`.
    let fields_vecs: Vec<Vec<PickerField>> = built
        .iter()
        .map(|b| {
            b.field_data
                .iter()
                .map(|(l, v)| PickerField { label: l, value: v })
                .collect()
        })
        .collect();

    // Build picker entries, optionally grouped by repo_name.
    let (mut picker_entries, non_selectable_indices) = if ctx.grouped {
        let current_repo =
            crate::views::session_picker::repo_name_from_cwd(&ctx.cwd.to_string_lossy());
        build_grouped_picker_entries(
            entries_data,
            &filtered_indices,
            &built,
            &fields_vecs,
            ctx.state,
            Some(current_repo.as_str()),
        )
    } else {
        let entries: Vec<PickerEntry> = built
            .iter()
            .zip(fields_vecs.iter())
            .map(|(b, fields)| {
                PickerEntry::Row(PickerRow {
                    label: &b.summary,
                    right_label: &b.right_text,
                    selected: b.is_selected,
                    expanded: b.is_expanded,
                    fields,
                    description_lines: &[],
                    summary_lines: &[],
                    dimmed: false,
                    indent: 0,
                    badge: b.badge,
                    badge_color: None,
                    collapsible: b.collapsible,
                })
            })
            .collect();
        (entries, Vec::new())
    };

    // Append content search result rows (shared helper handles dedup).
    use crate::views::session_picker::{build_content_entry_data, build_content_header_label};
    // Content rows will start after fuzzy rows + 1 header row.
    let content_start = picker_entries.len() + 1;
    let content_entry_data: Vec<SessionEntryData> = if let Some(hits) = ctx.content_results
        && ctx.source_filter != crate::views::session_picker::SourceFilter::External
        && !filter_query.is_empty()
    {
        build_content_entry_data(
            hits,
            entries_data,
            &filtered_indices,
            ctx.state,
            content_start,
        )
    } else {
        Vec::new()
    };

    // Show header only if there are actual deduped content rows to display.
    let has_content_rows = !content_entry_data.is_empty();
    let content_loading = ctx.content_loading
        && ctx.source_filter != crate::views::session_picker::SourceFilter::External;
    let spinner_label = build_content_header_label(content_loading, has_content_rows, ctx.tick);
    // Only show the header when content results exist or when content
    // search is in progress with a non-empty query.  This must match the
    // header condition inside `build_entry_map` as called from
    // `handle_welcome_input` (app_view.rs) so the input handler's
    // `entry_count` agrees with the rendered entry list — a mismatch causes
    // arrow-key selection to target the wrong row. Both sides therefore gate
    // on the same EFFECTIVE query (`filter_query`), not the live one.
    let show_content_header =
        has_content_rows || (content_loading && !filter_query.trim().is_empty());
    if show_content_header {
        picker_entries.push(PickerEntry::Header {
            label: &spinner_label,
        });
    }

    let content_fields: Vec<Vec<PickerField>> = content_entry_data
        .iter()
        .map(|b| {
            b.field_data
                .iter()
                .map(|(l, v)| PickerField { label: l, value: v })
                .collect()
        })
        .collect();

    let content_snippets: Vec<[&str; 1]> = content_entry_data
        .iter()
        .map(|b| [b.snippet_preview.as_deref().unwrap_or("")])
        .collect();

    for (i, (b, fields)) in content_entry_data
        .iter()
        .zip(content_fields.iter())
        .enumerate()
    {
        let has_snippet = b.snippet_preview.is_some();
        picker_entries.push(PickerEntry::Row(PickerRow {
            label: &b.summary,
            right_label: &b.right_text,
            selected: b.is_selected,
            expanded: b.is_expanded,
            fields,
            description_lines: if has_snippet {
                &content_snippets[i]
            } else {
                &[]
            },
            summary_lines: &[],
            dimmed: false,
            indent: 1,
            badge: if has_snippet { "match" } else { "" },
            badge_color: Some(theme.accent_user),
            collapsible: true,
        }));
    }

    // Build shortcuts for fullscreen mode. Chat mode drops the worktree /
    // deep-search / filter hints (local-Build-row actions).
    let worktree_shortcut: &'static str = "ctrl+w";
    use crate::views::shortcuts_bar::HintItem;
    let mut default_shortcuts: Vec<HintItem> = vec![
        HintItem::new(crate::key!(Esc), "back"),
        HintItem::new(crate::key!(Enter), "select"),
    ];
    if !ctx.chat_mode {
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "worktree".into(),
            custom_display: Some(worktree_shortcut),
            description: None,
            pinned: false,
        });
    }
    default_shortcuts.push(HintItem {
        keys: vec![],
        label: "navigate".into(),
        custom_display: Some("\u{2191}\u{2193}"),
        description: None,
        pinned: false,
    });
    if !ctx.chat_mode {
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "filter".into(),
            custom_display: Some("f"),
            description: None,
            pinned: false,
        });
    }

    let config = PickerConfig {
        title: Some("Resume session"),
        show_search_hint: true,
        expandable: true,
        esc_clears_query: true,
        shortcuts: Some(&default_shortcuts),
        pending_hint: ctx.pending_hint,
        non_selectable: &non_selectable_indices,
        non_selectable_clickable: &[],
        shortcuts_area: ctx.shortcuts_area,
        tabs: None,
        active_tab: 0,
        filter_label: (!ctx.chat_mode).then(|| ctx.source_filter.label()),
        filter_key_hint: (!ctx.chat_mode).then_some("f"),
        filter_active: !ctx.chat_mode && ctx.source_filter.is_active(),
        action_keys: &[],
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: crate::appearance::cache::load_vim_mode(),
    };

    picker::render_picker(
        buf,
        area,
        theme,
        ctx.state,
        &picker_entries,
        &config,
        ctx.loading,
    )
}

/// Render the auth token input box (loopback mode).
fn render_auth_input_box(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    input: &str,
    placeholder: &str,
) {
    let prompt_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_user))
        .padding(Padding {
            left: 2,
            right: 1,
            top: 0,
            bottom: 0,
        });
    let inner = prompt_block.inner(area);
    prompt_block.render(area, buf);

    if inner.height > 0 && inner.width > 2 {
        let display = if input.is_empty() {
            placeholder.to_string()
        } else {
            mask_auth_token_for_display(input)
        };

        let style = if input.is_empty() {
            Style::default().fg(theme.gray_dim)
        } else {
            Style::default().fg(theme.accent_user)
        };

        let line = Line::from(vec![
            Span::styled(
                crate::glyphs::prompt_arrow(),
                Style::default().fg(theme.accent_user),
            ),
            Span::styled(display, style),
        ]);
        buf.set_line(inner.x, inner.y, &line, inner.width);
    }
}

/// Render the first startup warning centered in the given area.
///
/// `startup_warnings` can hold more than one entry (the WezTerm
/// kitty-keyboard banner is prepended ahead of `summarize_warnings()`
/// output — see `diagnostics::assemble_startup_warnings`), but only the
/// first is rendered; all of them point at `/terminal-setup`, which lists
/// every issue. One message line, one optional action line, plus a buffer
/// row for spacing. Severity controls color (yellow for `Warning`, dim
/// for `Info`).
fn render_startup_warnings(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    warnings: &[StartupWarning],
) -> Option<Rect> {
    let w = warnings.first()?;

    // Skip the import-claude startup warning entirely — the import row in the
    // menu now carries the call-to-action with the same visual weight as
    // every other welcome menu item. Showing the warning text in addition to
    // the menu row would be redundant noise.
    if w.message.starts_with("Import Claude settings")
        || w.message.starts_with("Claude settings detected")
    {
        return None;
    }
    let color = match w.severity {
        crate::startup::WarningSeverity::Warning => theme.warning,
        crate::startup::WarningSeverity::Info => theme.gray_dim,
    };
    let style = Style::default().fg(color);

    let mut lines: Vec<Line<'_>> = w
        .message
        .lines()
        .map(|l| Line::from(Span::styled(l, style)).alignment(Alignment::Center))
        .collect();
    if let Some(ref action) = w.action {
        lines.push(Line::from(Span::styled(action.as_str(), style)).alignment(Alignment::Center));
    }

    Paragraph::new(lines).render(area, buf);
    None
}

fn mask_auth_token_for_display(input: &str) -> String {
    use crate::render::line_utils::floor_char_boundary;

    if input.is_empty() {
        return "Paste your token here...".to_string();
    }
    let len = input.len();
    if len <= 8 {
        return input.to_string();
    }
    let boundary = floor_char_boundary(input, len - 4);
    let visible = &input[boundary..];
    let masked_count = input[..boundary].chars().count();
    format!("{}{}", "\u{2022}".repeat(masked_count), visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_view::SessionPickerEntry;
    use crate::views::picker::PickerState;
    use crate::views::session_picker::{build_grouped_picker_entries, build_session_entry_data};

    #[test]
    fn mask_auth_token_cases() {
        assert_eq!(mask_auth_token_for_display(""), "Paste your token here...");
        assert_eq!(mask_auth_token_for_display("12345678"), "12345678");

        let masked = mask_auth_token_for_display("abcdefghij");
        assert!(masked.ends_with("ghij"));
        assert!(masked.starts_with("\u{2022}"));

        // Regression: multi-byte input panicked on byte-index slicing
        let masked = mask_auth_token_for_display("测试令牌一二三四五六");
        assert!(masked.starts_with("\u{2022}"));
    }

    fn make_entry(id: &str, summary: &str, repo_name: &str) -> SessionPickerEntry {
        SessionPickerEntry {
            id: id.into(),
            summary: summary.into(),
            updated_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            cwd: format!("/home/user/{repo_name}"),
            hostname: None,
            source: "local".into(),
            model_id: None,
            num_messages: 1,
            last_active_at: None,
            branch: None,
            repo_name: repo_name.into(),
            worktree_label: None,
            card_detail: None,
        }
    }

    fn render_params<'a>(
        auth_state: &'a AuthState,
        trust_state: &'a TrustState,
        session_picker: Option<&'a [SessionPickerEntry]>,
    ) -> WelcomeRenderParams<'a> {
        WelcomeRenderParams {
            prompt_focus: WelcomePromptFocus::Unfocused,
            auth_state,
            trust_state,
            auth_methods: &[],
            login_label: None,
            auth_code_input: "",
            clipboard_copied: false,
            show_raw_url: false,
            tip: None,
            model_name: "test",
            flags: &[],
            selected: None,
            menu_scroll: 0,
            has_claude_import: false,
            mouse_pos: None,
            session_picker,
            session_picker_loading: false,
            compact: false,
            pending_hint: None,
            startup_warnings: &[],
            pending_update_version: None,
            foreign_resume_hint: None,
            is_api_key_auth: false,
            session_picker_content_results: None,
            session_picker_content_loading: false,
            session_picker_entries_query: None,
            welcome_tick: 0,
            session_picker_grouped: false,
            session_picker_source_filter: crate::views::session_picker::SourceFilter::All,
            chat_mode: false,
            cwd: std::path::Path::new("/repo"),
        }
    }

    fn render_done_text(params: &WelcomeRenderParams<'_>) -> String {
        render_done_text_h(params, 40)
    }

    /// Render at an explicit height. The login picker lists one row per
    /// advertised platform; with ~20 API-key providers the menu no longer
    /// fits a 40-row viewport, so the full-menu content test needs more rows
    /// to verify every label renders. (On-terminal, rows past the fold are
    /// clipped — the `q`/`l` shortcuts still work; see the menu-scroll debt.)
    fn render_done_text_h(params: &WelcomeRenderParams<'_>, height: u16) -> String {
        let area = Rect::new(0, 0, 100, height);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();
        render_welcome(area, &mut buf, params, &mut prompt, &mut picker);
        buffer_text(&buf)
    }

    /// The unauthenticated welcome menu lists one row per interactive login
    /// method — the OAuth device login plus BOTH Moonshot open platforms —
    /// and Quit, when the shell advertises all three.
    #[test]
    fn pending_menu_lists_three_login_rows_plus_quit() {
        use kigi_shell::agent::auth_method::{AuthMethodsBuildInputs, build_auth_methods};
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: false,
            login_label: None,
        });
        let auth = AuthState::Pending { error: None };
        let trust = TrustState::Done;
        let mut params = render_params(&auth, &trust, None);
        params.auth_methods = &built.methods;
        // Tall viewport so every advertised login row renders (the picker now
        // lists ~20 platforms); this asserts content coverage, not fit.
        let text = render_done_text_h(&params, 72);
        assert!(text.contains("Kimi Code (OAuth)"), "{text}");
        assert!(
            text.contains("xAI Grok (subscription) (OAuth)"),
            "the xai-grok interactive OAuth login row must render: {text}"
        );
        assert!(
            text.contains("Claude Pro/Max (subscription) (OAuth)"),
            "the claude-pro-max interactive OAuth login row must render: {text}"
        );
        assert!(
            text.contains("GitHub Copilot (subscription) (OAuth)"),
            "the github-copilot interactive OAuth login row must render: {text}"
        );
        assert!(
            text.contains("ChatGPT Plus/Pro (Codex) (OAuth)"),
            "the openai-codex interactive OAuth login row must render: {text}"
        );
        assert!(
            text.contains("Moonshot Open Platform (API key \u{b7} moonshot.cn)"),
            "{text}"
        );
        assert!(
            text.contains("Moonshot Open Platform (API key \u{b7} moonshot.ai)"),
            "{text}"
        );
        assert!(text.contains("Quit"), "{text}");
        // Shortcut hints for muscle memory: `l` (first row) and `q` (Quit).
        assert!(text.contains('l'), "{text}");
        assert!(text.contains('q'), "{text}");
    }

    /// With one row per advertised platform the login picker outgrows a
    /// normal terminal. The stacked layout must cap the menu to the rows
    /// genuinely left over (never letting the constraint solver squeeze the
    /// moon or the bottom chrome) and the menu must scroll its viewport to
    /// keep the selected row visible.
    #[test]
    fn pending_menu_scrolls_and_never_clips_the_moon() {
        use kigi_shell::agent::auth_method::{AuthMethodsBuildInputs, build_auth_methods};
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: false,
            has_cached_token: false,
            login_label: None,
        });
        let auth = AuthState::Pending { error: None };
        let trust = TrustState::Done;
        let mut params = render_params(&auth, &trust, None);
        params.auth_methods = &built.methods;

        // A realistic 40-row terminal: far too short for one row per method.
        let text = render_done_text_h(&params, 40);
        let braille_rows = text
            .lines()
            .filter(|l| l.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)))
            .count();
        assert_eq!(
            braille_rows, 10,
            "the full moon must render whole, not squeezed:\n{text}"
        );
        assert!(
            text.contains(kigi_version::VERSION),
            "the version badge must survive the layout:\n{text}"
        );

        // The selection drives the viewport: selecting the last row (Quit)
        // must scroll it into view — and scroll the first row out.
        let items = pending_menu_items(params.auth_methods, None);
        params.selected = Some(items.len() - 1);
        let text = render_done_text_h(&params, 40);
        assert!(
            text.contains("Quit"),
            "the selected row must be scrolled into view:\n{text}"
        );
        assert!(
            !text.contains("Kimi Code (OAuth)"),
            "the viewport must actually scroll (first row off-screen):\n{text}"
        );
    }

    /// An old/limited shell that advertises only `kimi-code` keeps the
    /// two-row shape (single login row + Quit) — and never a Moonshot row.
    #[test]
    fn pending_menu_without_moonshot_methods_keeps_two_rows() {
        let methods = vec![kigi_shell::agent::auth_method::kimi_code_auth_method(None)];
        let auth = AuthState::Pending { error: None };
        let trust = TrustState::Done;
        let mut params = render_params(&auth, &trust, None);
        params.auth_methods = &methods;
        let text = render_done_text(&params);
        assert!(text.contains("Kimi Code (OAuth)"), "{text}");
        assert!(!text.contains("Moonshot"), "{text}");
    }

    /// The Moonshot API-key entry arm renders the platform copy, the paste
    /// box, and the esc-back hint — and no OAuth-URL affordances.
    #[test]
    fn api_key_entry_arm_shows_platform_copy_and_paste_box() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();

        let (copy_rect, fallback_rect) = render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            None, // auth_url — none in key-entry mode
            AuthMode::ApiKeyEntry(crate::app::app_view::PlatformLogin(
                kigi_shell::models::PlatformId::MoonshotCn,
            )),
            "",    // auth_code_input
            false, // clipboard_copied
            false, // show_raw_url
        );

        let text = buffer_text(&buf);
        // The instruction may soft-wrap; assert its two halves (each stays an
        // intact word run on one row).
        assert!(
            text.contains("Paste your Moonshot API key"),
            "key-entry arm must show the platform instruction, got:\n{text}"
        );
        assert!(
            text.contains("platform.moonshot.cn"),
            "key-entry arm must name the platform console, got:\n{text}"
        );
        assert!(
            text.contains("Paste your API key here..."),
            "key-entry arm must render the paste box placeholder, got:\n{text}"
        );
        assert!(
            text.contains("esc") && text.contains("back"),
            "key-entry arm must hint esc-back, got:\n{text}"
        );
        assert!(copy_rect.is_none() && fallback_rect.is_none());
    }

    #[test]
    fn foreign_resume_tip_names_each_tool_and_age() {
        use kigi_workspace::foreign_sessions::ForeignSessionTool;

        let auth = AuthState::Done;
        let trust = TrustState::Done;
        for (tool, label) in [
            (ForeignSessionTool::Claude, "Claude Code"),
            (ForeignSessionTool::Codex, "Codex"),
            (ForeignSessionTool::Cursor, "Cursor"),
        ] {
            let hint = kigi_workspace::foreign_sessions::RecentForeignSession {
                tool,
                native_id: "native-id".into(),
                age: std::time::Duration::from_secs(125),
            };
            let mut params = render_params(&auth, &trust, None);
            params.foreign_resume_hint = Some(&hint);
            let text = render_done_text(&params);
            assert!(text.contains(&format!("Coming from {label}?")), "{text}");
            assert!(text.contains("2m ago"), "{text}");
            assert!(text.contains("ctrl+u"), "{text}");
        }
    }

    #[test]
    fn pending_update_suppresses_foreign_resume_tip() {
        let auth = AuthState::Done;
        let trust = TrustState::Done;
        let hint = kigi_workspace::foreign_sessions::RecentForeignSession {
            tool: kigi_workspace::foreign_sessions::ForeignSessionTool::Cursor,
            native_id: "native-id".into(),
            age: std::time::Duration::from_secs(30),
        };
        let mut params = render_params(&auth, &trust, None);
        params.foreign_resume_hint = Some(&hint);
        params.pending_update_version = Some("9.9.9");

        let text = render_done_text(&params);
        assert!(text.contains("v9.9.9 available"), "{text}");
        assert!(!text.contains("Coming from Cursor?"), "{text}");
    }

    fn png() -> [u8; 8] {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
    }

    fn seed_static_owner(owner_id: u64) {
        let _ = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id)
            .unwrap()
            .commit();
    }

    fn assert_promptless_clear(result: WelcomeRenderResult, owner_id: u64) {
        let post_flush = result
            .post_flush_escapes
            .expect("promptless welcome must clear ID 1");
        assert!(post_flush.as_str().contains("a=d"));
        let before_write =
            crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id).unwrap();
        assert!(
            !before_write.as_str().contains("a=t"),
            "constructing the clear must not commit ownership"
        );
        post_flush.write_to(&mut Vec::new()).unwrap();
        let after_write =
            crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id).unwrap();
        assert!(
            after_write.as_str().contains("a=t"),
            "writing the clear must commit ownership"
        );
    }

    #[test]
    fn authenticating_welcome_returns_paired_overlay_clear() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(81);
        let auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Command,
        };
        let trust_state = TrustState::Done;
        let params = render_params(&auth_state, &trust_state, None);
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();

        let result = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_promptless_clear(result, 81);
    }

    #[test]
    fn picker_welcome_returns_paired_overlay_clear() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(82);
        let auth_state = AuthState::Done;
        let trust_state = TrustState::Done;
        let sessions = [make_entry("session-1", "summary", "repo")];
        let params = render_params(&auth_state, &trust_state, Some(&sessions));
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();

        let result = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_promptless_clear(result, 82);
    }

    /// RENDER half of the header-gate invariant (input half:
    /// `session_picker::tests::grouped_entry_map_empty_query_with_loading_has_no_header`):
    /// with stamp==live and a re-search in flight, the "Searching…" header
    /// must NOT render — a render-only header row shifts arrow-key row
    /// indices. Control leg: the same search WITHOUT the stamp keeps it.
    #[test]
    fn render_header_gate_uses_effective_query() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let theme = crate::theme::Theme::default();
        let area = Rect::new(0, 0, 80, 20);
        // Content-only hit: title shares nothing with the query "hit".
        let entries = vec![make_entry("conv-1", "Quarterly roadmap notes", "repo")];

        let render = |entries_query: Option<&str>| -> String {
            let mut buf = Buffer::empty(area);
            let mut state = PickerState {
                query: "hit".into(),
                ..PickerState::default()
            };
            render_session_picker(
                area,
                &mut buf,
                &theme,
                &mut SessionPickerRenderCtx {
                    state: &mut state,
                    sessions: Some(&entries),
                    cwd: std::path::Path::new("/repo"),
                    loading: false,
                    pending_hint: None,
                    shortcuts_area: None,
                    content_results: None,
                    content_loading: true,
                    entries_query,
                    tick: 0,
                    grouped: false,
                    source_filter: crate::views::session_picker::SourceFilter::All,
                    chat_mode: true,
                },
            );
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| {
                            buf.cell((x, y))
                                .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let stamped = render(Some("hit"));
        assert!(
            !stamped.contains("Searching session content"),
            "stamp==live must not render the search header:\n{stamped}"
        );
        assert!(
            stamped.contains("Quarterly roadmap notes"),
            "stamped server hit must render:\n{stamped}"
        );

        // Control: unstamped in-flight search keeps the header, proving the
        // negative assertion above exercises the gate.
        let unstamped = render(None);
        assert!(
            unstamped.contains("Searching session content"),
            "in-flight search without the stamp must render the header:\n{unstamped}"
        );
    }

    #[test]
    fn grouped_entries_insert_headers() {
        let entries = vec![
            make_entry("s1", "Fix auth", "xai"),
            make_entry("s2", "Add streaming", "xai"),
            make_entry("s3", "Nuke tables", "fw-1"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        // 2 headers + 3 rows = 5 entries
        assert_eq!(result.len(), 5);
        // Groups are sorted alphabetically: fw-1 before xai.
        // Header positions: 0 (fw-1), 2 (xai)
        assert_eq!(non_sel.len(), 5);
        assert!(non_sel[0], "first entry should be header (non-selectable)");
        assert!(!non_sel[1], "second entry should be selectable row");
        assert!(non_sel[2], "third entry should be header (non-selectable)");
        assert!(!non_sel[3], "fourth entry should be selectable row");
        assert!(!non_sel[4], "fifth entry should be selectable row");

        // Verify headers
        assert!(
            matches!(&result[0], crate::views::picker::PickerEntry::Header { label } if label == &"fw-1")
        );
        assert!(
            matches!(&result[2], crate::views::picker::PickerEntry::Header { label } if label == &"xai")
        );
    }

    #[test]
    fn grouped_entries_pin_current_repo_first() {
        // Render path (build_grouped_picker_entries) must pin the current
        // working directory's repo group ahead of the alphabetical rest,
        // matching build_entry_map's index ordering.
        let entries = vec![
            make_entry("s1", "Fix auth", "aaa"),
            make_entry("s2", "Add streaming", "zzz"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        // Pin "zzz": it leads despite sorting last alphabetically.
        let (result, _) = build_grouped_picker_entries(
            &entries,
            &indices,
            &built,
            &fields_vecs,
            &state,
            Some("zzz"),
        );
        assert!(
            matches!(&result[0], crate::views::picker::PickerEntry::Header { label } if label == &"zzz"),
            "current repo group pinned first"
        );
        assert!(
            matches!(&result[2], crate::views::picker::PickerEntry::Header { label } if label == &"aaa"),
            "remaining group follows alphabetically"
        );
    }

    #[test]
    fn grouped_entries_single_group_has_one_header() {
        let entries = vec![
            make_entry("s1", "Fix auth", "xai"),
            make_entry("s2", "Add streaming", "xai"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        assert_eq!(result.len(), 3); // 1 header + 2 rows
        assert!(non_sel[0]);
        assert!(!non_sel[1]);
        assert!(!non_sel[2]);
    }

    #[test]
    fn grouped_entries_empty_input() {
        let entries: Vec<SessionPickerEntry> = vec![];
        let indices: Vec<usize> = vec![];
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> = vec![];

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        assert!(result.is_empty());
        assert!(non_sel.is_empty());
    }

    #[test]
    fn grouped_entries_rows_are_indented() {
        let entries = vec![make_entry("s1", "Fix auth", "xai")];
        let indices: Vec<usize> = vec![0];
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, _) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        // The row (second entry) should have indent=1
        if let crate::views::picker::PickerEntry::Row(row) = &result[1] {
            assert_eq!(row.indent, 1);
        } else {
            panic!("expected Row, got Header");
        }
    }

    fn resume_picker_config() -> crate::views::picker::PickerConfig<'static> {
        crate::views::picker::PickerConfig {
            title: Some("Resume session"),
            show_search_hint: true,
            expandable: true,
            esc_clears_query: true,
            shortcuts: None,
            pending_hint: None,
            non_selectable: &[],
            non_selectable_clickable: &[],
            shortcuts_area: None,
            tabs: None,
            active_tab: 0,
            filter_label: None,
            filter_key_hint: None,
            filter_active: false,
            action_keys: &[],
            disable_search: false,
            compact_bottom_bar: false,
            search_only_on_slash: false,
            vim_normal_first: false,
        }
    }

    #[test]
    fn e_key_expands_selected_entry_in_resume_picker() {
        use crate::views::picker::{PickerOutcome, handle_picker_input};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut state = PickerState::default();
        let config = resume_picker_config();
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let outcome = handle_picker_input(&ev, &mut state, 3, &config);
        assert!(matches!(outcome, PickerOutcome::Expand(0)));
    }

    #[test]
    fn e_key_routes_to_search_when_active() {
        use crate::views::picker::{PickerOutcome, handle_picker_input};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut state = PickerState {
            search_active: true,
            ..PickerState::default()
        };
        let config = resume_picker_config();
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let outcome = handle_picker_input(&ev, &mut state, 3, &config);
        assert!(matches!(outcome, PickerOutcome::Changed));
        assert_eq!(state.query, "e");
    }

    #[test]
    fn hero_box_active_on_wide_tall_terminal() {
        // 90 cols, 50 rows: meets the minimum for the hero box.
        let area = Rect::new(0, 0, 90, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(layout.has_hero_box(), "hero box should be active at 90x50");
        assert!(layout.hero_box.width > 0);
        assert!(layout.hero_box.height > 0);
        // Logo and menu slots are zero in hero box mode (content is inside the box).
        assert_eq!(layout.logo.width, 0);
        assert_eq!(layout.menu.width, 0);
        // Sub-rects inside the hero box are valid.
        assert!(layout.hero_logo.height > 0);
        assert!(layout.hero_menu.height > 0);
        assert_eq!(layout.hero_version.height, 1);
        assert_eq!(layout.hero_subtitle.height, 1);
    }

    #[test]
    fn hero_box_inactive_on_narrow_terminal() {
        // 80 cols is below the 90-col threshold.
        let area = Rect::new(0, 0, 80, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 80x50"
        );
        assert_eq!(layout.hero_box.width, 0);
    }

    #[test]
    fn hero_box_boundary_at_min_width() {
        let just_below = Rect::new(0, 0, 89, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: just_below,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 89 cols"
        );

        let at_threshold = Rect::new(0, 0, 90, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: at_threshold,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            layout.has_hero_box(),
            "hero box should be active at 90 cols"
        );
    }

    #[test]
    fn hero_box_inactive_when_compact() {
        // Compact mode (session picker visible) never uses the hero box.
        let area = Rect::new(0, 0, 120, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            compact: true,
            prompt_compact: true,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive in compact mode"
        );
        assert_eq!(layout.hero_box.width, 0);
    }

    #[test]
    fn hero_box_inactive_on_short_terminal() {
        // 16 rows is one short of the 17 the box needs (11 box + 1 flex gap +
        // 5 fixed-below), so it falls back to the stacked layout.
        let area = Rect::new(0, 0, 90, 16);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 90x16 (needs 17 rows)"
        );
    }

    #[test]
    fn hero_box_inactive_when_warning_would_overflow() {
        // Regression: the box is forced to the full 10-row moon, so even a
        // 3-item menu needs 14 box rows (min content 20). A startup warning
        // (error_height = 2, +1 gap) pushes the total past height 20, so the
        // gate must fall back to the stacked layout instead of overflowing.
        let area = Rect::new(0, 0, 90, 20);
        let with_warning = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            error_height: 2,
            menu_height: 3,
            ..Default::default()
        });
        assert!(!with_warning.has_hero_box());
        // The same terminal fits the box once the warning is gone.
        let no_warning = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        assert!(no_warning.has_hero_box());
    }

    #[test]
    fn blocked_layout_stays_stacked_on_wide_terminal() {
        // The login / ZDR screens render through render_welcome_blocked, which
        // only paints the stacked logo/menu rects. compute_stacked must never
        // hand them a hero-box layout (which zeroes those rects), even on a
        // wide, tall terminal where the normal path picks the hero box.
        let area = Rect::new(0, 0, 120, 40);
        assert!(
            WelcomeLayout::compute(WelcomeLayoutInput {
                content_area: area,
                menu_height: 2,
                ..Default::default()
            })
            .has_hero_box(),
            "sanity: the normal path should pick the hero box at 120x40"
        );
        let blocked = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
            content_area: area,
            menu_height: 2,
            ..Default::default()
        });
        assert!(!blocked.has_hero_box());
        assert!(
            blocked.logo.height > 0,
            "logo must be painted on the login screen"
        );
        assert!(
            blocked.menu.height > 0,
            "menu must be painted on the login screen"
        );
    }

    #[test]
    fn hero_box_does_not_overflow_with_tall_menu() {
        // A tall menu can outgrow the default box, so the centering pad
        // (derived from the default box) must be clamped or the box gets
        // pushed down and the version row clips at exactly
        // min_content_height. 20 == min_content_height(0, 6, 0, 0): a 14-row
        // box (10-row moon) + 1 flex gap + 5 fixed-below.
        let area = Rect::new(0, 0, 100, 20);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 6,
            ..Default::default()
        });
        assert!(
            layout.has_hero_box(),
            "hero box should be active at the boundary"
        );
        // top_pad must clamp to 0, so the box sits at the top, not pushed down.
        assert_eq!(
            layout.hero_box.y, area.y,
            "box pushed down by unclamped pad"
        );
        assert!(
            layout.version.y + layout.version.height <= area.y + area.height,
            "version row (y={}, h={}) overflows content height {}",
            layout.version.y,
            layout.version.height,
            area.height,
        );
    }

    #[test]
    fn hero_box_height_accounts_for_borders_and_padding() {
        // At h >= 26 the full moon is used (10 lines). With menu_height=3:
        // right_col = 2 + 0 + 0 + 1 + 3 = 6, inner = max(10, 6) = 10.
        // hero_box_height = 2 (borders) + 2 (v_pad) + 10 = 14.
        let area = Rect::new(0, 0, 100, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        assert!(layout.has_hero_box());
        assert_eq!(layout.hero_box.height, 14);
    }

    #[test]
    fn hero_box_logo_top_aligned() {
        let area = Rect::new(0, 0, 100, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        // Logo y should be at hero_box.y + 1 (border) + 1 (v_pad).
        assert_eq!(layout.hero_logo.y, layout.hero_box.y + 2);
    }

    /// Flatten a rendered buffer into one string for substring assertions.
    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn extract_user_code_parses_verification_url() {
        // The live Kimi device flow returns this exact URL shape.
        assert_eq!(
            extract_user_code("https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH"),
            Some("ABCD-EFGH"),
        );
        // Trailing params after the code are ignored.
        assert_eq!(
            extract_user_code(
                "https://www.kimi.com/code/authorize_device?user_code=WXYZ-1234&foo=bar"
            ),
            Some("WXYZ-1234"),
        );
        // A param whose name merely ends in `user_code` must not be matched.
        assert_eq!(
            extract_user_code("https://example.com/d?foo_user_code=BAD&user_code=GOOD"),
            Some("GOOD"),
        );
        // No code param, empty code, and unexpected characters all yield None.
        assert_eq!(
            extract_user_code("https://www.kimi.com/code/authorize_device"),
            None
        );
        assert_eq!(extract_user_code("https://example.com/d?user_code="), None);
        assert_eq!(
            extract_user_code("https://example.com/d?user_code=AB%20CD"),
            None
        );
    }

    #[test]
    fn device_auth_arm_shows_url_and_no_paste_box() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH";

        let (copy_rect, fallback_rect) = render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",    // auth_code_input — unused in device mode
            false, // clipboard_copied
            false, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("Approve in your browser"),
            "device arm must show the approval header, got:\n{text}"
        );
        // Device code shown for the browser-match check (anti-phishing).
        assert!(
            text.contains("ABCD-EFGH"),
            "device arm must show the device code, got:\n{text}"
        );
        assert!(
            text.contains("Make sure your browser shows this code"),
            "device arm must show the code caption, got:\n{text}"
        );
        // Copy affordance (click-to-copy line) is present.
        assert!(
            text.contains("to copy"),
            "device arm must show the copy-URL affordance, got:\n{text}"
        );
        // No manual-paste affordance in device mode.
        assert!(
            !text.contains("Paste your token"),
            "device arm must NOT render the token paste box, got:\n{text}"
        );
        // Copy + fallback links are clickable.
        assert!(
            copy_rect.is_some(),
            "device arm must expose a copy hit-rect"
        );
        assert!(
            fallback_rect.is_some(),
            "device arm must expose a show-full-URL hit-rect"
        );
    }

    #[test]
    fn device_auth_arm_raw_url_mode_shows_full_url() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://www.kimi.com/code/authorize_device?user_code=WXYZ-1234";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            false,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("WXYZ-1234"),
            "raw URL mode must render the full URL including the user code, got:\n{text}"
        );
    }

    #[test]
    fn raw_url_mode_centers_url_that_fits_on_one_line() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://www.kimi.com/code/authorize_device?user_code=WXYZ-1234";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            false,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        let url_line = text
            .lines()
            .find(|l| l.contains("https://"))
            .expect("raw URL mode must render the URL");
        // Whole URL on one line, not wrapped.
        assert!(url_line.contains(url), "URL must be intact: {url_line:?}");
        // Centered: leading pad within 1 cell of trailing pad (integer split).
        let lead = url_line.len() - url_line.trim_start().len();
        let trail = url_line.len() - url_line.trim_end().len();
        assert!(
            lead > 0 && lead.abs_diff(trail) <= 1,
            "URL must be horizontally centered, lead={lead} trail={trail}:\n{text}"
        );
    }

    #[test]
    fn raw_url_mode_uses_full_width_for_long_urls() {
        let area = Rect::new(0, 0, 40, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        // 40-col terminal; URL longer than one row must wrap at the exact
        // screen edge with no leading spaces so copy-paste stays intact.
        let url = "https://www.kimi.com/code/authorize_device?user_code=WXYZ-1234&extra=0123456789";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            false,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        let mut lines = text.lines();
        let first = lines
            .by_ref()
            .find(|l| l.contains("https://"))
            .expect("raw URL mode must render the URL");
        let second = lines.next().expect("URL must wrap to a second row");
        // First row flush against both edges (full width), remainder on the
        // next row starting at column 0.
        assert_eq!(
            first,
            &url[..40],
            "long URL row must span the full terminal width:\n{text}"
        );
        assert!(
            second.starts_with(&url[40..]),
            "wrapped remainder must start at column 0:\n{text}"
        );
    }

    #[test]
    fn command_auth_arm_shows_url_and_waiting() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://example.com/oauth2/authorize?client_id=kigi";

        let (copy_rect, fallback_rect) = render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Command,
            "",    // auth_code_input — unused
            false, // clipboard_copied
            false, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("A browser window will open"),
            "command arm must show the auth header, got:\n{text}"
        );
        assert!(
            text.contains("Waiting for login to complete"),
            "command arm must show the waiting status, got:\n{text}"
        );
        // No device code — that's device-flow only.
        assert!(
            !text.contains("Make sure your browser shows this code"),
            "command arm must NOT show the device-code caption, got:\n{text}"
        );
        // No manual-paste affordance in command mode.
        assert!(
            !text.contains("Paste your token"),
            "command arm must NOT render the token paste box, got:\n{text}"
        );
        // Copy + fallback links are clickable.
        assert!(
            copy_rect.is_some(),
            "command arm must expose a copy hit-rect"
        );
        assert!(
            fallback_rect.is_some(),
            "command arm must expose a show-full-URL hit-rect"
        );
    }
}
