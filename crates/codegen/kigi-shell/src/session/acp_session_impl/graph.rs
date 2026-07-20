//! Graph mode orchestration seam: a deterministic DAG scheduler layered
//! over the goal engine.
//!
//! Every node executes as one ordinary goal (planner → worker loop →
//! adversarial verifier), so the agentic loop lives inside the node and
//! this module only does the deterministic parts: decompose (graph
//! planner + static validation), launch the next `Ready` node as a fresh
//! goal, observe the goal's terminal state at the in-turn loop's
//! `EndTurn` boundary, advance the DAG, and persist every transition via
//! [`PersistenceMsg::GraphModeState`].
//!
//! Failure semantics: goal-side auto-pauses cascade to the graph at the
//! single chokepoint (`auto_pause_goal_if_active_inner` in `goal.rs`),
//! and the node budget is always armed with the REMAINING graph budget,
//! so a mid-node overrun trips the goal engine's own enforcement and is
//! mirrored here as a graph-level `BudgetLimited`.

use std::sync::Arc;

use super::super::goal_planner::{ChannelSpawner, GoalPlannerSpawner};
use super::super::goal_tracker::{GoalPauseReason, GoalStatus};
use super::super::graph_planner::{
    GRAPH_PLANNER_SUBAGENT_DESCRIPTION, GraphPlannerInputs, GraphPlannerOutcome, run_graph_planner,
};
use super::super::graph_tracker::{GraphNode, NodeStatus};
use super::super::persistence::PersistenceMsg;
use super::SessionActor;

/// Outcome of `/graph <objective>` and `/graph resume` interception in
/// `handle_prompt`: either flow into inference with a system reminder
/// (mirrors [`GoalResumeOutcome`](super::goal_support::GoalResumeOutcome))
/// or print a terminal message and end the turn.
pub(super) enum GraphSetupOutcome {
    Inference { reminder: String, user_msg: String },
    Message(String),
}

/// Compose the goal objective for one graph node. The node spec is the
/// contract; the graph context line keeps the node model from wandering
/// into other nodes' scope.
pub(super) fn node_goal_objective(
    graph_objective: &str,
    node: &GraphNode,
    position: usize,
    total: usize,
) -> String {
    format!(
        "[Graph node {position}/{total}: {title}]\n\
         {spec}\n\n\
         This node is one unit of a larger graph objective:\n\
         {graph_objective}\n\n\
         Complete ONLY this node's scope; other nodes cover the rest.",
        title = node.title,
        spec = node.spec,
    )
}

/// Snake-case wire form of a graph/goal status (matches the
/// `GoalUpdated` vocabulary the pager already parses).
fn graph_status_str(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::UserPaused => "user_paused",
        GoalStatus::BackOffPaused => "back_off_paused",
        GoalStatus::NoProgressPaused => "no_progress_paused",
        GoalStatus::InfraPaused => "infra_paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::BudgetLimited => "budget_limited",
        GoalStatus::Complete => "complete",
    }
}

fn graph_event_as_str(event: &super::super::graph_tracker::GraphEvent) -> &'static str {
    use super::super::graph_tracker::GraphEvent;
    match event {
        GraphEvent::GraphCreated => "graph_created",
        GraphEvent::PlanningStarted => "planning_started",
        GraphEvent::PlanningCompleted => "planning_completed",
        GraphEvent::PlanningFailed => "planning_failed",
        GraphEvent::NodeStarted => "node_started",
        GraphEvent::NodeAchieved => "node_achieved",
        GraphEvent::NodeFailed => "node_failed",
        GraphEvent::GraphPaused => "graph_paused",
        GraphEvent::GraphResumed => "graph_resumed",
        GraphEvent::GraphCompleted => "graph_completed",
        GraphEvent::GraphCleared => "graph_cleared",
        GraphEvent::BudgetExceeded => "budget_exceeded",
        GraphEvent::Unknown => "unknown",
    }
}

