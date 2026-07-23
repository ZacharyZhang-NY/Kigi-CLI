//! Layer 2a: screen state tracking via `alacritty_terminal` (ptyctl).
//!
//! Parses raw PTY output through a headless terminal emulator and provides
//! queries for what the user would see on screen.

use ptyctl::styled::StyledLine;
use ptyctl::term::{ScreenOpts, ScreenOutput, SessionListener, Terminal};

pub struct ScreenTracker {
    terminal: Terminal,
    /// Receives terminal-generated replies (cursor-position reports, device
    /// attributes, color queries, …) the emulator emits while parsing input.
    /// Drained by [`ScreenTracker::drain_responses`] so the harness can forward
    /// them back to the child (real terminals answer these automatically).
    pty_write_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
}

impl ScreenTracker {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = SessionListener::new(tx);
        Self {
            terminal: Terminal::new(cols, rows, listener),
            pty_write_rx: rx,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    /// Replies queued while parsing fed input, concatenated in order.
    ///
    /// These MUST be written back to the PTY or programs that probe the
    /// terminal will hang or time out — most relevant here, the inline
    /// viewport's startup cursor-position query that minimal mode depends on
    /// (a timeout there downgrades `--minimal` to full-screen inline). A real
    /// terminal answers automatically; the harness forwards these in
    /// [`crate::PtyHarness::update`] when response forwarding is enabled.
    pub fn drain_responses(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(bytes) = self.pty_write_rx.try_recv() {
            out.extend_from_slice(&bytes);
        }
        out
    }

    /// Structured screen contents, with escape codes stripped.
    pub fn output(&self) -> ScreenOutput {
        self.terminal.screen_content(&ScreenOpts::default())
    }

    /// Full screen text, with escape codes stripped.
    pub fn contents(&self) -> String {
        self.output().lines.join("\n")
    }

    pub fn contains(&self, text: &str) -> bool {
        self.contents().contains(text)
    }

    /// Cursor position as `(row, col)`, 0-indexed to match the vt100
    /// convention the tests are written against.
    pub fn cursor_position(&self) -> (u16, u16) {
        let pos = self.terminal.cursor_position();
        // ptyctl cursor is 1-indexed; the harness API is 0-indexed.
        (
            (pos.row as u16).saturating_sub(1),
            (pos.col as u16).saturating_sub(1),
        )
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.terminal.resize(cols, rows);
    }

    pub fn styled(&self) -> Vec<StyledLine> {
        self.terminal.screen_styled(&ScreenOpts::default())
    }

    pub fn html(&self) -> String {
        self.terminal.screen_html(&ScreenOpts::default())
    }

    /// Escape hatch for queries this wrapper does not expose (scrollback
    /// details, terminal modes, …).
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Number of history lines that have scrolled *above* the visible screen.
    /// This is where minimal mode's committed conversation blocks land
    /// (printed via `insert_before`).
    pub fn scrollback_count(&self) -> usize {
        self.terminal.scrollback_count()
    }

    /// Scrollback history as text, oldest line first.
    pub fn scrollback_text(&self) -> String {
        let n = self.terminal.scrollback_count();
        self.terminal
            .scrollback_lines(n)
            .into_iter()
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Scrollback plus visible screen, oldest→newest. Minimal-mode committed
    /// content may be in either region depending on how much has accumulated,
    /// so assertions on committed output should use this.
    pub fn full_text(&self) -> String {
        let sb = self.scrollback_text();
        let screen = self.contents();
        if sb.is_empty() {
            screen
        } else {
            format!("{sb}\n{screen}")
        }
    }

    pub fn full_contains(&self, text: &str) -> bool {
        self.full_text().contains(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolled_off_lines_are_captured_by_scrollback_helpers() {
        let mut s = ScreenTracker::new(3, 20);
        for i in 1..=8 {
            s.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(
            !s.contains("line1"),
            "line1 should have scrolled off-screen"
        );
        assert!(s.scrollback_count() >= 5, "expected scrolled-off history");
        assert!(s.scrollback_text().contains("line1"));
        assert!(s.full_contains("line1"));
        assert!(s.full_contains("line8"));
    }

    /// Without a forwardable CPR reply the inline viewport's startup cursor
    /// query never completes and `--minimal` silently downgrades to
    /// full-screen inline.
    #[test]
    fn drain_responses_answers_cursor_position_query() {
        let mut s = ScreenTracker::new(24, 80);
        assert!(s.drain_responses().is_empty());

        s.feed(b"\x1b[6n");
        let reply = s.drain_responses();
        assert!(
            reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
            "expected a cursor-position report, got {:?}",
            String::from_utf8_lossy(&reply)
        );

        // Drained exactly once — no duplicate delivery on the next call.
        assert!(s.drain_responses().is_empty());
    }
}
