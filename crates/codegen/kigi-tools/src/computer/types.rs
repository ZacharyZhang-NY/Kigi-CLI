use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::notification::types::ToolNotificationHandle;

#[derive(thiserror::Error, Debug, Clone)]
pub enum ComputerError {
    #[error("IO Error: {0}")]
    IOError(String, Option<std::io::ErrorKind>),
    #[error("UnQuoted command")]
    CommandNotQuoted,
}

impl ComputerError {
    pub fn io(msg: impl Into<String>) -> Self {
        Self::IOError(msg.into(), None)
    }

    pub fn io_with_kind(msg: impl Into<String>, kind: std::io::ErrorKind) -> Self {
        Self::IOError(msg.into(), Some(kind))
    }

    pub fn io_error_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::IOError(_, kind) => *kind,
            _ => None,
        }
    }
}

impl From<std::io::Error> for ComputerError {
    fn from(err: std::io::Error) -> Self {
        Self::IOError(err.to_string(), Some(err.kind()))
    }
}

#[async_trait::async_trait]
pub trait AsyncFileSystem: Send + Sync {
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, ComputerError>;

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), ComputerError>;

    async fn delete_file(&self, path: &Path) -> Result<(), ComputerError>;
}

pub struct TerminalRunRequest {
    pub command: String,
    pub working_directory: PathBuf,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub output_byte_limit: usize,
    /// Output is written here incrementally, so the full text stays retrievable
    /// after the in-memory buffer is truncated or the agent has moved on.
    pub output_file: PathBuf,

    /// Receives `BashOutputChunk` notifications every ~100ms during execution.
    /// Callers that don't want streaming pass `ToolNotificationHandle::noop()`,
    /// which drops them — hence no `Option` wrapper.
    pub notification_handle: ToolNotificationHandle,

    /// Flows from `ToolContext::tool_call_id()` through the actor to
    /// `BashOutputChunk.base.tool_call_id`.
    pub tool_call_id: String,

    /// Original user command before isolation wrapping. Surfaces through
    /// `TaskSnapshot.display_command` so model-facing `get_task_output` shows
    /// the user's command instead of the `unshare`/mount wrapper.
    pub display_command: Option<String>,

    /// Auto-background on timeout instead of killing (default `false`).
    pub auto_background_on_timeout: bool,

    /// When [`Self::auto_background_on_timeout`] is true, maximum time the
    /// command may block the turn before being moved to the background (process
    /// keeps running). Independent of [`Self::timeout`].
    ///
    /// - `None` → use the terminal backend default (typically 15s).
    /// - `Some(Duration::MAX)` → no short budget; auto-bg only when `timeout` elapses.
    /// - `Some(d)` → auto-bg after `d` if still running.
    pub foreground_block_budget: Option<Duration>,

    pub kind: TaskKind,

    /// Scopes kill operations so `kill_all_background_tasks_by_owner` only
    /// targets the requesting session's processes — not the parent's or
    /// a sibling's.
    pub owner_session_id: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Bash,
    /// Monitor tool — streams stdout events with rate limiting.
    Monitor,
}

#[derive(Clone)]
pub struct TerminalRunResult {
    pub combined_output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub signal: Option<String>,
    pub timed_out: bool,
    /// Holds the full output; read it back with the read_file tool when
    /// `combined_output` is truncated.
    pub output_file: PathBuf,
    /// Byte count before truncation. When truncated, `combined_output` holds
    /// the first and last portions up to `output_byte_limit` chars.
    pub total_bytes: usize,
    /// PID of the spawned shell process. Lets a foreground command that
    /// auto-backgrounds on timeout report a real PID rather than a placeholder.
    /// `None` for backends without a local PID (e.g. ACP/remote terminals) or
    /// when the process exited before `child.id()` could be queried.
    pub pid: Option<u32>,
}

