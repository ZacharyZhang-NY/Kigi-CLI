//! Topology optimizer (G6): a plan-boundary review pass that may issue
//! a RESTRICTED set of graph edits — remove false deps (restoring
//! parallelism), reorder pending priority, merge tiny nodes, split
//! oversized ones — over Waiting/Ready nodes only.
//!
//! The optimizer changes GRAPH DATA only; the executor stays pure
//! deterministic Rust. It fires ① right after initial planning and
//! ② at each replan boundary (piggybacked), never mid-execution.
//! Applied (non-empty) passes bump `plan_version`, freeze a baseline,
//! and consume a slot of the SHARED replan cap; an explicit `[]` is a
//! respected no-op consuming nothing. Failure degrades — the current
//! graph keeps running. `KIGI_GRAPH_OPTIMIZER=0` disables entirely.

use std::sync::Arc;

use super::super::goal_planner::{ChannelSpawner, GoalPlannerSpawner};
use super::super::graph_plan;
use super::super::graph_planner::{ArtifactPassSpec, run_graph_artifact_pass};
use super::SessionActor;

const OPTIMIZER_PROMPT_TEMPLATE: &str = include_str!("../templates/graph_optimizer_prompt.md");

impl SessionActor {
    /// One optimizer pass at a plan boundary. No-op when disabled, when
    /// the shared cap is exhausted, or when the graph is not Active.
    pub(super) async fn maybe_optimize_graph(&self) {
        if !self.graph_optimizer_enabled {
            return;
        }
        let (replan_runs, current_graph, history_text, graph_file, next_version) = {
            let tracker = self.graph_tracker.lock();
            let Some(state) = tracker.snapshot() else {
                return;
            };
            if state.status != crate::session::goal_tracker::GoalStatus::Active {
                return;
            }
            let compact: Vec<serde_json::Value> = state
                .nodes
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "title": n.title,
                        "spec": n.spec,
                        "status": format!("{:?}", n.status),
                        "deps": n.deps.iter().map(|d| d.on.clone()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let history_text = state
                .history
                .iter()
                .rev()
                .take(12)
                .map(|h| {
                    format!(
                        "- {:?} {} {}",
                        h.event,
                        h.node_id.as_deref().unwrap_or("-"),
                        h.detail.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                state.replan_runs,
                serde_json::to_string(&compact).unwrap_or_default(),
                history_text,
                tracker
                    .artifacts_dir()
                    .join(format!("optimize.v{}.json", state.plan_version + 1)),
                state.plan_version + 1,
            )
        };
        if self.graph_replan_cap == 0 || replan_runs >= self.graph_replan_cap {
            tracing::info!(
                replan_runs,
                cap = self.graph_replan_cap,
                "graph optimizer: shared cap exhausted; skipping pass"
            );
            return;
        }
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            return;
        };
        let objective = self
            .graph_tracker
            .lock()
            .objective()
            .map(str::to_owned)
            .unwrap_or_default();
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let spawner: Arc<dyn GoalPlannerSpawner> = Arc::new(ChannelSpawner {
            event_tx,
            parent_session_id: self.session_id_string(),
            parent_prompt_id,
            cwd: Some(self.tool_context.cwd.as_str().to_owned()),
            role_override: Default::default(),
            events: Some(self.events.writer()),
        });
        let tool_names = self.resolve_inherit_role_tool_names().await;
        let sections = format!(
            "OBJECTIVE:\n{objective}\n\nCURRENT GRAPH:\n{current_graph}\n\n\
             EXECUTION HISTORY:\n{history_text}\n"
        );
        tracing::info!(next_version, "graph optimizer: firing");
        let json = match run_graph_artifact_pass(
            spawner,
            ArtifactPassSpec {
                template: OPTIMIZER_PROMPT_TEMPLATE,
                sections: &sections,
                graph_file: &graph_file,
                tool_names: &tool_names,
                role: "graph optimizer",
            },
        )
        .await
        {
            Ok(json) => json,
            Err(reason) => {
                // Degrade: an enhancement pass never blocks the graph.
                tracing::warn!(%reason, "graph optimizer: pass failed; keeping current plan");
                return;
            }
        };
        let existing = self
            .graph_tracker
            .lock()
            .snapshot()
            .map(|s| s.nodes.clone())
            .unwrap_or_default();
        match graph_plan::apply_optimization(&existing, &json) {
            Ok(None) => {
                tracing::info!("graph optimizer: no ops (already good)");
            }
            Ok(Some(optimized)) => {
                let n_before = existing.len();
                let n_after = optimized.len();
                self.graph_tracker.lock().install_optimized_nodes(optimized);
                let all_nodes = self
                    .graph_tracker
                    .lock()
                    .snapshot()
                    .map(|s| s.nodes.clone())
                    .unwrap_or_default();
                if let Err(err) = self.write_graph_baseline(&all_nodes).await {
                    tracing::warn!(%err, "graph optimizer: baseline write failed (audit gap only)");
                }
                self.persist_graph_state();
                tracing::info!(
                    next_version,
                    n_before,
                    n_after,
                    "graph optimizer: plan optimized"
                );
                self.send_slash_command_output(&format!(
                    "Graph optimized (v{next_version}): {n_before} → {n_after} node(s)."
                ))
                .await;
            }
            Err(err) => {
                tracing::warn!(%err, "graph optimizer: ops rejected; keeping current plan");
            }
        }
    }
}
