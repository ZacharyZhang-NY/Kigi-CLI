//! Graph planner runner: one attempt at decomposing an objective into a
//! validated node DAG.
//!
//! Deliberately thin: the spawn plumbing (harness-internal subagent,
//! verbatim-fork, fail-open model retry) is reused from
//! [`goal_planner`](super::goal_planner) via the same
//! [`GoalPlannerSpawner`] contract; this module only swaps the template
//! and replaces "plan file exists" with "graph JSON parses and passes
//! the static DAG gate" ([`graph_plan::parse_and_validate`]).
//!
//! Outcome split (both are loud, nothing is papered over):
//! - [`GraphPlannerOutcome::Invalid`] — the planner wrote an artifact
//!   that failed validation. Retryable ONCE by the caller, feeding the
//!   precise validation error back as CONTEXT.
//! - [`GraphPlannerOutcome::FailClosed`] — spawn/transport/missing-file
//!   failure. The caller pauses the graph; `/graph resume` retries.

use std::path::Path;
use std::sync::Arc;

use super::goal_planner::{
    GoalPlannerSpawner, RoleRenderedPrompt, SpawnError, parse_terminal_response,
};
use super::goal_role_tools::RoleToolNames;
use super::graph_plan::{self, MAX_GRAPH_JSON_BYTES};
use super::graph_tracker::GraphNode;

const GRAPH_PLANNER_PROMPT_TEMPLATE: &str = include_str!("templates/graph_planner_prompt.md");
pub(crate) const GRAPH_PLANNER_SUBAGENT_DESCRIPTION: &str = "graph plan writer";

#[derive(Debug)]
pub(crate) enum GraphPlannerOutcome {
    /// Validated, canonicalized nodes (topo-ordered, final node appended).
    Planned(Vec<GraphNode>),
    /// Artifact written but rejected by the static gate; retry once with
    /// the reason as feedback.
    Invalid { reason: String },
    /// Infrastructure/spawn failure or missing artifact; pause the graph.
    FailClosed { reason: String },
}

pub(crate) struct GraphPlannerInputs<'a> {
    pub objective: &'a str,
    /// Empty on the first attempt; the previous attempt's validation
    /// error on the retry.
    pub feedback: &'a str,
    pub graph_file: &'a Path,
    pub tool_names: &'a RoleToolNames,
    pub inherit_tool_names: &'a RoleToolNames,
}