/// Build the pager-facing `GraphUpdated` badge payload from a snapshot.
pub(crate) fn build_graph_updated(
    state: &super::super::graph_tracker::GraphOrchestration,
) -> crate::extensions::notification::SessionUpdate {
    use super::super::goal_tracker::GoalPhase;
    let count = |s: NodeStatus| state.nodes.iter().filter(|n| n.status == s).count() as u32;
    let current = state
        .current_node
        .as_deref()
        .and_then(|id| state.nodes.iter().find(|n| n.id == id));
    crate::extensions::notification::SessionUpdate::GraphUpdated {
        graph_id: state.graph_id.clone(),
        objective: state.objective.clone(),
        status: graph_status_str(state.status).to_owned(),
        phase: match state.phase {
            GoalPhase::Idle => "idle",
            GoalPhase::Planning => "planning",
            GoalPhase::Executing => "executing",
        }
        .to_owned(),
        plan_version: state.plan_version,
        total_nodes: state.nodes.len() as u32,
        achieved_nodes: count(NodeStatus::Achieved),
        failed_nodes: count(NodeStatus::Failed) + count(NodeStatus::Blocked),
        running_nodes: count(NodeStatus::Running) + count(NodeStatus::Verifying),
        current_node: current.map(|n| n.id.clone()),
        current_node_title: current.map(|n| n.title.clone()),
        token_budget: state.token_budget,
        tokens_spent: state.tokens_spent_nodes,
        last_event: state
            .history
            .last()
            .map(|e| graph_event_as_str(&e.event).to_owned()),
        pause_message: state.pause_message.clone(),
    }
}

/// `status: "cleared"` sentinel — the pager drops its graph state.
pub(crate) fn build_graph_cleared() -> crate::extensions::notification::SessionUpdate {
    crate::extensions::notification::SessionUpdate::GraphUpdated {
        graph_id: String::new(),
        objective: String::new(),
        status: "cleared".to_owned(),
        phase: "idle".to_owned(),
        plan_version: 0,
        total_nodes: 0,
        achieved_nodes: 0,
        failed_nodes: 0,
        running_nodes: 0,
        current_node: None,
        current_node_title: None,
        token_budget: None,
        tokens_spent: 0,
        last_event: None,
        pause_message: None,
    }
}

impl SessionActor {
    /// Graph feature flag AND the goal harness (nodes execute as goals).
    pub(super) fn graph_harness_enabled(&self) -> bool {
        self.graph_enabled && self.goal_harness_enabled()
    }

