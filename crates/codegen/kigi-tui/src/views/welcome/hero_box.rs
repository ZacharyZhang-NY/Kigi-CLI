//! Hero box component — side-by-side logo + menu inside a bordered box.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::theme::Theme;

use super::WelcomeLayout;

/// Minimum terminal width for the side-by-side hero box layout.
pub(super) const HERO_BOX_MIN_WIDTH: u16 = 90;

/// Vertical padding (rows) between the box border and its inner content.
const V_PAD: u16 = 1;

/// Horizontal inset (cols) between the right column's content and the box
/// border; also the collapsed left-column width when the logo is hidden.
const H_INSET: u16 = 2;

/// Horizontal gap (cols) between the logo and the right column inside the box.
const LOGO_H_PAD: u16 = 3;

const HERO_SUBTITLE: &str = "Thanks for trying Kigi, give feedback with /feedback!";

use super::{PROMPT_HEIGHT, VERSION_GAP};

/// Height of the hero box's right column: version + subtitle + the gap
/// before the menu + the menu itself.
fn right_col_height(menu_height: u16) -> u16 {
    // version(1) + subtitle(1) + gap-before-menu(1) + menu
    3 + menu_height
}

/// Minimum content-area height the hero box needs to render without truncating:
/// the optional error row, the box, a one-row flex gap, and the fixed rows
/// below (tip + prompt + version). The box always shows the full-height logo,
/// so a terminal shorter than this falls back to the stacked layout instead of
/// overflowing.
pub(super) fn min_content_height(error_height: u16, menu_height: u16, tip_height: u16) -> u16 {
    let inner = super::logo::full_logo_line_count().max(right_col_height(menu_height));
    let hero_box_height = 2 + V_PAD * 2 + inner;
    let gap_after_error = if error_height > 0 { 1u16 } else { 0 };
    gap_after_error + error_height + hero_box_height + 1 + WelcomeLayout::fixed_below(tip_height)
}

/// Width (cols) of the hero box's left (logo) column, including padding.
/// Collapses to a small inset when the logo is hidden.
fn left_col_width() -> u16 {
    let logo_width = super::logo::full_logo_visual_width();
    if logo_width == 0 {
        H_INSET
    } else {
        logo_width + LOGO_H_PAD.saturating_sub(1) + LOGO_H_PAD
    }
}

