//! Turn-loop seam for the stationarity detector.

use super::*;
use crate::session::stationarity::{
    IdenticalToolCallRun, NUDGE_AFTER_IDENTICAL_TOOL_CALLS, command_is_true, hash_batch,
};

/// Sent once, naming the blocking wait a poller should use.
const STATIONARITY_NUDGE: &str = "\
You have called the same tool with the same arguments repeatedly and are in a \
polling loop. Stop repeating that call. If you are waiting on a background \
task, block on it with the wait-tasks tool instead of re-checking its output; \
if you are waiting on anything else, sleep once and check once. If you cannot \
make progress, stop and tell the user what you are waiting for. This turn will \
be halted automatically if the identical call keeps repeating.";

impl SessionActor {
    /// Enforces the ceilings; `Some` halts the turn. Silent but logged.
    pub(crate) async fn observe_tool_call_stationarity(
        self: &Arc<Self>,
        run: &mut IdenticalToolCallRun,
        tool_calls: &[kigi_sampling_types::conversation::ToolCall],
        loop_index: usize,
    ) -> Option<StationarityHalt> {
        let batch_hash = hash_batch(
            tool_calls
                .iter()
                .map(|tc| (tc.name.as_str(), tc.arguments.as_ref())),
        );
        let tool_name = tool_calls
            .first()
            .map(|tc| tc.name.clone())
            .unwrap_or_default();
        let is_true_noop = self.is_run_true_step(tool_calls).await;
        let run_len = run.observe(batch_hash, &tool_name, is_true_noop);

        if run_len == NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
            tracing::warn!(
                tool_name = %run.tool_name(),
                run_len,
                loop_index,
                "action stationarity: nudging a repeating tool call"
            );
            kigi_log::unified_log::warn(
                "shell.turn.action_stationarity_nudge",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "tool_name": run.tool_name(),
                    "run_len": run_len,
                    "loop_index": loop_index,
                })),
            );
            self.push_system_reminder(STATIONARITY_NUDGE);
        }

        if run_len < run.hard_stop_threshold() {
            return None;
        }

        tracing::warn!(
            tool_name = %run.tool_name(),
            run_len,
            loop_index,
            true_noop = run.is_true_noop_run(),
            "action stationarity: halting the turn"
        );
        kigi_log::unified_log::warn(
            "shell.turn.action_stationarity_stop",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "tool_name": run.tool_name(),
                "run_len": run_len,
                "loop_index": loop_index,
                "true_noop": run.is_true_noop_run(),
            })),
        );
        Some(StationarityHalt {
            tool_name: run.tool_name().to_string(),
            run_len,
            true_noop: run.is_true_noop_run(),
        })
    }

    /// Whether this batch is a single shell call that does nothing.
    ///
    /// Size-gated, not name-gated: the shell tool is renamed
    /// `run_terminal_command`, so a name gate would silently disable this.
    /// Parse failure fails open; multi-call batches use the ordinary ceiling.
    async fn is_run_true_step(
        &self,
        tool_calls: &[kigi_sampling_types::conversation::ToolCall],
    ) -> bool {
        /// Bound on `{"command":"true"}` plus sibling fields.
        const MAX_NOOP_ARGS_BYTES: usize = 512;

        let [tc] = tool_calls else {
            return false;
        };
        if tc.arguments.as_ref().len() > MAX_NOOP_ARGS_BYTES {
            return false;
        }
        let Ok(args) = serde_json::from_str::<serde_json::Value>(tc.arguments.as_ref()) else {
            return false;
        };
        let Ok(input) = self.tool_bridge_handle().try_parse(&tc.name, args).await else {
            return false;
        };
        matches!(
            input,
            kigi_tools::types::tool_io::ToolInput::Bash(ref b) if command_is_true(&b.command)
        )
    }
}

/// Why a turn was halted, carried to its outcome.
pub(crate) struct StationarityHalt {
    pub(crate) tool_name: String,
    pub(crate) run_len: u32,
    pub(crate) true_noop: bool,
}
