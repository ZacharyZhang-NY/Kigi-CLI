//! Clipboard, paste, primary-selection, and inline-media PTY coverage.
//!
//! This family is isolated from ordinary PTY scheduling and serialized by
//! Bazel because its platform cases touch host-global clipboard state.

// Shared support intentionally serves all PTY family crates.
#[allow(dead_code, unused_imports)]
#[path = "pty_e2e/common.rs"]
mod common;

#[path = "pty_e2e/bracketed_ime_paste_skips_clipboard_image_linux.rs"]
mod bracketed_ime_paste_skips_clipboard_image_linux;
#[path = "pty_e2e/bracketed_ime_paste_skips_clipboard_image_macos.rs"]
mod bracketed_ime_paste_skips_clipboard_image_macos;
#[path = "pty_e2e/image_chip_preview_path_free_pty.rs"]
mod image_chip_preview_path_free_pty;
#[path = "pty_e2e/middle_click_pastes_primary_linux.rs"]
mod middle_click_pastes_primary_linux;
#[path = "pty_e2e/paste_bracketed_chip_text_sends_full_payload.rs"]
mod paste_bracketed_chip_text_sends_full_payload;
#[path = "pty_e2e/paste_bracketed_inline_text_echoes_and_sends_intact.rs"]
mod paste_bracketed_inline_text_echoes_and_sends_intact;
#[path = "pty_e2e/paste_bracketed_then_immediate_enter_sends_intact.rs"]
mod paste_bracketed_then_immediate_enter_sends_intact;
#[path = "pty_e2e/paste_ctrl_v_image_keeps_ui_responsive_macos.rs"]
mod paste_ctrl_v_image_keeps_ui_responsive_macos;
#[path = "pty_e2e/paste_ctrl_v_image_keeps_ui_responsive_windows.rs"]
mod paste_ctrl_v_image_keeps_ui_responsive_windows;
#[path = "pty_e2e/paste_ctrl_v_text_echoes_fast_macos.rs"]
mod paste_ctrl_v_text_echoes_fast_macos;
#[path = "pty_e2e/paste_ctrl_v_text_echoes_fast_windows.rs"]
mod paste_ctrl_v_text_echoes_fast_windows;
