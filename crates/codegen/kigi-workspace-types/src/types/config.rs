//! Configuration shapes referenced from session lifecycle requests and
//! `OpsChunk::ProjectConfig` / `OpsChunk::Permissions`.
//!
//! TODO(workspace): align with the canonical project / permission /
//! agent-session config types in `kigi-config` and friends.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Filesystem isolation strategy for a forked session.
///
/// `Default` is [`IsolationMode::None`], which shares the parent's working
/// tree; that suits the root session, but subagent forks should opt into a
/// more restrictive mode rather than relying on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// No isolation: shares the parent's working tree.
    #[default]
    None,
    /// Run the subagent in a copy-on-write git worktree.
    Worktree,
    /// Run the subagent inside a sandbox/container.
    Sandbox,
}

/// Capability mode applied to a forked session.
///
/// `Default` is [`CapabilityMode::ReadWrite`], which suits the root session;
/// subagents should opt into a more restrictive mode rather than relying on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    /// Full read+write capability (default for the root session).
    #[default]
    ReadWrite,
    /// Read-only: tools that mutate state are unavailable.
    ReadOnly,
    /// No tools at all.
    None,
}

/// Per-tool-server configuration knob.
///
/// TODO(workspace): align with the actual MCP/tool-server config in
/// `kigi-tools` once the wire surface is firm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolServerConfig {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    /// Command override for dynamically launched servers.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Configuration applied when forking a session via
/// `SessionLifecycleRequest::Fork`.
///
/// `Default` yields `IsolationMode::None` + `CapabilityMode::ReadWrite`, which
/// suit the root session, not subagents. Fork a subagent by naming the fields
/// explicitly rather than relying on `..Default::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionConfig {
    pub agent_id: String,
    #[serde(default)]
    pub isolation: IsolationMode,
    #[serde(default)]
    pub capability_mode: CapabilityMode,
    #[serde(default)]
    pub tool_config: Vec<ToolServerConfig>,
    /// Maximum recursion depth for subagent nesting; 0 = no further nesting.
    #[serde(default)]
    pub max_depth: u32,
    /// Working directory override, relative to workspace root.
    #[serde(default)]
    pub cwd_override: Option<String>,
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
}

/// Project configuration returned by `OpsChunk::ProjectConfig`.
///
/// TODO(workspace): align with `kigi_config::ProjectConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Free-form key/value config (placeholder).
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Whether the project is trusted, allowing hooks/plugins to run.
    #[serde(default)]
    pub trusted: bool,
}

/// Permission policy returned by `OpsChunk::Permissions`.
///
/// TODO(workspace): align with the canonical permission policy type
/// (currently a free-form JSON shape).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    /// Tool patterns that are unconditionally allowed (no prompt).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tool patterns that are unconditionally denied.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tool patterns that always prompt for permission.
    #[serde(default)]
    pub ask: Vec<String>,
}
