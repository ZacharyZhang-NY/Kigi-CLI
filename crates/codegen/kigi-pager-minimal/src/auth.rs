//! Minimal-mode sign-in rendering for the live region.
//!
//! Minimal has no welcome screen, so before any agent session exists the live
//! region itself shows the sign-in flow.
//! [`draw_live`](super::live::draw_live) computes a [`MinimalAuthHint`] from the
//! app's [`AuthState`] and renders it via [`render_auth`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use kigi_tui::app::app_view::AuthState;
use kigi_tui::theme::Theme;

/// What the no-agent live region shows. Computed from [`AuthState`] before the
/// draw closure so the closure can own it.
pub(super) enum MinimalAuthHint {
    /// Covers both the device flow and the external command flow, where the
    /// provider opens its own browser and `url` may be `None`.
    SigningIn {
        url: Option<String>,
        code: Option<String>,
    },
    Failed(String),
    /// Authenticated; the session is being created (brief transient).
    Starting,
}

pub(super) fn minimal_auth_hint(auth: &AuthState) -> MinimalAuthHint {
    match auth {
        AuthState::Authenticating { auth_url, .. } => MinimalAuthHint::SigningIn {
            url: auth_url.clone(),
            code: auth_url
                .as_deref()
                .and_then(device_user_code)
                .map(str::to_owned),
        },
        AuthState::Pending { error: Some(err) } => MinimalAuthHint::Failed(err.clone()),
        // Login is starting (auto-triggered at startup); the URL arrives via
        // AuthUrlReady, which flips us to `Authenticating`.
        AuthState::Pending { error: None } => MinimalAuthHint::SigningIn {
            url: None,
            code: None,
        },
        AuthState::Done => MinimalAuthHint::Starting,
    }
}

/// Mirrors `views::welcome::extract_user_code`, kept local so minimal does not
/// depend on welcome-screen internals.
fn device_user_code(url: &str) -> Option<&str> {
    let code = url
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|kv| kv.strip_prefix("user_code="))?;
    (!code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(code)
}

/// Returns the next free row; `y` unchanged when the line did not fit.
fn put_line(buf: &mut Buffer, area: Rect, y: u16, bottom: u16, line: Line<'_>) -> u16 {
    if y < bottom {
        buf.set_line(area.x, y, &line, area.width);
        y + 1
    } else {
        y
    }
}

/// Writes `url` character-by-character across as many rows as it needs, so no
/// wrap-inserted spaces land inside it and the terminal's native selection
/// copies it verbatim — minimal has no mouse capture, so copy is the terminal's
/// job. Returns the next free row.
fn render_url(
    buf: &mut Buffer,
    area: Rect,
    start_y: u16,
    bottom: u16,
    url: &str,
    style: Style,
) -> u16 {
    let width = area.width.max(1);
    // Snapshot the bounds as values so the `&Rect` borrow of `buf` ends before
    // the mutable cell writes below.
    let (max_x, max_y) = {
        let a = buf.area();
        (a.right(), a.bottom())
    };
    let mut col = 0u16;
    let mut y = start_y;
    for ch in url.chars() {
        // Skip control chars to prevent terminal escape injection.
        if ch.is_control() {
            continue;
        }
        if col >= width {
            col = 0;
            y = y.saturating_add(1);
        }
        if y >= bottom {
            return bottom;
        }
        let x = area.x + col;
        if x < max_x && y < max_y {
            buf[(x, y)].set_char(ch).set_style(style);
        }
        col += 1;
    }
    y.saturating_add(1)
}

/// Top-aligned in `area`; clips to its height.
pub(super) fn render_auth(buf: &mut Buffer, area: Rect, theme: &Theme, hint: &MinimalAuthHint) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bottom = area.y + area.height;
    let mut y = area.y;
    let gray = theme.muted().bg(Color::Reset);
    let bold = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD)
        .bg(Color::Reset);

    match hint {
        MinimalAuthHint::SigningIn { url, code } => {
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("Sign in to Kimi", bold)),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            match url {
                Some(url) => {
                    y = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled(
                            "Open this URL in your browser to approve:",
                            gray,
                        )),
                    );
                    y = render_url(
                        buf,
                        area,
                        y,
                        bottom,
                        url,
                        Style::default().fg(theme.accent_user).bg(Color::Reset),
                    );
                    if let Some(code) = code {
                        y = put_line(buf, area, y, bottom, Line::default());
                        y = put_line(
                            buf,
                            area,
                            y,
                            bottom,
                            Line::from(vec![
                                Span::styled("Code: ", gray),
                                Span::styled(code.clone(), bold),
                            ]),
                        );
                    }
                    y = put_line(buf, area, y, bottom, Line::default());
                    let _ = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled("Waiting for approval\u{2026}", gray)),
                    );
                }
                None => {
                    let _ = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled(
                            "Opening your browser to sign in\u{2026}",
                            gray,
                        )),
                    );
                }
            }
        }
        MinimalAuthHint::Failed(err) => {
            let warn = Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD)
                .bg(Color::Reset);
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("Sign-in failed", warn)),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(err.clone(), gray)),
            );
        }
        MinimalAuthHint::Starting => {
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(
                    "Signing in\u{2026} starting your session.",
                    gray,
                )),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_user_code_parses_verification_url() {
        // The live Kimi device flow returns this exact URL shape.
        assert_eq!(
            device_user_code("https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH"),
            Some("ABCD-EFGH")
        );
        assert_eq!(
            device_user_code("https://www.kimi.com/code/authorize_device"),
            None
        );
        assert_eq!(device_user_code("https://x/device?other=1"), None);
    }

    #[test]
    fn auth_hint_maps_auth_state() {
        use kigi_tui::app::app_view::AuthMode;

        let st = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: Some("https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH".into()),
            mode: AuthMode::Device,
        };
        match minimal_auth_hint(&st) {
            MinimalAuthHint::SigningIn { url, code } => {
                assert_eq!(
                    url.as_deref(),
                    Some("https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH")
                );
                assert_eq!(code.as_deref(), Some("ABCD-EFGH"));
            }
            _ => panic!("expected SigningIn"),
        }

        // The external command flow carries no `user_code` in its URL.
        let st = AuthState::Authenticating {
            request_seq: 2,
            handle: None,
            auth_url: Some("https://provider.example/login".into()),
            mode: AuthMode::Command,
        };
        match minimal_auth_hint(&st) {
            MinimalAuthHint::SigningIn { url, code } => {
                assert_eq!(url.as_deref(), Some("https://provider.example/login"));
                assert!(code.is_none());
            }
            _ => panic!("expected SigningIn"),
        }

        assert!(matches!(
            minimal_auth_hint(&AuthState::Done),
            MinimalAuthHint::Starting
        ));
        assert!(matches!(
            minimal_auth_hint(&AuthState::Pending {
                error: Some("nope".into())
            }),
            MinimalAuthHint::Failed(_)
        ));
    }

    #[test]
    fn render_auth_shows_url_and_code() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        let hint = MinimalAuthHint::SigningIn {
            url: Some("https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH".into()),
            code: Some("ABCD-EFGH".into()),
        };
        render_auth(&mut buf, area, &theme, &hint);
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(c) = buf.cell((x, y)) {
                    text.push_str(c.symbol());
                }
            }
        }
        assert!(text.contains("Sign in to Kimi"), "header: {text:?}");
        assert!(
            text.contains("www.kimi.com/code/authorize_device"),
            "url: {text:?}"
        );
        assert!(text.contains("ABCD-EFGH"), "device code: {text:?}");
        assert!(
            text.contains("Waiting for approval"),
            "waiting line: {text:?}"
        );
    }
}