/// Compute the hero box layout: bordered box with logo left, version + menu right.
pub(super) fn compute_hero_box(
    content_area: Rect,
    error_height: u16,
    menu_height: u16,
    tip_height: u16,
) -> WelcomeLayout {
    let zero = Rect::default();
    let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
    let fixed_below = WelcomeLayout::fixed_below(tip_height);

    // Column widths are height-independent, so derive them once and reuse for
    // both the measurement and the rects.
    let box_width = content_area.width.saturating_sub(6).min(120);
    let inner_width = box_width.saturating_sub(2);
    let left_col_width = left_col_width();
    let right_width = inner_width.saturating_sub(left_col_width);
    let menu_slot_width = right_width.saturating_sub(H_INSET);

    let logo_rows = super::logo::full_logo_line_count();
    let inner_height = logo_rows.max(right_col_height(menu_height));
    let hero_box_height = 2 + V_PAD * 2 + inner_height;

    let gap_after_error = if error_height > 0 { 1 } else { 0 };
    let fixed_above = gap_after_error + error_height;

    // Top padding for vertical centering (use the default menu height so the
    // logo position stays constant regardless of picker/focus state).
    let default_menu_height = 4u16;
    let default_inner = logo_rows.max(right_col_height(default_menu_height));
    let default_hero = 2 + V_PAD * 2 + default_inner;
    let remaining = content_area.height.saturating_sub(fixed_above);
    let top_pad = remaining
        .saturating_sub(default_hero)
        .saturating_sub(fixed_below)
        / 3;
    // Centering derives top_pad from the default-menu box, but the fit gate
    // (min_content_height) sizes for the actual box with no pad. Clamp to the
    // real slack so a taller-than-default menu can't push the rows below the
    // box off the bottom at the tight boundary.
    let top_pad = top_pad.min(
        content_area
            .height
            .saturating_sub(fixed_above + hero_box_height + 1 + fixed_below),
    );

    let [
        _,
        _,
        error,
        hero_box_slot,
        _,
        tip,
        _,
        prompt,
        _,
        version_slot,
    ] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(gap_after_error),
        Constraint::Length(error_height),
        Constraint::Length(hero_box_height),
        Constraint::Min(1), // flex gap
        Constraint::Length(tip_height),
        Constraint::Length(tip_gap),
        Constraint::Length(PROMPT_HEIGHT),
        Constraint::Length(VERSION_GAP),
        Constraint::Length(1),
    ])
    .areas(content_area);

    // Horizontally center the hero box (`box_width` derived above).
    let [_, hero_box, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(box_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(hero_box_slot);

    // Inner area inside the border + v_pad. Widths reuse the values above; only
    // x/y come from the laid-out box.
    let inner = Rect {
        x: hero_box.x + 1,
        y: hero_box.y + 1 + V_PAD,
        width: inner_width,
        height: inner_height,
    };

    // Left column: balanced padding around the logo; collapses to a small
    // inset when the logo is hidden.
    let logo_width = super::logo::full_logo_visual_width();
    // Logo body leans right; shave a column off the left pad to optically center.
    let logo_left_pad = LOGO_H_PAD.saturating_sub(1);

    // Logo top-aligned, horizontally centered within left column.
    let hero_logo = Rect {
        x: inner.x + logo_left_pad,
        y: inner.y,
        width: logo_width.min(inner.width.saturating_sub(logo_left_pad)),
        height: logo_rows.min(inner.height),
    };

    // Right column: rest of inner width after left column.
    let right_x = inner.x + left_col_width;

    // Version line at top of right column.
    let hero_version = Rect {
        x: right_x,
        y: inner.y,
        width: right_width,
        height: 1,
    };

    // Subtitle line below version.
    let hero_subtitle = Rect {
        x: right_x,
        y: inner.y + 1,
        width: right_width,
        height: 1,
    };

    // version + subtitle + gap-before-menu
    let right_header_rows = 3;

    // Menu below the header rows, left-aligned in right column.
    let hero_menu = Rect {
        x: right_x,
        y: inner.y + right_header_rows,
        width: menu_slot_width,
        height: menu_height.min(inner.height.saturating_sub(right_header_rows)),
    };

    WelcomeLayout {
        logo: zero,
        error,
        menu: zero,
        tip,
        prompt,
        version: version_slot,
        hero_box,
        hero_logo,
        hero_version,
        hero_subtitle,
        hero_menu,
    }
}

/// Render the bordered hero box with logo left, version + subtitle + menu right.
/// Returns the hit-test rect per menu item row (for click/hover).
pub(super) fn render_hero_box(
    layout: &WelcomeLayout,
    buf: &mut Buffer,
    theme: &Theme,
    menu_items: &[(&str, &str)],
    selected: Option<usize>,
    mouse_pos: Option<(u16, u16)>,
) -> Vec<Rect> {
    // Dim the box border toward the background for a softer, dimmer gray.
    let border_color = crate::render::color::blend_color(theme.bg_base, theme.gray_dim, 0.45)
        .unwrap_or(theme.gray_dim);
    let border_block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    border_block.render(layout.hero_box, buf);

    super::logo::render_full_logo(layout.hero_logo, buf, theme);

    super::render_version_badge(
        layout.hero_version,
        buf,
        theme,
        None,
        0,
        false,
        super::VersionBadgeMode::HeroInline,
    );

    // Subtitle line below the version.
    let subtitle_style = Style::default().fg(theme.gray);
    buf.set_span(
        layout.hero_subtitle.x,
        layout.hero_subtitle.y,
        &Span::styled(HERO_SUBTITLE, subtitle_style),
        layout.hero_subtitle.width,
    );

    super::menu::render_menu(
        layout.hero_menu,
        buf,
        theme,
        menu_items,
        selected,
        mouse_pos,
        layout.hero_menu.width,
        0,
    )
    .0
}