/// Returned by `TerminalBackend::run_background` — the `task_id` is the key for
/// subsequent `get_task`, `kill_task`, `wait_for_completion` calls.
pub struct BackgroundHandle {
    pub task_id: String,
    pub output_file: PathBuf,
    /// `None` for backends without a local PID (e.g. ACP/remote gateways) or
    /// when the process exited before the PID could be captured.
    pub pid: Option<u32>,
}

#[derive(
    Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct TaskSnapshot {
    pub task_id: String,
    /// The command as executed, which may be isolation-wrapped.
    pub command: String,
    /// The original user command before isolation wrapping. Model/user-facing
    /// output should prefer this over `command` to avoid exposing internal
    /// isolation mechanics (unshare/mount wrapper).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_command: Option<String>,
    pub cwd: String,
    pub start_time: std::time::SystemTime,
    pub end_time: Option<std::time::SystemTime>,
    pub output: String,
    pub output_file: PathBuf,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub completed: bool,
    #[serde(default)]
    pub kind: TaskKind,
    /// Set when a block-waiter (`block=true`) consumed this task's result, so
    /// the notification bridge skips auto-wake synthetic prompts — the blocking
    /// caller already received the result directly.
    #[serde(default)]
    pub block_waited: bool,
    /// Set when the task was killed via the `kill_command_or_subagent` tool,
    /// suppressing auto-wake synthetic prompts because the model already
    /// received the kill result via `KillTaskResult`. Also set during
    /// `kill_all_background_tasks` teardown (e.g. subagent cleanup), where
    /// auto-wake suppression is irrelevant since the session is shutting down.
    #[serde(default)]
    pub explicitly_killed: bool,

    /// Scopes kill operations so subagent teardown only kills the subagent's
    /// own tasks, not the parent's or a sibling's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
}

impl TaskSnapshot {
    /// For a task still running, this is the time since start.
    pub fn duration_secs(&self) -> f64 {
        let end = self.end_time.unwrap_or_else(std::time::SystemTime::now);
        end.duration_since(self.start_time)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Deliberately kind-agnostic: the runtime turn-end TodoGate counts both
    /// bash and monitor tasks as backing work.
    pub fn is_outstanding(&self) -> bool {
        !self.completed
    }
}

/// Result of killing a terminal task.
///
/// Serialized over the wire in the `kigi/task/kill` ext response
/// (`kigi-shell::extensions::task::KillTaskResponse`) and deserialized
/// by clients (kigi-tui), so it derives both serde directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillOutcome {
    Killed,
    AlreadyExited,
    NotFound,
}

/// The single abstraction over terminal execution backends.
///
/// Implemented by:
/// - `LocalTerminalBackend` (in kigi-tools, spawns processes)
/// - `AcpTerminalBackend` (in kigi-shell, calls ACP protocol)
#[async_trait::async_trait]
pub trait TerminalBackend: Send + Sync {
    /// Blocks until completion or timeout.
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, ComputerError>;

    /// Returns immediately while the process keeps running; manage it via
    /// `get_task`/`kill_task`/`wait_for_completion`.
    async fn run_background(
        &self,
        request: TerminalRunRequest,
    ) -> Result<BackgroundHandle, ComputerError>;

    async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot>;

    async fn kill_task(&self, task_id: &str) -> KillOutcome;

    async fn kill_foreground_commands(&self) {}

    /// Owner-scoped variant for a shared terminal backend, so a subagent's
    /// cancel doesn't kill the parent's foreground commands.
    async fn kill_foreground_commands_by_owner(&self, _owner_session_id: &str) {}

    /// Used during subagent teardown to clean up orphaned processes.
    async fn kill_all_background_tasks(&self) {}

    /// Owner-scoped variant for a shared terminal backend, so subagent teardown
    /// kills only the subagent's own tasks — not the parent's.
    async fn kill_all_background_tasks_by_owner(&self, _owner_session_id: &str) {}

