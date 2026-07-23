use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod info;

pub use info::Info;

/// Snapshot of the user's terminal environment at feedback time.
///
/// Shared here (rather than in kigi-shell) because kigi-pager-render builds it
/// from its terminal probes and the shell attaches it to feedback records.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackTerminalInfo {
    /// e.g. "Ghostty", "iTerm2", "Unknown".
    pub brand: String,
    /// e.g. "tmux", "Zellij", "None detected".
    pub multiplexer: String,
    pub is_ssh: bool,
    pub is_byobu: bool,
    pub term_var: String,
    /// tmux server version if inside tmux, otherwise "n/a".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_version: Option<String>,
    /// Hyperlink (OSC 8) support level (e.g. "native", "hostile_parser").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink_osc8_support: Option<String>,
    /// Active clipboard legs, e.g. "native+osc52" or "native+tmux+osc52".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_route: Option<String>,
    /// Native clipboard tool: "pbcopy", "wl-copy", "xclip", "xsel", "arboard".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_native_tool: Option<String>,
    /// Display server: "wayland", "x11", "quartz", "win32", "unknown".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_server: Option<String>,
}

pub fn session_dir(info: &Info) -> PathBuf {
    kigi_tools::util::kigi_home::sessions_cwd_dir(&info.cwd).join(info.id.to_string())
}
