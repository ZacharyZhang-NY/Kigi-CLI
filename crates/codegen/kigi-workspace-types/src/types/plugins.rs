//! Discovery shapes for plugins and hooks surfaced by `OpsChunk::Plugins`,
//! `OpsChunk::Plugin`, `WorkspaceEvent::PluginsChanged`, and
//! `WorkspaceEvent::HooksChanged`.
//!
//! TODO(workspace): align with the canonical types in
//! `kigi-hooks-plugins-types` and `kigi-plugin-marketplace`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub path: String,
    /// Source: `"global"`, `"workspace"`, `"marketplace"`, ...
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Hook event the script attaches to (e.g. `"PreToolUse"`).
    ///
    /// TODO(workspace): the event field will become a typed enum once
    /// aligned with `kigi_hooks_plugins_types::HookEvent` -- right
    /// now it's a free-form string for placeholder convenience, which
    /// allows typos through.
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub plugin_id: Option<String>,
    #[serde(default)]
    pub enabled: bool,
}
