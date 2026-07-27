//! `agent_swarm` tool — one prompt template over many items, run as a fleet.
pub mod plan;
pub mod run;
pub mod schedule;
pub mod tool;

pub use tool::{AGENT_SWARM_TOOL_NAME, AgentSwarmTool};