/// Run one graph-planner attempt end to end: render, spawn, read the
/// artifact (size-capped), validate, canonicalize.
pub(crate) async fn run_graph_planner(
    spawner: Arc<dyn GoalPlannerSpawner>,
    inputs: GraphPlannerInputs<'_>,
) -> GraphPlannerOutcome {
    if let Some(parent) = inputs.graph_file.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        return GraphPlannerOutcome::FailClosed {
            reason: format!("failed to create graph dir {}: {err}", parent.display()),
        };
    }

    let graph_file_str = inputs.graph_file.to_string_lossy();
    let with_graph_file = GRAPH_PLANNER_PROMPT_TEMPLATE.replace("{GRAPH_FILE}", &graph_file_str);
    let render = |tool_names: &RoleToolNames| -> String {
        let rendered = tool_names.apply(&with_graph_file);
        let mut full = String::with_capacity(rendered.len() + inputs.objective.len() + 256);
        full.push_str(&rendered);
        full.push_str("\n\nOBJECTIVE:\n");
        full.push_str(inputs.objective);
        full.push_str("\n\nCONTEXT:\n");
        full.push_str(inputs.feedback);
        full.push('\n');
        full
    };
    let prompt = RoleRenderedPrompt {
        primary: render(inputs.tool_names),
        fallback: render(inputs.inherit_tool_names),
    };

    let spawn_id = uuid::Uuid::now_v7().to_string();
    let response = match spawner.spawn_planner(&spawn_id, prompt).await {
        Ok(text) => text,
        Err(SpawnError::Transport(detail)) => {
            return GraphPlannerOutcome::FailClosed {
                reason: format!("graph planner transport error: {detail}"),
            };
        }
        Err(SpawnError::Runtime { message, cancelled }) => {
            return GraphPlannerOutcome::FailClosed {
                reason: if cancelled {
                    format!("graph planner aborted: {message}")
                } else {
                    format!("graph planner runtime error: {message}")
                },
            };
        }
    };

    match tokio::fs::metadata(inputs.graph_file).await {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {
            if meta.len() > MAX_GRAPH_JSON_BYTES {
                return GraphPlannerOutcome::Invalid {
                    reason: format!(
                        "graph JSON is {} bytes; the cap is {MAX_GRAPH_JSON_BYTES}",
                        meta.len()
                    ),
                };
            }
        }
        _ => {
            tracing::info!(
                graph_file = %graph_file_str,
                terminal_token_ok = parse_terminal_response(&response),
                response_snippet = %response.chars().take(120).collect::<String>(),
                "graph planner: graph file missing or empty; failing closed",
            );
            return GraphPlannerOutcome::FailClosed {
                reason: "graph planner produced no graph file".to_owned(),
            };
        }
    }

    let json = match tokio::fs::read_to_string(inputs.graph_file).await {
        Ok(json) => json,
        Err(err) => {
            return GraphPlannerOutcome::FailClosed {
                reason: format!("failed to read graph file: {err}"),
            };
        }
    };

    match graph_plan::parse_and_validate(&json, inputs.objective) {
        Ok(nodes) => GraphPlannerOutcome::Planned(nodes),
        Err(err) => GraphPlannerOutcome::Invalid {
            reason: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::goal_role_tools::tests::summary_with;
    use kigi_tools::types::tool::ToolKind;
    use std::path::PathBuf;
    use std::sync::Mutex;

    enum MockReply {
        Done,
        Transport,
        Runtime { cancelled: bool },
    }

    struct MockSpawner {
        response: MockReply,
        body: Option<Vec<u8>>,
        target: PathBuf,
        last_prompt: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl GoalPlannerSpawner for MockSpawner {
        async fn spawn_planner(
            &self,
            _id: &str,
            prompt: RoleRenderedPrompt,
        ) -> Result<String, SpawnError> {
            *self.last_prompt.lock().unwrap() = Some(prompt.primary.clone());
            if let Some(body) = &self.body {
                std::fs::write(&self.target, body).unwrap();
            }
            match &self.response {
                MockReply::Done => Ok("Done".to_owned()),
                MockReply::Transport => Err(SpawnError::Transport("channel closed".into())),
                MockReply::Runtime { cancelled } => Err(SpawnError::Runtime {
                    message: "boom".into(),
                    cancelled: *cancelled,
                }),
            }
        }
    }

    fn tool_names() -> RoleToolNames {
        RoleToolNames::from_summary(&summary_with(&[
            (ToolKind::Read, "read_file"),
            (ToolKind::Search, "grep"),
            (ToolKind::List, "list_files"),
            (ToolKind::Write, "write"),
        ]))
    }

    /// Self-cleaning temp home per test: never leak dirs into the OS
    /// temp root (storage discipline — see AGENTS.md gates).
    fn tmp_graph_file(_name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("graph.json");
        (dir, path)
    }

    async fn run(spawner: MockSpawner, graph_file: &Path) -> GraphPlannerOutcome {
        let names = tool_names();
        run_graph_planner(
            Arc::new(spawner),
            GraphPlannerInputs {
                objective: "build the thing",
                feedback: "",
                graph_file,
                tool_names: &names,
                inherit_tool_names: &names,
            },
        )
        .await
    }

    #[tokio::test]
    async fn valid_artifact_yields_canonical_nodes() {
        let (_tmp, target) = tmp_graph_file("valid");
        let body = serde_json::json!({
            "nodes": [
                {"id": "core", "title": "Core", "spec": "core spec", "deps": []},
                {"id": "ui", "title": "UI", "spec": "ui spec", "deps": ["core"]},
            ]
        })
        .to_string();
        let spawner = MockSpawner {
            response: MockReply::Done,
            body: Some(body.into_bytes()),
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::Planned(nodes) => {
                assert_eq!(nodes.len(), 3, "2 planner nodes + appended final");
                assert_eq!(nodes[2].id, crate::session::graph_tracker::FINAL_NODE_ID);
            }
            other => panic!("expected Planned, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_embeds_objective_feedback_and_tool_names() {
        let (_tmp, target) = tmp_graph_file("prompt");
        let spawner = MockSpawner {
            response: MockReply::Done,
            body: None,
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        let prompt_cell = std::sync::Arc::new(spawner);
        let names = tool_names();
        let _ = run_graph_planner(
            prompt_cell.clone(),
            GraphPlannerInputs {
                objective: "OBJ-MARKER",
                feedback: "FEEDBACK-MARKER",
                graph_file: &target,
                tool_names: &names,
                inherit_tool_names: &names,
            },
        )
        .await;
        let prompt = prompt_cell.last_prompt.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("OBJ-MARKER"));
        assert!(prompt.contains("FEEDBACK-MARKER"));
        assert!(prompt.contains(&target.to_string_lossy().into_owned()));
        assert!(prompt.contains("read_file"), "placeholders rendered");
        assert!(!prompt.contains("{READ_TOOL}"), "no leftover placeholder");
        assert!(!prompt.contains("{GRAPH_FILE}"), "no leftover placeholder");
    }

    #[tokio::test]
    async fn invalid_artifact_is_retryable_with_reason() {
        let (_tmp, target) = tmp_graph_file("invalid");
        let spawner = MockSpawner {
            response: MockReply::Done,
            body: Some(br#"{"nodes":[{"id":"a","title":"A","spec":"s","deps":["a"]}]}"#.to_vec()),
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::Invalid { reason } => {
                assert!(reason.contains("depends on itself"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_artifact_fails_closed() {
        let (_tmp, target) = tmp_graph_file("missing");
        let spawner = MockSpawner {
            response: MockReply::Done,
            body: None,
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::FailClosed { reason } => {
                assert!(reason.contains("no graph file"), "{reason}");
            }
            other => panic!("expected FailClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_error_fails_closed() {
        let (_tmp, target) = tmp_graph_file("runtime");
        let spawner = MockSpawner {
            response: MockReply::Runtime { cancelled: false },
            body: None,
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::FailClosed { reason } => {
                assert!(reason.contains("runtime error"), "{reason}");
            }
            other => panic!("expected FailClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_artifact_is_invalid_with_cap_in_reason() {
        let (_tmp, target) = tmp_graph_file("oversize");
        let mut body = vec![b'x'; (MAX_GRAPH_JSON_BYTES as usize) + 1];
        body[0] = b'{'; // content is irrelevant; the size gate fires first
        let spawner = MockSpawner {
            response: MockReply::Done,
            body: Some(body),
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::Invalid { reason } => {
                assert!(reason.contains("the cap is"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_fails_closed() {
        let (_tmp, target) = tmp_graph_file("transport");
        let spawner = MockSpawner {
            response: MockReply::Transport,
            body: None,
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::FailClosed { reason } => {
                assert!(reason.contains("transport error"), "{reason}");
            }
            other => panic!("expected FailClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelled_runtime_error_reports_aborted() {
        let (_tmp, target) = tmp_graph_file("aborted");
        let spawner = MockSpawner {
            response: MockReply::Runtime { cancelled: true },
            body: None,
            target: target.clone(),
            last_prompt: Mutex::new(None),
        };
        match run(spawner, &target).await {
            GraphPlannerOutcome::FailClosed { reason } => {
                assert!(reason.contains("aborted"), "{reason}");
            }
            other => panic!("expected FailClosed, got {other:?}"),
        }
    }
}
