//! Cross-platform child-process lifecycle helpers for `tokio::process::Command`.
//!
//! The implementations live in the lightweight [`kigi_tty_utils`] crate so that
//! every crate in the workspace can use them without pulling in the heavyweight
//! `kigi-tools` dependency. This module re-exports the public API for callers
//! that reach it through `kigi-tools`.

pub use kigi_tty_utils::{
    ProcessGroup, ProcessScope, detach_command, global_process_scope, new_process_group,
};

/// Reap an already-killed search child, bounded by
/// [`kigi_tty_utils::KILL_REAP_TIMEOUT`]; on `None` warn and leave the corpse
/// to tokio's orphan reaper.
pub async fn reap_killed_search_child(
    child: &mut tokio::process::Child,
) -> Option<std::process::ExitStatus> {
    let status = kigi_tty_utils::reap_killed_bounded(child, kigi_tty_utils::KILL_REAP_TIMEOUT).await;
    if status.is_none() {
        tracing::warn!(
            reap_timeout_secs = kigi_tty_utils::KILL_REAP_TIMEOUT.as_secs(),
            "killed search child not reaped (bound expired — likely uninterruptible kernel I/O — or wait failed); abandoning to the orphan reaper"
        );
    }
    status
}
