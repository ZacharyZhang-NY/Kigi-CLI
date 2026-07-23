use crate::types::requirements::{Expr, ToolRequirement};

use crate::types::tool::{ToolKind, ToolNamespace};

use super::interval::{interval_to_human, parse_interval};
use super::types::{ScheduledTask, SchedulerCommand, SchedulerHandle};

// Canonical /loop wording lives in the light API crate so other consumers can
// link it without the tools implementation crate; re-exported to keep paths stable.
pub use kigi_tools_api::slash_commands::{
    SCHEDULER_CREATE_TOOL_NAME, loop_schedule_instruction, loop_usage_message,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerCreateInput {
    #[schemars(description = "Interval between executions, e.g. \"5m\", \"2h\", \"1d\"")]
    pub interval: String,

    #[schemars(description = "The prompt text to execute on each scheduled fire")]
    pub prompt: String,

    #[serde(
        default = "default_true",
        deserialize_with = "crate::types::schema::deserialize_lenient_bool"
    )]
    #[schemars(
        description = "Whether the task repeats (true) or fires once (false). Default: true"
    )]
    pub recurring: bool,

    #[serde(
        default,
        deserialize_with = "crate::types::schema::deserialize_lenient_option_bool"
    )]
    #[schemars(description = "Whether the task persists across sessions. Default: false")]
    pub durable: Option<bool>,

    /// Default false: a "scheduled" task should not run on creation unless
    /// explicitly asked to.
    #[serde(
        default,
        deserialize_with = "crate::types::schema::deserialize_lenient_bool"
    )]
    #[schemars(
        description = "Whether to fire immediately on creation (true) or wait for the first interval (false). Default: false"
    )]
    pub fire_immediately: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerCreateOutput {
    pub id: String,
    pub human_schedule: String,
    pub recurring: bool,
}

impl kigi_tool_runtime::ToolOutput for SchedulerCreateOutput {}

#[derive(Debug, Default)]
pub struct SchedulerCreateTool;

impl crate::types::tool_metadata::ToolMetadata for SchedulerCreateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Kigi
    }

    fn description_template(&self) -> &str {
        r#"Create a scheduled task that runs a prompt on a recurring interval.

Set fire_immediately: true to also fire once on creation; by default the first run waits for the interval.

Usage notes:
- Interval format: "5m" (minutes), "2h" (hours), "1d" (days), "60s" (seconds, min 60)
- Maximum 50 scheduled tasks at once
- Recurring tasks auto-expire after 7 days"#
        // TODO: scheduler tools share ToolKind::Other so they can't be template-ized
        // via ${{ tools.by_kind.* }}. If tool name randomization is needed, add
        // dedicated ToolKind variants (SchedulerCreate, SchedulerDelete, SchedulerList).
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        // A create call only registers the task (the actor emits
        // ScheduledTaskCreated). Fired/Removed come later from the actor timer,
        // delete, or shutdown — not from this tool's execution.
        &["ScheduledTaskCreated"]
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl kigi_tool_runtime::Tool for SchedulerCreateTool {
    type Args = SchedulerCreateInput;
    type Output = SchedulerCreateOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new(SCHEDULER_CREATE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(
            "scheduler_create",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.scheduler_create",
        skip_all,
        fields(interval = %input.interval)
    )]
    async fn run(
        &self,
        ctx: kigi_tool_runtime::ToolCallContext,
        input: SchedulerCreateInput,
    ) -> Result<SchedulerCreateOutput, kigi_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let interval_secs = parse_interval(&input.interval)
            .map_err(|e| kigi_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

        let sender = {
            let res = resources.lock().await;
            res.get::<SchedulerHandle>()
                .ok_or_else(|| {
                    kigi_tool_runtime::ToolError::custom("missing_resource", "SchedulerHandle")
                })?
                .0
                .clone()
        };

        let durable = input.durable.unwrap_or(false);
        let task = ScheduledTask::with_fire_immediately(
            interval_secs,
            input.prompt,
            input.recurring,
            durable,
            input.fire_immediately,
        );

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(SchedulerCommand::Create {
                task: task.clone(),
                reply: reply_tx,
            })
            .map_err(|_| {
                kigi_tool_runtime::ToolError::custom("process_manager", "Scheduler actor stopped")
            })?;

        let created = reply_rx
            .await
            .map_err(|_| {
                kigi_tool_runtime::ToolError::custom(
                    "process_manager",
                    "Scheduler actor dropped reply",
                )
            })?
            .map_err(|e| kigi_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

        Ok(SchedulerCreateOutput {
            id: created.id,
            human_schedule: interval_to_human(interval_secs),
            recurring: input.recurring,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_usage_message_has_no_host_default() {
        let usage = loop_usage_message();
        assert!(usage.contains("Usage: /loop"));
        assert!(
            !usage.contains("10m"),
            "usage must not claim a default: {usage}"
        );
    }

    #[test]
    fn loop_schedule_instruction_holds_invariants() {
        let args = "every 30 minutes do x";
        let instr = loop_schedule_instruction(args);
        assert!(
            !instr.contains("10m"),
            "instruction must not default: {instr}"
        );
        assert!(instr.contains("Deriving the interval"));
        assert!(instr.contains("<number><unit>"));
        assert!(instr.contains("ask the user how often"));
        assert!(instr.contains("Do NOT execute the prompt inline"));
        // Raw request forwarded verbatim for the model to parse.
        assert!(instr.contains(args));
    }
}
