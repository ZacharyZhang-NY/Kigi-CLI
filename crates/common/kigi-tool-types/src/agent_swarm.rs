//! Input/output types for the `agent_swarm` tool — one prompt template
//! expanded over a list of items into a fleet of subagents.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The literal a `prompt_template` must contain; each expansion substitutes
/// one `items` entry for it.
pub const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";

/// Upper bound on members in one call, counting resumes.
pub const MAX_AGENT_SWARM_MEMBERS: usize = 128;

/// Input for the `agent_swarm` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSwarmToolInput {
    #[schemars(description = "Short description of what the whole swarm is doing (3-7 words).")]
    pub description: String,

    /// Subagent type every item-spawned member runs as.
    #[schemars(
        description = "Name of the subagent type every member runs as. Built-in types: \"general-purpose\", \"explore\", \"plan\"."
    )]
    #[serde(default = "default_subagent_type")]
    pub subagent_type: String,

    /// Prompt shared by every member; must contain `{{item}}`.
    #[schemars(
        description = "Prompt shared by every member. Must contain the literal {{item}}, which is replaced by each entry of `items`. Required whenever `items` is given."
    )]
    #[serde(default)]
    pub prompt_template: Option<String>,

    /// The work units. Each expands `prompt_template` into one member.
    #[schemars(
        description = "One entry per member: each is substituted into `prompt_template`. Give every member a distinct scope so members never edit the same file. At least 2 entries unless `resume_agent_ids` is used."
    )]
    #[serde(default)]
    pub items: Vec<String>,

    /// Continue named subagents from a previous swarm: id → follow-up prompt.
    #[schemars(
        description = "Continue previously spawned subagents: a map of agent_id (from an earlier agent_swarm result) to the follow-up prompt for that member."
    )]
    #[serde(default)]
    pub resume_agent_ids: std::collections::BTreeMap<String, String>,

    /// Model slug every member runs on; omitted inherits the caller's.
    #[schemars(
        description = "Model every member runs on. Omit to inherit the caller's current model."
    )]
    #[serde(default)]
    pub model: Option<String>,
}

fn default_subagent_type() -> String {
    "general-purpose".to_string()
}

/// How a member's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwarmMemberOutcome {
    Completed,
    Failed,
    Aborted,
    /// Still running: it outlived the foreground await budget and the subagent
    /// coordinator detached it. Distinct from `Failed` because the member is
    /// alive and still writing — relaunching its item would put a second agent
    /// on the same files, and resuming it is refused while it runs.
    Backgrounded,
}

impl SwarmMemberOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Backgrounded => "backgrounded",
        }
    }

    /// Whether re-running this member's work is safe to suggest.
    fn is_resumable(self) -> bool {
        matches!(self, Self::Failed | Self::Aborted)
    }
}

/// One member's contribution to the aggregate result.
#[derive(Debug, Clone)]
pub struct SwarmMemberResult {
    /// The `items` entry (or the resumed agent id) this member was given —
    /// what the caller needs to retry exactly the members that did not finish.
    pub item: String,
    /// Present once the member started; absent means it never launched.
    pub agent_id: Option<String>,
    pub resumed: bool,
    pub outcome: SwarmMemberOutcome,
    pub summary: String,
}

impl SwarmMemberResult {
    /// Whether the member ever reached the backend. A member that never
    /// started has nothing to resume.
    pub fn started(&self) -> bool {
        self.agent_id.is_some()
    }

    /// Whether the caller may be told to continue this member. A member that
    /// is still running must not be offered: the coordinator refuses to resume
    /// a live subagent, and relaunching its item duplicates its writes.
    pub fn is_resumable(&self) -> bool {
        self.started() && self.outcome.is_resumable()
    }
}
