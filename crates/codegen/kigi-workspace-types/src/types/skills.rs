//! Discovery shapes for skills surfaced by `OpsChunk::Skills` and
//! `WorkspaceEvent::SkillsChanged`.
//!
//! NOTE: this `source`-keyed `SkillInfo` is **not** the wire shape of
//! the `workspace.discover_skills` RPC -- that is
//! [`crate::rpc::skills::SkillInfo`] (`scope`-keyed).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    /// Stable identifier (also the slash-command name).
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub path: String,
    /// Source bucket: `"global"`, `"workspace"`, `"server"`, `"bundled"`, etc.
    #[serde(default)]
    pub source: String,
}