    /// Send the current graph snapshot (or a tombstone after `clear`) to
    /// the persistence actor, AND notify the pager: every graph
    /// transition is both a checkpoint and a `GraphUpdated` badge tick
    /// (single chokepoint — no transition can persist without also
    /// updating the UI, and vice versa).
    pub(crate) fn persist_graph_state(&self) {
        let snapshot = self.graph_tracker.lock().snapshot().cloned();
        let update = match &snapshot {
            Some(state) => build_graph_updated(state),
            None => build_graph_cleared(),
        };
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::GraphModeState(snapshot));
        self.goal_notify_sender().send_update(update);
    }

    /// True when the graph occupies the goal engine (any non-terminal
    /// state). `/goal` commands are refused in this window.
    pub(super) fn graph_owns_goal_engine(&self) -> bool {
        self.graph_tracker
            .lock()
            .status()
            .is_some_and(|s| s == GoalStatus::Active || s.is_paused())
    }

    /// Apply `/graph <objective>` — create the graph, run the graph
    /// planner (one validation retry with feedback), install the DAG,
    /// freeze the immutable baseline, and launch the first node.
    pub(super) async fn setup_graph(
        &self,
        objective: &str,
        token_budget: Option<i64>,
    ) -> GraphSetupOutcome {
        {
            let goal_status = self.goal_tracker.lock().status();
            if matches!(goal_status, Some(s) if s == GoalStatus::Active || s.is_paused()) {
                return GraphSetupOutcome::Message(
                    "A goal is active or paused. Use /goal clear first, then /graph <objective>."
                        .to_owned(),
                );
            }
            let graph_status = self.graph_tracker.lock().status();
            // Refuse over ANYTHING non-Complete — including BudgetLimited,
            // which is resumable and must never be silently replaced.
            if matches!(graph_status, Some(s) if s != GoalStatus::Complete) {
                return GraphSetupOutcome::Message(
                    "A graph is already set. Use /graph status, /graph resume \
                     [--budget <tokens>], or /graph clear."
                        .to_owned(),
                );
            }
        }

        let graph_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(%graph_id, "graph: created, planning started");
        self.graph_tracker.lock().create_graph(
            graph_id,
            objective.to_owned(),
            token_budget,
            chrono::Utc::now().to_rfc3339(),
        );
        self.persist_graph_state();

        let nodes = match self.run_graph_planning(objective).await {
            Ok(nodes) => nodes,
            Err(reason) => {
                tracing::warn!(%reason, "graph: planning failed; pausing");
                {
                    let mut tracker = self.graph_tracker.lock();
                    tracker.record_planning_failed(reason.clone());
                    tracker.pause_with_message(
                        GoalPauseReason::Infra,
                        format!("Graph planning failed: {reason}"),
                    );
                }
                self.persist_graph_state();
                return GraphSetupOutcome::Message(format!(
                    "Graph planning failed: {reason}\n\
                     Use /graph resume to retry or /graph clear to abandon."
                ));
            }
        };

        // Freeze the immutable v1 baseline BEFORE anything executes
        // (SGH: plans are immutable within a version boundary). Failing
        // to write the audit baseline is an infra failure, not ignorable.
        if let Err(err) = self.write_graph_baseline(&nodes).await {
            let reason = format!("failed to write graph baseline: {err}");
            tracing::warn!(%reason, "graph: baseline write failed; pausing");
            {
                let mut tracker = self.graph_tracker.lock();
                tracker.record_planning_failed(reason.clone());
                tracker.pause_with_message(GoalPauseReason::Infra, reason.clone());
            }
            self.persist_graph_state();
            return GraphSetupOutcome::Message(format!(
                "{reason}\nUse /graph resume to retry or /graph clear to abandon."
            ));
        }

        let total = nodes.len();
        self.graph_tracker.lock().install_nodes(nodes);
        self.persist_graph_state();
        tracing::info!(total, "graph: DAG installed, launching first node");

        match self.drive_graph().await {
            Some(reminder) => GraphSetupOutcome::Inference {
                reminder,
                user_msg: format!(
                    "Graph created: {total} nodes (incl. final verification). Starting work."
                ),
            },
            // The dispatch loop already settled (parallel completion,
            // pause, budget) and messaged; point at the status view.
            None => GraphSetupOutcome::Message(
                "Graph did not enter a serial node. See /graph status.".to_owned(),
            ),
        }
    }

    /// One planning pass with a single validation retry: an `Invalid`
    /// artifact re-runs the planner once with the exact validation error
    /// as CONTEXT; infra failures fail closed immediately.
    async fn run_graph_planning(&self, objective: &str) -> Result<Vec<GraphNode>, String> {
        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            return Err("no subagent coordinator channel".to_owned());
        };
        let graph_file = self.graph_tracker.lock().artifacts_dir().join("graph.json");
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        // Verbatim mirror-child fork on the parent model, same rationale
        // as the goal planner (radix-cache reuse).
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
                role = GRAPH_PLANNER_SUBAGENT_DESCRIPTION,
                "graph planner: firing"
            );
            match run_graph_planner(
                spawner.clone(),
                GraphPlannerInputs {
                    objective,
                    feedback: &feedback,
                    graph_file: &graph_file,
                    tool_names: &tool_names,
                    inherit_tool_names: &tool_names,
                },
            )
            .await
            {
                GraphPlannerOutcome::Planned(nodes) => return Ok(nodes),
                GraphPlannerOutcome::Invalid { reason } if attempt == 1 => {
                    tracing::warn!(%reason, "graph planner: invalid artifact; retrying with feedback");
                    feedback = format!(
                        "Your previous graph JSON failed validation:\n{reason}\n\
                         Rewrite the file fixing exactly this."
                    );
                }
                GraphPlannerOutcome::Invalid { reason } => {
                    return Err(format!("graph failed validation twice: {reason}"));
                }
                GraphPlannerOutcome::FailClosed { reason } => return Err(reason),
            }
        }
        unreachable!("planning loop returns on every branch by attempt 2")
    }

    /// Write the immutable plan baseline for the current version.
    /// `create_new` guarantees a frozen baseline is never overwritten —
    /// an existing file is the infra failure it looks like.
    async fn write_graph_baseline(&self, nodes: &[GraphNode]) -> std::io::Result<()> {
        let path = {
            let tracker = self.graph_tracker.lock();
            let version = tracker.snapshot().map(|s| s.plan_version).unwrap_or(1);
            tracker.baseline_path(version)
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_vec_pretty(nodes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &json).await
    }

    /// Launch the next `Ready` node as a fresh goal on a clean engine.
    /// Returns the node goal's system reminder to seed/continue the turn,
    /// or `None` when nothing is launchable (all done / wedged / budget).
    pub(super) async fn launch_next_graph_node(&self) -> Option<String> {
        let (node_id, node_objective, position, total) = {
            let tracker = self.graph_tracker.lock();
            let node = tracker.next_ready_node()?;
            let snapshot = tracker.snapshot()?;
            let position = snapshot
                .nodes
                .iter()
                .position(|n| n.id == node.id)
                .unwrap_or(0)
                + 1;
            (
                node.id.clone(),
                node_goal_objective(&snapshot.objective, node, position, snapshot.nodes.len()),
                position,
                snapshot.nodes.len(),
            )
        };
        let node_budget = self.graph_tracker.lock().remaining_budget();
        if node_budget == Some(0) {
            tracing::warn!(%node_id, "graph: budget exhausted before node start");
            self.graph_tracker.lock().budget_limit();
            self.persist_graph_state();
            self.send_slash_command_output(
                "Graph token budget exhausted. Top up with /graph resume --budget <tokens>, \
                 or /graph clear to abandon.",
            )
            .await;
            return None;
        }

        // Fresh engine per node: the previous node's goal state, token
        // records, and pending classifier claims must not leak.
        self.reset_goal_engine_state().await;
        // Mark Running BEFORE the long setup_goal await (its planner run
        // can take minutes): a cancel landing inside it then finds
        // consistent bookkeeping — node Running, cascade pauses the
        // graph, resume retries the node — instead of a Ready node whose
        // goal is already live.
        self.graph_tracker
            .lock()
            .mark_node_running(&node_id, String::new());
        self.persist_graph_state();
        let reminder = self.setup_goal(&node_objective, node_budget).await;
        let goal_id = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|o| o.goal_id.clone())
            .unwrap_or_default();
        if let Some(node) = self
            .graph_tracker
            .lock()
            .snapshot_mut()
            .and_then(|s| s.nodes.iter_mut().find(|n| n.id == node_id))
        {
            node.goal_id = Some(goal_id.clone());
        }
        self.persist_graph_state();
        // setup_goal fails closed on planner errors by pausing the node
        // goal — and the chokepoint cascade pauses the graph with it. Do
        // NOT hand the now-stale "Start now" reminder to inference.
        if self.goal_tracker.lock().status() != Some(GoalStatus::Active) {
            tracing::warn!(
                %node_id,
                goal_status = ?self.goal_tracker.lock().status(),
                "graph: node goal not active after setup; ending turn instead of launching"
            );
            return None;
        }
        tracing::info!(%node_id, %goal_id, position, total, "graph: node launched");
        Some(reminder)
    }

    /// Graph seam for the in-turn loop, called when the goal loop decided
    /// `EndTurn`. Reads the node goal's terminal state, advances the DAG,
    /// and returns `Some(reminder)` to keep the turn alive on the next
    /// node — or `None` to genuinely end the turn (graph done, paused,
    /// budget-limited, or not in graph mode at all).
    pub(super) async fn run_graph_round_end(&self) -> Option<String> {
        if !self.graph_harness_enabled() || !self.graph_tracker.lock().is_active() {
            return None;
        }
        let goal_status = self.goal_tracker.lock().status();
        match goal_status {
            Some(GoalStatus::Complete) => self.advance_graph_after_node_complete().await,
            Some(GoalStatus::BudgetLimited) => {
                // Node goals are armed with the remaining graph budget,
                // so a node-level trip IS the graph-level trip. The
                // enforce-side cascade normally handles this; mirroring
                // here is idempotent (budget_limit is Active-only), and
                // the two charge sites are mutually exclusive with it.
                tracing::warn!("graph: node goal budget-limited; graph budget-limited");
                let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                let node_tokens = self.goal_tokens_used(current_tokens);
                {
                    let mut tracker = self.graph_tracker.lock();
                    // Budget integrity: the tripped node's partial burn
                    // was still spent — charge it before the demotion
                    // clears current_node.
                    if let Some(node_id) = tracker.current_node_id().map(str::to_owned) {
                        tracker.charge_node_tokens(&node_id, node_tokens);
                    }
                    tracker.budget_limit();
                }
                self.persist_graph_state();
                None
            }
            Some(s) if s.is_paused() => {
                // The auto-pause chokepoint cascade normally paused the
                // graph before we got here (making is_active() false and
                // returning early above). Reaching this arm means a pause
                // path bypassed the chokepoint — mirror it, loudly.
                tracing::warn!(status = ?s, "graph: node goal paused without cascade; mirroring");
                let reason = match s {
                    GoalStatus::BackOffPaused => GoalPauseReason::BackOff,
                    GoalStatus::NoProgressPaused => GoalPauseReason::NoProgress,
                    GoalStatus::InfraPaused => GoalPauseReason::Infra,
                    GoalStatus::Blocked => GoalPauseReason::Verification,
                    _ => GoalPauseReason::User,
                };
                let message = self
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .and_then(|o| o.pause_message.clone());
                let mut tracker = self.graph_tracker.lock();
                match message {
                    Some(msg) => tracker.pause_with_message(reason, msg),
                    None => tracker.pause(reason),
                };
                drop(tracker);
                self.persist_graph_state();
                None
            }
            other => {
                // EndTurn with an Active goal cannot happen (an Active
                // goal always yields Continue), and a missing goal while
                // a node is Running is a launch bug. Pause loudly rather
                // than leaving a self-driving graph with no engine.
                tracing::error!(
                    goal_status = ?other,
                    current_node = ?self.graph_tracker.lock().current_node_id(),
                    "graph: inconsistent engine state at round end; pausing graph"
                );
                self.graph_tracker.lock().pause_with_message(
                    GoalPauseReason::Infra,
                    format!("Inconsistent goal engine state at round end: {other:?}"),
                );
                self.persist_graph_state();
                None
            }
        }
    }

    /// The `Complete` arm of the seam: harvest the node's cost, archive
    /// its goal artifacts, mark it achieved, and either finish the graph
    /// or launch the next node.
    async fn advance_graph_after_node_complete(&self) -> Option<String> {
        let Some(node_id) = self
            .graph_tracker
            .lock()
            .current_node_id()
            .map(str::to_owned)
        else {
            tracing::error!("graph: goal completed but no current node; pausing graph");
            self.graph_tracker.lock().pause_with_message(
                GoalPauseReason::Infra,
                "Goal completed with no current graph node".to_owned(),
            );
            self.persist_graph_state();
            return None;
        };

        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let node_tokens = self.goal_tokens_used(current_tokens);
        let rounds = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|o| o.total_worker_rounds)
            .unwrap_or(0);
        self.archive_node_artifacts(&node_id).await;
        tracing::info!(%node_id, rounds, node_tokens, "graph: node achieved");
        self.graph_tracker
            .lock()
            .mark_node_achieved(&node_id, rounds, node_tokens);
        self.persist_graph_state();
        self.drive_graph().await
    }

    /// THE single dispatch loop, shared by setup/advance/resume: run
    /// parallel batches while ≥2 nodes are `Ready` (and the cap allows),
    /// then launch the next serial node on the goal engine and return
    /// its reminder — or settle the graph (complete / wedged / paused /
    /// budget) and return `None`. With `graph_concurrency == 1` this
    /// degenerates to exactly the G0 serial behavior.
    pub(super) async fn drive_graph(&self) -> Option<String> {
        loop {
            if !self.graph_tracker.lock().is_active() {
                // Paused/limited during a batch (cancel cascade) or by a
                // serial-launch failure — the pauser already messaged.
                return None;
            }
            if self.graph_tracker.lock().remaining_budget() == Some(0) {
                tracing::warn!("graph: budget exhausted at dispatch");
                self.graph_tracker.lock().budget_limit();
                self.persist_graph_state();
                self.send_slash_command_output(
                    "Graph token budget exhausted. Top up with /graph resume --budget <tokens>, \
                 or /graph clear to abandon.",
                )
                .await;
                return None;
            }
            let ready: Vec<String> = self
                .graph_tracker
                .lock()
                .snapshot()
                .map(|s| {
                    s.nodes
                        .iter()
                        .filter(|n| n.status == NodeStatus::Ready)
                        .map(|n| n.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            if ready.is_empty() {
                if self.graph_tracker.lock().all_achieved() {
                    let (total, objective) = {
                        let mut tracker = self.graph_tracker.lock();
                        tracker.complete();
                        let snapshot = tracker.snapshot();
                        (
                            snapshot.map(|s| s.nodes.len()).unwrap_or(0),
                            snapshot.map(|s| s.objective.clone()).unwrap_or_default(),
                        )
                    };
                    self.persist_graph_state();
                    self.reset_goal_engine_state().await;
                    tracing::info!(total, "graph: complete");
                    self.send_slash_command_output(&format!(
                        "Graph complete: all {total} nodes achieved (final verification \
                         included).\nObjective: {objective}"
                    ))
                    .await;
                } else if self.graph_tracker.lock().is_wedged() {
                    tracing::warn!("graph: wedged (no runnable node, work remaining)");
                    self.graph_tracker.lock().pause_with_message(
                        GoalPauseReason::Verification,
                        "No runnable node left: a dependency chain failed".to_owned(),
                    );
                    self.persist_graph_state();
                    self.send_slash_command_output(
                        "Graph blocked: a dependency chain failed and no runnable node is \
                         left. See /graph status.",
                    )
                    .await;
                } else {
                    tracing::error!("graph: active with no ready node and work remaining");
                    self.graph_tracker.lock().pause_with_message(
                        GoalPauseReason::Infra,
                        "Scheduler found no runnable node while work remains".to_owned(),
                    );
                    self.persist_graph_state();
                    // Never pause invisibly: the user must know the
                    // scheduler stopped and why.
                    self.send_slash_command_output(
                        "Graph paused: scheduler found no runnable node while work \
                         remains. See /graph status.",
                    )
                    .await;
                }
                return None;
            }
            // Parallel batches need per-node worktrees, which need a git
            // repo. Outside one, degrade to serial (the goal engine works
            // anywhere) instead of letting N workers share one tree.
            let parallel_possible = kigi_workspace::session::git::find_git_root_from_path(
                self.tool_context.cwd.as_path(),
            )
            .is_ok();
            let cap = if parallel_possible {
                self.graph_concurrency as usize
            } else {
                1
            };
            if cap <= 1 || ready.len() == 1 {
                return self.launch_next_graph_node().await;
            }
            let batch: Vec<String> = ready.into_iter().take(cap).collect();
            // No goal is Active during a batch, so arm the interrupt
            // gate explicitly: a queued user prompt must stack FIFO
            // behind the batch instead of cancelling the graph turn
            // (the in-turn loop re-syncs the gate from goal status at
            // every round start).
            self.set_goal_loop_active_resource(true).await;
            self.run_graph_parallel_batch(batch).await;
            // Loop: the batch resolved nodes (achieved/failed); recompute
            // and keep driving — another batch, a serial tail (gn-final),
            // or settle.
        }
    }

    /// Copy the node goal's durable artifacts (plan, plan baseline,
    /// strategy note) into the node's archive dir before the engine is
    /// cleared for the next node. Sources come from the ORCHESTRATION's
    /// own claims (`plan_file`/`plan_baseline_file`/`last_strategy_path`),
    /// never from bare path probing — a stale file left by a previous
    /// node must not be archived as this node's work. Best-effort: a
    /// failed copy loses audit detail, never progress — but always logs.
    async fn archive_node_artifacts(&self, node_id: &str) {
        let (sources, dst_dir) = {
            let goal = self.goal_tracker.lock();
            let graph = self.graph_tracker.lock();
            let mut sources: Vec<std::path::PathBuf> = Vec::new();
            if let Some(o) = goal.snapshot() {
                if let Some(p) = &o.plan_file {
                    sources.push(p.clone());
                }
                if let Some(p) = &o.plan_baseline_file {
                    sources.push(p.clone());
                }
                if let Some(p) = &o.last_strategy_path {
                    sources.push(std::path::PathBuf::from(p));
                }
            }
            (sources, graph.node_archive_dir(node_id))
        };
        if sources.is_empty() {
            return;
        }
        if let Err(err) = tokio::fs::create_dir_all(&dst_dir).await {
            tracing::warn!(%node_id, %err, "graph: failed to create node archive dir");
            return;
        }
        for src in sources {
            if !src.is_file() {
                continue;
            }
            let Some(name) = src.file_name() else {
                continue;
            };
            if let Err(err) = tokio::fs::copy(&src, dst_dir.join(name)).await {
                tracing::warn!(%node_id, src = %src.display(), %err, "graph: artifact archive copy failed");
            }
        }
    }

    /// Clear all goal-engine session state (tracker, streaks, task ids,
    /// token records, pending classifier claims) and tell the pager.
    /// Shared by `/goal clear`, graph node boundaries, and `/graph clear`.
    pub(super) async fn reset_goal_engine_state(&self) {
        self.goal_tracker.lock().clear();
        self.goal_continuation_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_blocked_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_turn_task_ids.lock().clear();
        self.subagent_token_records.lock().clear();
        self.clear_pending_classifier_completions();
        let update = crate::session::goal_orchestrator::build_goal_cleared();
        self.send_xai_notification(update).await;
    }

    /// Apply `/graph resume`: re-arm a paused graph. If the current node's
    /// goal is still in the engine and paused, resume it (planner retry
    /// included); otherwise (post-restart empty engine) launch the next
    /// `Ready` node fresh — the verifier gates completion, so re-running
    /// a node is always safe.
    pub(super) async fn resume_graph(&self, extra_budget: Option<i64>) -> GraphSetupOutcome {
        use super::goal_support::GoalResumeOutcome;
        let status = self.graph_tracker.lock().status();
        match status {
            None => GraphSetupOutcome::Message(
                "No graph is set. Use /graph <objective> to start one.".to_owned(),
            ),
            Some(GoalStatus::Active) => {
                GraphSetupOutcome::Message("Graph is already running.".to_owned())
            }
            Some(GoalStatus::Complete) => GraphSetupOutcome::Message(
                "Graph is already complete. Use /graph <objective> to start a new one.".to_owned(),
            ),
            Some(GoalStatus::BudgetLimited) => {
                let Some(extra) = extra_budget else {
                    return GraphSetupOutcome::Message(
                        "Graph is budget-limited. Top up with /graph resume --budget \
                         <tokens>, or /graph clear to abandon."
                            .to_owned(),
                    );
                };
                if !self.graph_tracker.lock().resume_budget_limited(extra) {
                    return GraphSetupOutcome::Message(
                        "Budget top-up must be a positive token count.".to_owned(),
                    );
                }
                self.persist_graph_state();
                tracing::info!(extra, "graph: resumed with budget top-up");
                match self.drive_graph().await {
                    Some(reminder) => GraphSetupOutcome::Inference {
                        reminder,
                        user_msg: format!("Graph resumed with {extra} fresh budget tokens."),
                    },
                    None => GraphSetupOutcome::Message(
                        "Graph resumed with fresh budget but did not enter a serial node. \
                         See /graph status."
                            .to_owned(),
                    ),
                }
            }
            Some(s) if s.is_paused() => {
                if extra_budget.is_some() {
                    // Never silently discard an explicit flag.
                    return GraphSetupOutcome::Message(
                        "--budget only applies to a budget-limited graph; this graph is \
                         paused. Use /graph resume (no flags) to continue, or /graph \
                         clear to abandon."
                            .to_owned(),
                    );
                }
                {
                    let mut tracker = self.graph_tracker.lock();
                    tracker.resume();
                    // A cancel during a parallel batch left its nodes
                    // marked Running with no executor. Demote every
                    // orphaned in-flight node back to Ready, keeping only
                    // the node whose goal actually lives in the engine —
                    // otherwise resume wedges forever (nothing Ready,
                    // nothing achieved, is_wedged false).
                    let keep = self
                        .goal_tracker
                        .lock()
                        .snapshot()
                        .is_some()
                        .then(|| tracker.current_node_id().map(str::to_owned))
                        .flatten();
                    tracker.demote_orphaned_in_flight(keep.as_deref());
                }
                self.persist_graph_state();
                tracing::info!("graph: resumed");
                // Planning never finished? Re-plan before touching nodes.
                let needs_planning = self
                    .graph_tracker
                    .lock()
                    .snapshot()
                    .is_some_and(|s| s.nodes.is_empty());
                if needs_planning {
                    let objective = self
                        .graph_tracker
                        .lock()
                        .objective()
                        .map(str::to_owned)
                        .unwrap_or_default();
                    return match self.finish_planning_on_resume(&objective).await {
                        Ok(Some(reminder)) => GraphSetupOutcome::Inference {
                            reminder,
                            user_msg: "Graph resumed; planning retried.".to_owned(),
                        },
                        Ok(None) => GraphSetupOutcome::Message(
                            "Graph resumed but no node could start. See /graph status.".to_owned(),
                        ),
                        Err(msg) => GraphSetupOutcome::Message(msg),
                    };
                }
                let goal_paused = self
                    .goal_tracker
                    .lock()
                    .status()
                    .is_some_and(|s| s.is_paused());
                if goal_paused {
                    match self.resume_goal().await {
                        GoalResumeOutcome::Inference { reminder, user_msg } => {
                            GraphSetupOutcome::Inference {
                                reminder,
                                user_msg: format!("Graph resumed. {user_msg}"),
                            }
                        }
                        GoalResumeOutcome::Message(msg) => {
                            // The node goal re-paused (e.g. planner failed
                            // again); the cascade re-paused the graph.
                            GraphSetupOutcome::Message(format!("Graph resume: {msg}"))
                        }
                    }
                } else {
                    match self.drive_graph().await {
                        Some(reminder) => GraphSetupOutcome::Inference {
                            reminder,
                            user_msg: "Graph resumed.".to_owned(),
                        },
                        None => GraphSetupOutcome::Message(
                            "Graph resumed but did not enter a serial node. See /graph status."
                                .to_owned(),
                        ),
                    }
                }
            }
            Some(other) => GraphSetupOutcome::Message(format!(
                "Graph is in an unexpected state ({other:?}); use /graph status."
            )),
        }
    }

    /// Resume path for a graph that paused during planning: retry the
    /// planner, install on success, launch the first node.
    async fn finish_planning_on_resume(&self, objective: &str) -> Result<Option<String>, String> {
        match self.run_graph_planning(objective).await {
            Ok(nodes) => {
                if let Err(err) = self.write_graph_baseline(&nodes).await {
                    let reason = format!("failed to write graph baseline: {err}");
                    self.graph_tracker
                        .lock()
                        .pause_with_message(GoalPauseReason::Infra, reason.clone());
                    self.persist_graph_state();
                    return Err(reason);
                }
                self.graph_tracker.lock().install_nodes(nodes);
                self.persist_graph_state();
                Ok(self.drive_graph().await)
            }
            Err(reason) => {
                {
                    let mut tracker = self.graph_tracker.lock();
                    tracker.record_planning_failed(reason.clone());
                    tracker.pause_with_message(
                        GoalPauseReason::Infra,
                        format!("Graph planning failed: {reason}"),
                    );
                }
                self.persist_graph_state();
                Err(format!(
                    "Graph planning failed again: {reason}\nUse /graph resume to retry."
                ))
            }
        }
    }

    /// Render the `/graph status` tree.
    pub(super) async fn graph_status_message(&self) -> String {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let node_goal_tokens = self.goal_tokens_used(current_tokens);
        let tracker = self.graph_tracker.lock();
        let Some(s) = tracker.snapshot() else {
            return "No graph is set. Use /graph <objective> to start one.".to_owned();
        };
        let achieved = s
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Achieved)
            .count();
        let mut buf = format!(
            "Graph: {}\nStatus: {:?} | Phase: {:?} | Plan v{}\nNodes: {achieved}/{} achieved\n",
            s.objective,
            s.status,
            s.phase,
            s.plan_version,
            s.nodes.len(),
        );
        for node in &s.nodes {
            let glyph = match node.status {
                NodeStatus::Achieved => "[x]",
                NodeStatus::Running | NodeStatus::Verifying => "[>]",
                NodeStatus::Ready => "[ ]",
                NodeStatus::Waiting => "[.]",
                NodeStatus::Failed => "[!]",
                NodeStatus::Blocked => "[-]",
            };
            buf.push_str(&format!("  {glyph} {} — {}", node.id, node.title));
            if node.status == NodeStatus::Waiting && !node.deps.is_empty() {
                let deps: Vec<&str> = node.deps.iter().map(|d| d.on.as_str()).collect();
                buf.push_str(&format!("  (waiting on {})", deps.join(", ")));
            }
            if node.status == NodeStatus::Achieved {
                buf.push_str(&format!(
                    "  ({} tokens, {} rounds)",
                    node.tokens_used, node.rounds
                ));
            }
            if let Some(failure) = &node.failure {
                buf.push_str(&format!("  — {failure}"));
            }
            buf.push('\n');
        }
        let mut tokens = s.tokens_spent_nodes;
        if s.current_node.is_some() {
            tokens += node_goal_tokens;
        }
        buf.push_str(&format!("Tokens: {tokens}"));
        if let Some(budget) = s.token_budget {
            buf.push_str(&format!(" | Budget: {budget}"));
        }
        if let Some(node_id) = &s.current_node {
            buf.push_str(&format!("\nCurrent node: {node_id}"));
        }
        if let Some(msg) = &s.pause_message {
            buf.push_str(&format!("\nPaused: {msg}"));
        }
        buf
    }
}
