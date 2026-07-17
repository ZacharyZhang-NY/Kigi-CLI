//! Location of the local subagent-content cache (`~/.kigi/bundled/`).
//!
//! Formerly this module managed a synced bundle of personas/roles/agents/
//! skills fetched from the xAI cli-chat-proxy (`GET /v1/subagents/bundle`).
//! That backend is gone; the directory remains a passive, locally-populated
//! content root that role/persona discovery scans (see
//! `config::resolve_*` discovery in `config/mod.rs`).

use std::path::PathBuf;

const BUNDLED_DIR_NAME: &str = "bundled";

/// `~/.kigi/bundled/` — the on-disk root for bundled subagent content.
pub fn bundled_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kigi")
        .join(BUNDLED_DIR_NAME)
}
