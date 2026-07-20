//! Dynamic replan (G3): fold `DISCOVERED:` items surfaced during node
//! execution into the graph at dispatch boundaries.
//!
//! SGH version discipline: the running plan is immutable inside a
//! version — a replan appends new nodes (never edits existing ones),
//! bumps `plan_version`, and freezes a new immutable baseline. The pass
//! is BOUNDED by `KIGI_GRAPH_REPLAN_CAP` (default 3, 0 = off): past the
//! cap, discoveries drain to history only, so the graph always
//! converges. Replan failure DEGRADES (discoveries kept in history, the
//! graph keeps running) — unlike initial planning, a working graph is
//! never paused because an enhancement pass failed.

use std::sync::Arc;

use super::super::goal_planner::{ChannelSpawner, GoalPlannerSpawner};
use super::super::graph_planner::{
    GRAPH_REPLANNER_SUBAGENT_DESCRIPTION, GraphPlannerOutcome, GraphReplannerInputs,
    run_graph_replanner,
};
use super::SessionActor;

impl SessionActor {
    /// Replan boundary, called at the top of every `drive_graph`
    /// iteration. No-op without pending discoveries.
    pub(super) async fn maybe_replan_graph(&self) {
        let (pending, replan_runs) = {
            let tracker = self.graph_tracker.lock();
            let Some(state) = tracker.snapshot() else {
                return;
            };
            (state.pending_discoveries.clone(), state.replan_runs)
        };
        if pending.is_empty() {
            return;
        }
        // Budget first: a budget-dead graph must not spend two replanner
        // runs right before the dispatch loop trips BudgetLimited. The
        // discoveries STAY QUEUED (persisted) — a later
        // `/graph resume --budget` top-up re-enters and replans with
        // budget actually available.
        if self.graph_tracker.lock().remaining_budget() == Some(0) {
            tracing::info!("graph replan: budget exhausted; keeping discoveries queued");
            return;
        }
        let final_achieved = self
            .graph_tracker
            .lock()
            .node(super::super::graph_tracker::FINAL_NODE_ID)
            .is_some_and(|n| n.status == super::super::graph_tracker::NodeStatus::Achieved);
        if final_achieved {
            // The whole-objective gate already passed; late discoveries
            // (typically from the final verification itself) are
            // advisory — appending nodes now would ship work the
            // terminal gate never re-verified.
            let n = self.graph_tracker.lock().drain_discoveries_to_history();
            self.persist_graph_state();
            tracing::info!(
                drained = n,
                "graph replan: final already achieved; history only"
            );
            return;
        }
        if self.graph_replan_cap == 0 {
            // Feature off: quiet drain (history keeps the audit trail).
            let n = self.graph_tracker.lock().drain_discoveries_to_history();
            self.persist_graph_state();
            tracing::info!(drained = n, "graph replan: disabled (cap 0); history only");
            return;
        }
        if replan_runs >= self.graph_replan_cap {
            let n = self.graph_tracker.lock().drain_discoveries_to_history();
            self.persist_graph_state();
            tracing::warn!(
                drained = n,
                replan_runs,
                cap = self.graph_replan_cap,
                "graph replan: cap exhausted; discoveries recorded in history only"
            );
            self.send_slash_command_output(&format!(
                "Graph replan cap reached ({replan_runs}/{}); {n} discover{} recorded in \
                 history only — the graph will converge on the current plan.",
                self.graph_replan_cap,
                if n == 1 { "y" } else { "ies" },
            ))
            .await;
            return;
        }

        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::warn!("graph replan: no subagent coordinator; keeping discoveries queued");
            return;
        };
        let (existing, objective, current_graph, discoveries_text, graph_file, next_version) = {
            let tracker = self.graph_tracker.lock();
            let Some(state) = tracker.snapshot() else {
                return;
            };
            let compact: Vec<serde_json::Value> = state
                .nodes
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "title": n.title,
                        "status": format!("{:?}", n.status),
                        "deps": n.deps.iter().map(|d| d.on.clone()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let discoveries_text = pending
                .iter()
                .map(|d| format!("- (from {}) {}", d.from_node, d.description))
                .collect::<Vec<_>>()
                .join("\n");
            (
                state.nodes.clone(),
                state.objective.clone(),
                serde_json::to_string(&compact).unwrap_or_default(),
                discoveries_text,
                tracker
                    .artifacts_dir()
                    .join(format!("replan.v{}.json", state.plan_version + 1)),
                state.plan_version + 1,
            )
        };
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

        let mut feedback = String::new();
        for attempt in 1..=2u32 {
            tracing::info!(
                attempt,
                next_version,
                role = GRAPH_REPLANNER_SUBAGENT_DESCRIPTION,
                pending = pending.len(),
                "graph replan: firing"
            );
            match run_graph_replanner(
                spawner.clone(),
                &existing,
                GraphReplannerInputs {
                    objective: &objective,
                    current_graph: &current_graph,
                    discoveries: &discoveries_text,
                    feedback: &feedback,
                    graph_file: &graph_file,
                    tool_names: &tool_names,
                    inherit_tool_names: &tool_names,
                },
            )
            .await
            {
                GraphPlannerOutcome::Planned(appendix) if appendix.is_empty() => {
                    // Escape hatch: everything already covered. The pass
                    // still counts against the cap.
                    tracing::info!("graph replan: empty appendix (already covered)");
                    {
                        let mut tracker = self.graph_tracker.lock();
                        tracker.drain_discoveries_to_history();
                        if let Some(state) = tracker.snapshot_mut() {
                            state.replan_runs += 1;
                        }
                    }
                    self.persist_graph_state();
                    return;
                }
                GraphPlannerOutcome::Planned(appendix) => {
                    let added = appendix.len();
                    self.graph_tracker.lock().append_replan_nodes(appendix);
                    // Freeze the new version's immutable baseline (full
                    // node set post-append; create_new keeps v{N-1}
                    // byte-identical forever).
                    let all_nodes = self
                        .graph_tracker
                        .lock()
                        .snapshot()
                        .map(|s| s.nodes.clone())
                        .unwrap_or_default();
                    if let Err(err) = self.write_graph_baseline(&all_nodes).await {
                        tracing::warn!(%err, "graph replan: baseline write failed (audit gap only)");
                    }
                    self.persist_graph_state();
                    tracing::info!(added, next_version, "graph replan: appendix installed");
                    self.send_slash_command_output(&format!(
                        "Graph replanned (v{next_version}): {added} node(s) added from \
                         discovered work."
                    ))
                    .await;
                    return;
                }
                GraphPlannerOutcome::Invalid { reason } if attempt == 1 => {
                    tracing::warn!(%reason, "graph replan: invalid appendix; retrying with feedback");
                    feedback = format!(
                        "Your previous replan JSON failed validation:\n{reason}\n\
                         Rewrite the file fixing exactly this."
                    );
                }
                GraphPlannerOutcome::Invalid { reason }
                | GraphPlannerOutcome::FailClosed { reason } => {
                    // Degrade, never pause a working graph for a failed
                    // enhancement pass. The run still counts.
                    tracing::warn!(%reason, "graph replan: failed; draining discoveries to history");
                    {
                        let mut tracker = self.graph_tracker.lock();
                        tracker.drain_discoveries_to_history();
                        if let Some(state) = tracker.snapshot_mut() {
                            state.replan_runs += 1;
                        }
                    }
                    self.persist_graph_state();
                    self.send_slash_command_output(&format!(
                        "Graph replan failed ({reason}); discovered work recorded in \
                         history only."
                    ))
                    .await;
                    return;
                }
            }
        }
    }
}
