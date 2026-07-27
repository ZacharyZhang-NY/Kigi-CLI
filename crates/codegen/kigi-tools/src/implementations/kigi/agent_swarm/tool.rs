//! The `agent_swarm` tool: validate, then run every member to completion.

use kigi_tool_types::AgentSwarmToolInput;

use super::plan::{plan_members, render_results};
use super::run::{SwarmRunConfig, run_swarm};
use super::schedule::{MAX_CONCURRENCY_ENV, max_concurrency_from_env};
use crate::implementations::kigi::task::MAX_SUBAGENT_DEPTH;
use crate::implementations::kigi::task::backend::SubagentBackendResource;
use crate::implementations::kigi::task::types::{
    CurrentPromptIdResource, SessionIdResource, SubagentDepthCounter, SubagentValidateTypeOutcome,
    TaskModelValidator,
};
use crate::types::output::ToolOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

const DESCRIPTION: &str = "\
Run one prompt over many independent work items at once, as a fleet of subagents.

Give a `prompt_template` containing the literal {{item}} and an `items` list; each entry \
becomes one subagent whose prompt is the template with {{item}} substituted. The call \
returns only when every member has finished, with each member's result labelled by its item.

Use this when the work splits into 2 or more INDEPENDENT scopes — separate files, separate \
directories, separate questions. Every member must have a distinct scope: members share one \
working tree with no isolation, so two members told to edit the same file will corrupt each \
other's work. Read-only scopes may overlap freely.

For a single item, use the subagent (task) tool instead. To continue members from an earlier \
swarm, pass `resume_agent_ids` mapping the agent_id values from that swarm's result to a \
follow-up prompt.";

/// Registry name, so gating code matches on one definition rather than a
/// literal that can drift from the tool id.
pub const AGENT_SWARM_TOOL_NAME: &str = "agent_swarm";

#[derive(Debug, Default)]
pub struct AgentSwarmTool;

impl crate::types::tool_metadata::ToolMetadata for AgentSwarmTool {
    fn kind(&self) -> ToolKind {
        ToolKind::AgentSwarm
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Kigi
    }

    fn description_template(&self) -> &str {
        DESCRIPTION
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        // Members are subagents, so the same background-task management tools
        // the `task` tool depends on must be present.
        Expr::And(vec![
            Expr::Value(ToolRequirement::tool_kind(ToolKind::BackgroundTaskAction)),
            Expr::Value(ToolRequirement::tool_kind(ToolKind::KillTaskAction)),
        ])
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl kigi_tool_runtime::Tool for AgentSwarmTool {
    type Args = AgentSwarmToolInput;
    type Output = ToolOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new(AGENT_SWARM_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(AGENT_SWARM_TOOL_NAME, DESCRIPTION)
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.agent_swarm",
        skip_all,
        fields(
            subagent_type = %input.subagent_type,
            members = input.items.len() + input.resume_agent_ids.len(),
        )
    )]
    async fn run(
        &self,
        ctx: kigi_tool_runtime::ToolCallContext,
        input: AgentSwarmToolInput,
    ) -> Result<ToolOutput, kigi_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let (depth, backend, model_validator, parent_session_id, parent_prompt_id) = {
            let res = resources.lock().await;
            let depth = res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0);
            let model_validator = res.get::<TaskModelValidator>().cloned();
            let backend = res
                .get::<SubagentBackendResource>()
                .ok_or_else(|| {
                    kigi_tool_runtime::ToolError::custom(
                        "missing_resource",
                        "SubagentBackendResource (subagent support not initialized)",
                    )
                })?
                .clone();
            let parent_session_id = res
                .get::<SessionIdResource>()
                .map(|s| s.0.clone())
                .unwrap_or_default();
            let parent_prompt_id = res
                .get::<CurrentPromptIdResource>()
                .map(|p| p.0.clone())
                .filter(|prompt_id| !prompt_id.is_empty());
            (
                depth,
                backend,
                model_validator,
                parent_session_id,
                parent_prompt_id,
            )
        };

