//! Per-tool capabilities and notification schemas.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    /// `None` means the tool never emits partial-result progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<StreamingSpec>,

    /// Tool honours `hook { Cancel }`.
    #[serde(default)]
    pub supports_cancel: bool,

    /// `None` is unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,

    /// Mirrors `Tool::is_read_only`; used by doom-loop detection.
    #[serde(default)]
    pub is_read_only: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookKind>,

    /// Opaque per-tool behaviour version. Bytewise-compared (NOT semver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior_version: Option<String>,

    /// Per-tool override for the per-frame size cap. Service clamps to the
    /// 16 MiB hard ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_frame_bytes: Option<u32>,

    /// Per-call timeout override (defaults to 60_000ms when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// Absence is treated as [`ToolScope::Read`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_scope: Option<ToolScope>,
}

/// How a tool streams partial results. The spec is stamped onto a
/// self-describing progress envelope at the source, so downstream layers
/// dispatch on the envelope rather than on the tool's identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingSpec {
    /// Stable snake_case discriminator the tool stamps on its
    /// `ToolProgress::Custom.subkind` (e.g. `"bash_output_chunk"`).
    pub subkind: String,

    /// Per-frame `delta` byte cap (UTF-8-safe). Unset falls back to the
    /// runtime's 16 KiB default. Independent of
    /// [`ToolCapabilities::max_frame_bytes`], which caps whole frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delta_bytes: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    OnSessionOpen,
    OnSessionClose,
    OnToolCallStart,
    OnToolCallResult,
    OnCancel,
    OnNotification,
}

/// Multi-agent write-coordination scope.
///
/// Tools that mutate external state must declare `Write` so the computer hub
/// routes them to the leader agent only. Absence is treated as `Read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolScope {
    Read,
    Write,
}

/// Keys are the notification `kind` strings the computer hub validates
/// against.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationSchemas {
    /// Notifications the tool emits to subscribers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub outbound: HashMap<String, serde_json::Value>,

    /// Notifications the harness sends to the tool.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub inbound: HashMap<String, serde_json::Value>,
}
