//! Hook events delivered from the harness to tools.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HookEvent {
    /// The `tool_call_id` this cancels travels in the enclosing `hook` frame.
    Cancel,
    Pause,
    Resume,
    /// Broadcast to every tool server bound to the session.
    SessionEnded,
    /// Forward-compatible escape hatch.
    Custom {
        kind: String,
        payload: serde_json::Value,
    },
}