        if depth >= MAX_SUBAGENT_DEPTH {
            return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                "Subagent depth limit exceeded (current depth: {depth}, max: {MAX_SUBAGENT_DEPTH}). \
                 A subagent cannot start a swarm."
            )));
        }

        let max_concurrency =
            max_concurrency_from_env(std::env::var(MAX_CONCURRENCY_ENV).ok().as_deref())
                .map_err(kigi_tool_runtime::ToolError::invalid_arguments)?;

        // Every input fault is reported before a single member starts: a
        // half-launched swarm costs real tokens to unwind, and one bad model
        // slug would otherwise fan out into as many failures as there are items.
        let model = kigi_tool_types::sanitize_optional_arg(input.model.clone());
        if let Some(requested) = model.as_deref() {
            // Same contract as the `task` tool: an explicitly requested model
            // that cannot be checked is refused, not waved through. Skipping
            // silently would trade one loud error for `items.len()` quiet ones.
            let validator = model_validator.ok_or_else(|| {
                kigi_tool_runtime::ToolError::custom(
                    "validation_unavailable",
                    "Cannot validate agent_swarm.model: model catalog validator is unavailable.",
                )
            })?;
            if let Some(error) = validator.error_for(requested) {
                return Err(kigi_tool_runtime::ToolError::invalid_arguments(error));
            }
        }
        let specs =
            plan_members(&input).map_err(kigi_tool_runtime::ToolError::invalid_arguments)?;

        match backend
            .0
            .validate_type(&input.subagent_type, &parent_session_id)
            .await
        {
            SubagentValidateTypeOutcome::Ok => {}
            SubagentValidateTypeOutcome::Unknown { available } => {
                let suffix = if available.is_empty() {
                    String::new()
                } else {
                    format!(". Available types: {}", available.join(", "))
                };
                return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                    "Unknown subagent type: {}{suffix}",
                    input.subagent_type
                )));
            }
            SubagentValidateTypeOutcome::Disabled => {
                return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                    "Subagent '{}' is disabled via [subagents.toggle] in config.toml",
                    input.subagent_type
                )));
            }
            SubagentValidateTypeOutcome::NotAllowed { allowed } => {
                return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                    "agent can only spawn: {}; '{}' not allowed",
                    allowed.join(", "),
                    input.subagent_type
                )));
            }
            SubagentValidateTypeOutcome::ValidationUnavailable => {
                // `custom` (not `invalid_arguments`) so the model doesn't
                // retry with a different name on transport faults.
                return Err(kigi_tool_runtime::ToolError::custom(
                    "validation_unavailable",
                    format!(
                        "Cannot validate subagent type '{}': the subagent coordinator is \
                         unreachable. Retry shortly or notify ops.",
                        input.subagent_type
                    ),
                ));
            }
        }

        let config = SwarmRunConfig {
            subagent_type: input.subagent_type.clone(),
            description: input.description.clone(),
            parent_session_id,
            parent_prompt_id,
            model,
            // Members share the caller's tree; see `run::build_request`.
            cwd: None,
            max_concurrency,
        };

        let results = run_swarm(backend.0.clone(), specs, config).await;
        Ok(ToolOutput::Text(render_results(&results).into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::kigi::task::backend::ChannelBackend;
    use crate::implementations::kigi::task::types::{SubagentEvent, SubagentResult};
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx;
    use kigi_env::EnvVarGuard;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// What the coordinator was actually asked to do — a refusal that still
    /// reached the backend is the failure these tests exist to catch.
    #[derive(Default)]
    struct Seen {
        validations: AtomicUsize,
        spawns: AtomicUsize,
    }

    impl Seen {
        fn validations(&self) -> usize {
            self.validations.load(Ordering::SeqCst)
        }
        fn spawns(&self) -> usize {
            self.spawns.load(Ordering::SeqCst)
        }
    }

    /// `run` resolves the operator cap from the process environment, which every
    /// test in this binary shares. Each test therefore pins the variable for its
    /// duration through [`EnvVarGuard`], whose lock also serializes them against
    /// each other. The tests are synchronous so that lock never spans an await.
    fn cap_unset() -> EnvVarGuard {
        EnvVarGuard::remove(MAX_CONCURRENCY_ENV)
    }

    fn swarm_input(items: &[&str]) -> AgentSwarmToolInput {
        AgentSwarmToolInput {
            description: "test swarm".into(),
            subagent_type: "general-purpose".into(),
            prompt_template: Some("Review {{item}} for bugs".into()),
            items: items.iter().map(|s| (*s).to_string()).collect(),
            resume_agent_ids: Default::default(),
            model: None,
        }
    }

    /// Drives one whole tool call against a live channel backend that answers
    /// `ValidateType` with `validate` and completes every spawn.
    fn call(
        depth: u32,
        validate: SubagentValidateTypeOutcome,
        input: AgentSwarmToolInput,
    ) -> (Result<ToolOutput, String>, Arc<Seen>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<SubagentEvent>();
        let mut resources = Resources::new();
        resources.insert(SubagentBackendResource(Arc::new(ChannelBackend::new(tx))));
        resources.insert(SubagentDepthCounter(depth));
        resources.insert(SessionIdResource("parent-session".to_string()));
        resources.insert(CurrentPromptIdResource("prompt-1".to_string()));

        let seen = Arc::new(Seen::default());
        let recorder = seen.clone();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async move {
                let pump = tokio::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        match event {
                            SubagentEvent::ValidateType(req) => {
                                recorder.validations.fetch_add(1, Ordering::SeqCst);
                                let _ = req.respond_to.send(validate.clone());
                            }
                            SubagentEvent::Spawn(req) => {
                                recorder.spawns.fetch_add(1, Ordering::SeqCst);
                                let _ = req.result_tx.send(SubagentResult {
                                    success: true,
                                    output: Arc::from(format!("done: {}", req.prompt)),
                                    subagent_id: req.id.clone(),
                                    child_session_id: req.id.clone(),
                                    ..Default::default()
                                });
                            }
                            _ => {}
                        }
                    }
                });
                let out = kigi_tool_runtime::Tool::run(
                    &AgentSwarmTool,
                    test_ctx(resources.into_shared()),
                    input,
                )
                .await
                .map_err(|e| e.to_string());
                pump.abort();
                out
            });
        (result, seen)
    }

    #[test]
    fn every_member_runs_and_the_call_returns_one_aggregate() {
        let _cap = cap_unset();
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["a.rs", "b.rs"]),
        );

        match result.expect("a valid swarm runs") {
            ToolOutput::Text(text) => {
                assert!(
                    text.text
                        .contains("completed: 2, failed: 0, aborted: 0, still running: 0"),
                    "{}",
                    text.text
                );
                assert!(text.text.contains(r#"item="a.rs""#), "{}", text.text);
                assert!(text.text.contains(r#"item="b.rs""#), "{}", text.text);
            }
            other => panic!("expected Text output, got {other:?}"),
        }
        assert_eq!(seen.spawns(), 2, "one member per item");
    }

    /// A swarm child must never start its own swarm: kigi caps subagent nesting
    /// at one level, and a fan-out tool is exactly how that cap would be lost —
    /// 128 members each starting 128 more.
    #[test]
    fn a_child_at_the_depth_ceiling_cannot_start_a_swarm() {
        let _cap = cap_unset();
        let (result, seen) = call(
            MAX_SUBAGENT_DEPTH,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["a.rs", "b.rs"]),
        );

        let err = result.expect_err("a child must not fan out");
        assert!(err.contains("depth limit exceeded"), "error: {err}");
        assert_eq!(seen.spawns(), 0, "no member may reach the coordinator");
        assert_eq!(
            seen.validations(),
            0,
            "the coordinator must not even be asked to validate"
        );
    }

    /// An operator who set a concurrency ceiling must never silently get an
    /// unbounded fan-out because the value failed to parse.
    #[test]
    fn a_malformed_operator_cap_fails_the_call_rather_than_being_ignored() {
        let _cap = EnvVarGuard::set(MAX_CONCURRENCY_ENV, "lots");
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["a.rs", "b.rs"]),
        );

        let err = result.expect_err("a cap that does not parse must reject the call");
        assert!(err.contains(MAX_CONCURRENCY_ENV), "error: {err}");
        assert!(err.contains("positive integer"), "error: {err}");
        assert_eq!(
            seen.spawns(),
            0,
            "an unparseable cap must stop the swarm before anything spawns"
        );
    }

    /// A well-formed cap is honoured rather than rejected — the fail-fast path
    /// above must not swallow valid operator configuration.
    #[test]
    fn a_well_formed_operator_cap_still_runs_the_swarm() {
        let _cap = EnvVarGuard::set(MAX_CONCURRENCY_ENV, "1");
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["a.rs", "b.rs"]),
        );

        assert!(result.is_ok(), "a valid cap must not fail the call");
        assert_eq!(seen.spawns(), 2, "every member still runs, just serially");
    }

    /// `plan_members` rejections reach the model as invalid_arguments, and
    /// nothing spawns.
    /// An explicitly requested model that cannot be checked is refused, so a
    /// bad slug fails once here instead of once per member.
    #[test]
    fn an_unvalidatable_model_fails_the_call_before_any_member_spawns() {
        let _guard = cap_unset();
        let input = AgentSwarmToolInput {
            model: Some("nonexistent/model".into()),
            ..swarm_input(&["a.rs", "b.rs"])
        };
        let (result, seen) = call(0, SubagentValidateTypeOutcome::Ok, input);
        let err = result.expect_err("no validator resource is registered in this fixture");
        assert!(err.contains("validate"), "{err}");
        assert_eq!(
            seen.spawns(),
            0,
            "nothing may spawn on an unvalidated model"
        );
    }

    /// The discriminating partner: with no model requested the same call runs.
    #[test]
    fn omitting_the_model_leaves_the_swarm_runnable() {
        let _guard = cap_unset();
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["a.rs", "b.rs"]),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(seen.spawns(), 2);
    }

    #[test]
    fn a_single_item_is_refused_in_favour_of_the_task_tool() {
        let _cap = cap_unset();
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Ok,
            swarm_input(&["only.rs"]),
        );

        let err = result.expect_err("one item is a task, not a swarm");
        assert!(err.contains("task tool"), "error: {err}");
        assert_eq!(seen.spawns(), 0, "nothing may spawn");
    }

    #[test]
    fn an_unknown_subagent_type_is_refused_before_any_member_spawns() {
        let _cap = cap_unset();
        let mut input = swarm_input(&["a.rs", "b.rs"]);
        input.subagent_type = "invented-agent".into();
        let (result, seen) = call(
            0,
            SubagentValidateTypeOutcome::Unknown {
                available: vec!["general-purpose".to_string(), "explore".to_string()],
            },
            input,
        );

        let err = result.expect_err("an unknown type must reject");
        assert!(
            err.contains("Unknown subagent type: invented-agent"),
            "error: {err}"
        );
        assert!(err.contains("explore"), "error: {err}");
        assert_eq!(seen.validations(), 1, "the type is validated exactly once");
        assert_eq!(seen.spawns(), 0, "no member may spawn");
    }

    #[test]
    fn a_missing_backend_is_reported_rather_than_silently_skipped() {
        let _cap = cap_unset();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(kigi_tool_runtime::Tool::run(
                &AgentSwarmTool,
                test_ctx(Resources::new().into_shared()),
                swarm_input(&["a.rs", "b.rs"]),
            ));

        let err = result.expect_err("no backend must error").to_string();
        assert!(err.contains("SubagentBackendResource"), "error: {err}");
    }
}
