//! Layer 2b: Frame timing via VTE parser.
//!
//! Detects frame boundaries from synchronized-update markers
//! (`CSI ? 2026 h/l`) emitted by crossterm's `BeginSynchronizedUpdate` /
//! `EndSynchronizedUpdate`, and records wall-clock timing for each frame.

use std::time::{Duration, Instant};

// `vte` is re-exported by `alacritty_terminal` (pulled in via ptyctl), so no
// direct dependency on it is needed.
use alacritty_terminal::vte;

#[derive(Debug, Clone)]
pub struct FrameTiming {
    /// Wall-clock duration from BeginSynchronizedUpdate to EndSynchronizedUpdate.
    pub duration: Duration,
    /// Number of printable characters emitted within this frame.
    pub chars: usize,
}

pub struct FrameTimingParser {
    vte_parser: vte::Parser,
    handler: FrameTimingHandler,
}

impl FrameTimingParser {
    pub fn new() -> Self {
        Self {
            vte_parser: vte::Parser::new(),
            handler: FrameTimingHandler::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.vte_parser.advance(&mut self.handler, bytes);
    }

    pub fn timings(&self) -> &[FrameTiming] {
        &self.handler.timings
    }

    /// Counts only frames whose end marker has been seen; a frame still open
    /// is not included.
    pub fn frame_count(&self) -> u64 {
        self.handler.timings.len() as u64
    }

    pub fn reset(&mut self) {
        self.handler.timings.clear();
        self.handler.frame_start = None;
        self.handler.current_frame_chars = 0;
    }
}

impl Default for FrameTimingParser {
    fn default() -> Self {
        Self::new()
    }
}

struct FrameTimingHandler {
    frame_start: Option<Instant>,
    timings: Vec<FrameTiming>,
    current_frame_chars: usize,
}

impl FrameTimingHandler {
    fn new() -> Self {
        Self {
            frame_start: None,
            timings: Vec::new(),
            current_frame_chars: 0,
        }
    }
}

impl vte::Perform for FrameTimingHandler {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // Synchronized-update markers are private modes, so anything without
        // the `?` intermediate cannot be one.
        if intermediates != b"?" {
            return;
        }
        let param_val: u16 = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0);

        if param_val == 2026 {
            match action {
                'h' => {
                    self.frame_start = Some(Instant::now());
                    self.current_frame_chars = 0;
                }
                'l' => {
                    if let Some(start) = self.frame_start.take() {
                        self.timings.push(FrameTiming {
                            duration: start.elapsed(),
                            chars: self.current_frame_chars,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn print(&mut self, _c: char) {
        self.current_frame_chars += 1;
    }
}
