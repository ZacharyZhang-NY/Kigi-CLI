//! Menu component — renders shortcut key menus.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme::Theme;

use super::logo::logo_visual_width;

/// Key-column badge for a login-picker row whose provider already holds a
/// stored credential. Rendered green (content-based restyle, like the
/// import row's `[x]`).
pub(crate) const CONNECTED_BADGE: &str = "connected";

/// Render the welcome menu rows as `label … shortcut`, padded within each row.
///
/// The area is a viewport: when there are more items than rows, the window
/// scrolls minimally from `scroll` (the previous frame's offset) to keep
/// `selected` visible, and a scrollbar marks the position. Minimal scroll —
/// never re-centering — keeps rows stable under the mouse, so hover-select
/// cannot shift the list it is pointing at. Returns one Rect per item,
/// index-aligned for hit-testing (off-screen items get a zero Rect, which
/// never hit-tests true), plus the offset actually used, which the caller
/// feeds back next frame.
#[allow(clippy::too_many_arguments)]
pub fn render_menu(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    items: &[(&str, &str)],
    selected: Option<usize>,
    mouse_pos: Option<(u16, u16)>,
    min_width_hint: u16,
    scroll: usize,
) -> (Vec<Rect>, usize) {
    let label_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);
    let label_selected_style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.gray_bright);
    let key_selected_style = Style::default()
        .fg(theme.gray_bright)
        .bg(theme.bg_highlight);

    // Width: label + gap + key. Keep a 4-col gap between label and key for
    // readability.
    let content_min: u16 = items
        .iter()
        .map(|(key, label)| (key.len() + label.len() + 4) as u16)
        .max()
        .unwrap_or(0);
    let menu_width = logo_visual_width(area.height)
        .max(30)
        .max(content_min)
        .max(min_width_hint);

    let [_, menu_centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(menu_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    let total = items.len();
    let visible = menu_centered.height as usize;
    let offset = if total > visible && visible > 0 {
        let mut off = scroll.min(total - visible);
        if let Some(sel) = selected {
            if sel < off {
                // scroll up just enough
                off = sel;
            } else if sel >= off + visible {
                // scroll down just enough
                off = sel + 1 - visible;
            }
        }
        off
    } else {
        0
    };

    let mut rects = vec![Rect::default(); total];
    for (row, (i, (key, label))) in items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let y = menu_centered.y + row as u16;
        let is_selected = selected == Some(i);
        let key_width = key.len() as u16;
        let label_len = label.len() as u16;

        let row_rect = Rect {
            x: menu_centered.x,
            y,
            width: menu_centered.width,
            height: 1,
        };
        rects[i] = row_rect;

        // Fill row background when selected/hovered
        if is_selected {
            let hover_bg = Style::default().bg(theme.bg_highlight);
            for x in menu_centered.x..menu_centered.x + menu_centered.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(hover_bg);
                }
            }
        }

        // Label, flush with the left edge of the menu column.
        let lstyle = if is_selected {
            label_selected_style
        } else {
            label_style
        };
        buf.set_span(menu_centered.x, y, &Span::styled(*label, lstyle), label_len);

        // Key shortcut flush with the right edge of the menu column. The
        // "connected" badge renders green instead of the shortcut gray.
        let kstyle = if *key == CONNECTED_BADGE {
            let green = Style::default().fg(theme.accent_success);
            if is_selected {
                green.bg(theme.bg_highlight)
            } else {
                green
            }
        } else if is_selected {
            key_selected_style
        } else {
            key_style
        };
        buf.set_span(
            menu_centered.x + menu_centered.width - key_width,
            y,
            &Span::styled(*key, kstyle),
            key_width,
        );

        // [x] dismiss affordance restyling (for the import row)
        if let Some(x_offset) = key.rfind("[x]") {
            let key_x_start = menu_centered.x + menu_centered.width - key_width;
            let dismiss_start = key_x_start + x_offset as u16;
            let dismiss_end = dismiss_start + 3;
            let mouse_on_dismiss = mouse_pos
                .is_some_and(|(mx, my)| my == y && mx >= dismiss_start && mx < dismiss_end);
            let dismiss_color = if mouse_on_dismiss {
                theme.text_primary
            } else {
                theme.gray_bright
            };
            let dismiss_style = if is_selected {
                Style::default()
                    .fg(dismiss_color)
                    .bg(theme.bg_highlight)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(dismiss_color)
                    .add_modifier(Modifier::BOLD)
            };
            for (offset, ch) in "[x]".chars().enumerate() {
                let col = dismiss_start + offset as u16;
                if let Some(cell) = buf.cell_mut((col, y)) {
                    cell.set_char(ch);
                    cell.set_style(dismiss_style);
                }
            }
        }
    }

    // Scrollbar just outside the menu column (clamped to the area) when the
    // viewport is clipped. Same style as the pickers.
    if total > visible && visible > 0 {
        let sb_x =
            (menu_centered.x + menu_centered.width).min(area.x + area.width.saturating_sub(1));
        crate::render::scrollbar::render_scrollbar_styled(
            buf,
            Some(Rect::new(sb_x, menu_centered.y, 1, visible as u16)),
            total as u16,
            visible as u16,
            offset as u16,
            Style::default().bg(theme.bg_base),
            Style::default().fg(theme.gray_dim).bg(theme.bg_base),
        );
    }

    (rects, offset)
}