    /// Fire-and-forget prewarm of the persistent login shell; default no-op for
    /// backends without one (ACP/remote, non-persistent).
    async fn warm_persistent_shell(&self, _cwd: &std::path::Path) {}

    /// Swaps the dead child session's notification handle for the parent's live
    /// handle on every task owned by `old_owner_session_id`, so events from
    /// surviving processes keep routing correctly. Also re-spawns monitor
    /// pipelines on the caller's runtime so monitor events reach the parent.
    ///
    /// `backend_weak` is a [`Weak`](std::sync::Weak) to *this* backend (anchored
    /// by the parent session's `Arc`); it drives re-spawned monitor pipelines
    /// without keeping the backend alive. See `run_monitor_pipeline`.
    async fn reparent_notifications(
        &self,
        _old_owner_session_id: &str,
        _new_owner_session_id: &str,
        _new_handle: crate::notification::types::ToolNotificationHandle,
        _backend_weak: std::sync::Weak<dyn TerminalBackend>,
    ) {
    }

    /// Unblocks the foreground waiter for `tool_call_id` while the process keeps
    /// running. Returns `true` if a matching foreground process was found.
    async fn background_foreground_command(&self, _tool_call_id: &str) -> bool {
        false
    }

    async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<Duration>,
    ) -> Option<TaskSnapshot>;

    /// Includes completed tasks; context compaction uses this to put task state
    /// into summaries.
    async fn list_tasks(&self) -> Vec<TaskSnapshot>;

    /// `None` when persistent shell state is off or unsupported by the backend
    /// (e.g. ACP/remote).
    async fn get_shell_cwd(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub struct Computer {
    pub terminal: Arc<dyn TerminalBackend>,
    pub file_system: Arc<dyn AsyncFileSystem>,
}

impl Computer {
    pub fn new(terminal: Arc<dyn TerminalBackend>, file_system: Arc<dyn AsyncFileSystem>) -> Self {
        Self {
            terminal,
            file_system,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_kind_preserved_through_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let ce = ComputerError::from(io_err);
        assert_eq!(ce.io_error_kind(), Some(std::io::ErrorKind::NotFound));
    }

    #[test]
    fn io_error_kind_is_a_directory() {
        let io_err = std::io::Error::new(std::io::ErrorKind::IsADirectory, "it's a dir");
        let ce = ComputerError::from(io_err);
        assert_eq!(ce.io_error_kind(), Some(std::io::ErrorKind::IsADirectory));
    }

    #[test]
    fn io_error_kind_none_for_string_constructor() {
        let ce = ComputerError::io("something broke");
        assert_eq!(ce.io_error_kind(), None);
    }

    #[test]
    fn io_error_kind_none_for_non_io_variant() {
        let ce = ComputerError::CommandNotQuoted;
        assert_eq!(ce.io_error_kind(), None);
    }

    #[test]
    fn io_with_kind_preserves_not_found() {
        let ce = ComputerError::io_with_kind("Resource not found", std::io::ErrorKind::NotFound);
        assert_eq!(ce.io_error_kind(), Some(std::io::ErrorKind::NotFound));
    }

    #[test]
    fn io_with_kind_preserves_permission_denied() {
        let ce = ComputerError::io_with_kind("access denied", std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            ce.io_error_kind(),
            Some(std::io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn io_with_kind_matches_local_fs_dispatch_for_not_found() {
        // What LocalFs produces for a missing file.
        let local_err = ComputerError::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ));
        // What AcpFsAdapter produces for RESOURCE_NOT_FOUND.
        let acp_err =
            ComputerError::io_with_kind("Resource not found", std::io::ErrorKind::NotFound);

        // Both must produce the same io_error_kind so read_file dispatches
        // to FileNotFound("Error: {path} does not exist.") in both cases.
        assert_eq!(local_err.io_error_kind(), acp_err.io_error_kind());
    }
}
