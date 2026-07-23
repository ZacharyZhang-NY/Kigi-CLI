//! Legacy `--chat` gateway gate.
//!
//! The kimi.com chat-product model picker (`/rest/modes`, `ChatModesManager`)
//! has no Kimi counterpart, so only this process-mode gate remains: it keeps
//! the `--chat` frontend path a compile-time-off no-op across crates without a
//! cross-crate churn to delete every reference.

/// Process-wide flag set by the pager when started with `--chat`.
pub const KIGI_CHAT_MODE_ENV: &str = "KIGI_CHAT_MODE";

/// Whether the process is a gateway light-frontend (`--chat`) agent. Always
/// `false`: the kigi chat-modes backend has no Kimi counterpart.
pub fn process_chat_mode_enabled() -> bool {
    false
}
