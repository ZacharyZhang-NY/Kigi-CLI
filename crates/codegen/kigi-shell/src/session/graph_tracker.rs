//! Graph mode state machine.
//!
//! This module contains [`GraphTracker`], a pure state machine (no async
//! I/O) modeled after [`GoalTracker`](super::goal_tracker::GoalTracker).
//! The `SessionActor` owns one `GraphTracker` behind a `Mutex` and calls
//! its methods at the graph orchestration points.
//!
//! Architecture: a graph is a deterministic scheduler over goals. Each
//! node executes as one ordinary goal on the existing goal engine
//! (planner, worker loop, adversarial verifier, budget, pauses), so the
//! agentic loop lives INSIDE the node and the edges between nodes stay
//! deterministic Rust. The graph layer therefore reuses the goal
//! vocabulary — [`GoalStatus`], [`GoalPhase`], [`GoalPauseReason`] — and
//! adds only the DAG bookkeeping.
//!
//! Restore semantics: a snapshot restored from disk can never resurrect
//! as a self-driving graph. `from_snapshot` demotes `Active` to
//! `UserPaused` and any `Running`/`Verifying` node back to `Ready`; the
//! user re-arms with `/graph resume`, which re-launches the node as a
//! fresh goal (the per-node verifier gates completion, so a re-run is
//! always safe).

use std::path::PathBuf;
use std::time::Instant;

use super::goal_tracker::{GoalPauseReason, GoalPhase, GoalStatus};

/// Max retained graph-history entries; oldest dropped past the cap so a
/// long graph's snapshot stays bounded. Mirrors `GOAL_HISTORY_MAX`.
const GRAPH_HISTORY_MAX: usize = 64;

/// Canonical id of the harness-appended terminal verification node. It
/// depends on every planner node and its goal re-verifies the OVERALL
/// objective, closing the composition gap ("every node passed but the
/// whole didn't").
pub const FINAL_NODE_ID: &str = "gn-final";

// Node status / dependency kinds

/// Lifecycle status of one graph node. Single-direction machine:
/// `Waiting -> Ready -> Running -> Achieved`, with `Failed` (node judged
/// unachievable / budget-dead) and `Blocked` (a dependency failed) as
/// terminal side exits. Retries of a node stay in `Running` across
/// rounds — the goal engine owns intra-node iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Waiting,
    Ready,
    Running,
    /// Reserved for the live "verifier in flight" badge (G2); the G0
    /// scheduler never stores it and `from_snapshot` demotes it to
    /// `Ready`.
    Verifying,
    Achieved,
    Failed,
    Blocked,
}

impl<'de> serde::Deserialize<'de> for NodeStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire_str(&s))
    }
}

impl NodeStatus {
    /// Parse a persisted status string. Unknown values map to `Ready`:
    /// a node state this shell cannot interpret must restore as
    /// re-runnable work, never as silently-done or stuck.
    pub fn from_wire_str(s: &str) -> Self {
        match s {
            "waiting" => Self::Waiting,
            "ready" => Self::Ready,
            "running" => Self::Running,
            "verifying" => Self::Verifying,
            "achieved" => Self::Achieved,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => Self::Ready,
        }
    }

    /// Terminal states the scheduler never leaves.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Achieved | Self::Failed | Self::Blocked)
    }
}

/// Dependency edge kind. `Blocks` is the planner-authored ordering
/// dependency and the ONLY kind that gates scheduling.
/// `DiscoveredFrom` marks a node appended by a replan (G3) pointing
/// back at the node whose execution surfaced it — pure audit/render
/// metadata: its origin is always terminal at replan time, so gating on
/// it would either be a no-op (origin Achieved) or a permanent wedge
/// (origin Failed — and a failed node's discoveries are still real
/// work).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    #[default]
    Blocks,
    DiscoveredFrom,
}

/// One dependency edge: this node cannot start until `on` is `Achieved`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeDep {
    pub on: String,
    #[serde(default)]
    pub kind: DepKind,
}

// GraphNode

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    /// Short content-derived id (`gn-<fnv hex>`); stable across replans
    /// of the same node title so cross-machine graph merges (G4) stay
    /// line-mergeable.
    pub id: String,
    pub title: String,
    /// Node-level objective — the core of the node goal's objective
    /// string. Written by the graph planner.
    pub spec: String,
    #[serde(default)]
    pub deps: Vec<NodeDep>,
    pub status: NodeStatus,
    /// Goal id of the node's most recent goal instance (`None` until
    /// first launch). Links the node to the goal engine's own artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    /// Worker rounds the node's goal consumed, recorded at node
    /// completion.
    #[serde(default)]
    pub rounds: u32,
    /// Goal-scoped tokens the node consumed, recorded at node
    /// completion.
    #[serde(default)]
    pub tokens_used: i64,
    /// Short failure detail when `status` is `Failed`/`Blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

// History

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphEvent {
    GraphCreated,
    PlanningStarted,
    PlanningCompleted,
    PlanningFailed,
    NodeStarted,
    NodeAchieved,
    NodeFailed,
    GraphPaused,
    GraphResumed,
    GraphCompleted,
    GraphCleared,
    BudgetExceeded,
    /// Forward-compat sink for history written by a newer shell.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphHistoryEntry {
    pub timestamp: String,
    pub event: GraphEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl GraphHistoryEntry {
    pub(crate) fn now(event: GraphEvent, node_id: Option<String>, detail: Option<String>) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event,
            node_id,
            detail,
        }
    }
}

/// One piece of out-of-scope work surfaced during node execution
/// (`DISCOVERED:` marker from a worker, verifier, or the serial node's
/// final text). Queued until the next replan boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Discovery {
    /// Node whose execution surfaced this work.
    pub from_node: String,
    pub description: String,
}

// GraphOrchestration (full persisted state)

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphOrchestration {
    pub graph_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub phase: GoalPhase,
    /// Monotonic plan version; replans/optimizer passes (G3/G6) bump it.
    /// Version 1 is the initial planner output.
    #[serde(default = "default_plan_version")]
    pub plan_version: u32,
    /// Topological-friendly storage order (planner order + harness
    /// appendix). The scheduler picks the first `Ready` node in this
    /// order, so execution is deterministic. `default` so the project
    /// header line (which omits `nodes`) deserializes.
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    /// Node currently running as the active goal, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// Graph-level token budget; each node goal is armed with the
    /// remaining share so mid-node overruns trip the goal engine's own
    /// enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    /// Tokens consumed by completed node goals (boundary-accumulated).
    #[serde(default)]
    pub tokens_spent_nodes: i64,
    pub history: Vec<GraphHistoryEntry>,
    /// Human-readable reason set on paused/blocked transitions; cleared
    /// on resume/complete (mirrors `GoalOrchestration::pause_message`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_message: Option<String>,
    /// Discoveries queued for the next replan boundary. Drained by a
    /// replan pass; past the replan cap they drain to history only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_discoveries: Vec<Discovery>,
    /// Replan passes consumed (bounded by `KIGI_GRAPH_REPLAN_CAP`).
    #[serde(default)]
    pub replan_runs: u32,
}

fn default_plan_version() -> u32 {
    1
}

// GraphTracker

/// Pure graph-mode state machine owned by the `SessionActor`.
pub struct GraphTracker {
    session_dir: PathBuf,
    state: Option<GraphOrchestration>,
    /// Wall-clock anchor for `account_elapsed`; `Some` only while the
    /// graph is `Active`. Never persisted.
    last_probe: Option<Instant>,
}

impl GraphTracker {
    pub fn new(session_dir: PathBuf) -> Self {
        Self {
            session_dir,
            state: None,
            last_probe: None,
        }
    }

    /// Restore from a persisted snapshot, sanitized so it can never
    /// resurrect self-driving: `Active` demotes to `UserPaused` (the
    /// in-turn loop that drove it is gone) and `Running`/`Verifying`
    /// nodes demote to `Ready` for a fresh, verifier-gated re-run.
    pub fn from_snapshot(session_dir: PathBuf, mut snapshot: GraphOrchestration) -> Self {
        if snapshot.status == GoalStatus::Active {
            snapshot.status = GoalStatus::UserPaused;
            snapshot.pause_message =
                Some("Restored after a restart. Use /graph resume to continue.".to_owned());
        }
        for node in &mut snapshot.nodes {
            if matches!(node.status, NodeStatus::Running | NodeStatus::Verifying) {
                node.status = NodeStatus::Ready;
            }
        }
        snapshot.current_node = None;
        let mut tracker = Self {
            session_dir,
            state: Some(snapshot),
            last_probe: None,
        };
        tracker.recompute_ready();
        tracker
    }

    // Paths

    /// `<session_dir>/graph` — root for graph-owned state (`state.json`
    /// lives here, session-scoped: one current graph per session).
    pub fn graph_dir(&self) -> PathBuf {
        self.session_dir.join("graph")
    }

    /// Per-graph artifact root (`<session_dir>/graph/<graph_id>`), so a
    /// later `/graph` in the same session can never overwrite a prior
    /// graph's frozen baselines or node archives. Falls back to
    /// `graph_dir` when no graph is set (callers always have one).
    pub fn artifacts_dir(&self) -> PathBuf {
        match self.state.as_ref() {
            Some(s) => self.graph_dir().join(&s.graph_id),
            None => self.graph_dir(),
        }
    }

    /// Immutable baseline snapshot for `version`
    /// (`<artifacts_dir>/graph.baseline.v{N}.json`).
    pub fn baseline_path(&self, version: u32) -> PathBuf {
        self.artifacts_dir()
            .join(format!("graph.baseline.v{version}.json"))
    }

    /// Per-node artifact archive dir (`<artifacts_dir>/<node_id>`).
    /// Node-goal artifacts (plan.md, …) are copied here when the node
    /// completes, before the goal engine is cleared for the next node.
    pub fn node_archive_dir(&self, node_id: &str) -> PathBuf {
        self.artifacts_dir().join(node_id)
    }

    // Accessors

    pub fn snapshot(&self) -> Option<&GraphOrchestration> {
        self.state.as_ref()
    }

    pub fn snapshot_mut(&mut self) -> Option<&mut GraphOrchestration> {
        self.state.as_mut()
    }

    pub fn status(&self) -> Option<GoalStatus> {
        self.state.as_ref().map(|s| s.status)
    }

    pub fn is_active(&self) -> bool {
        self.status() == Some(GoalStatus::Active)
    }

    pub fn objective(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.objective.as_str())
    }

    pub fn current_node_id(&self) -> Option<&str> {
        self.state.as_ref()?.current_node.as_deref()
    }

    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.state.as_ref()?.nodes.iter().find(|n| n.id == id)
    }

    /// Remaining graph token budget (`None` when no budget is set).
    /// Saturates at zero.
    pub fn remaining_budget(&self) -> Option<i64> {
        let s = self.state.as_ref()?;
        let budget = s.token_budget?;
        Some((budget - s.tokens_spent_nodes).max(0))
    }

    // Transitions

    /// Create a fresh graph in `Planning` phase with no nodes yet.
    pub fn create_graph(
        &mut self,
        graph_id: String,
        objective: String,
        token_budget: Option<i64>,
        created_at: String,
    ) {
        let mut state = GraphOrchestration {
            graph_id,
            objective,
            status: GoalStatus::Active,
            phase: GoalPhase::Planning,
            plan_version: 1,
            nodes: Vec::new(),
            current_node: None,
            created_at,
            elapsed_ms: 0,
            token_budget,
            tokens_spent_nodes: 0,
            history: Vec::new(),
            pause_message: None,
            pending_discoveries: Vec::new(),
            replan_runs: 0,
        };
        state
            .history
            .push(GraphHistoryEntry::now(GraphEvent::GraphCreated, None, None));
        state.history.push(GraphHistoryEntry::now(
            GraphEvent::PlanningStarted,
            None,
            None,
        ));
        self.state = Some(state);
        self.last_probe = Some(Instant::now());
    }

    /// Install the validated node set (planner output + harness-appended
    /// final node) and move to `Executing`. Roots become `Ready`.
    pub fn install_nodes(&mut self, nodes: Vec<GraphNode>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.nodes = nodes;
        state.phase = GoalPhase::Executing;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::PlanningCompleted, None, None),
        );
        self.recompute_ready();
    }

    /// Record a planning failure in history (the caller pauses the graph
    /// with the canonical message).
    pub fn record_planning_failed(&mut self, detail: String) {
        if let Some(state) = self.state.as_mut() {
            push_history(
                state,
                GraphHistoryEntry::now(GraphEvent::PlanningFailed, None, Some(detail)),
            );
        }
    }

    /// First `Ready` node in storage order — the deterministic serial
    /// scheduling rule.
    pub fn next_ready_node(&self) -> Option<&GraphNode> {
        self.state
            .as_ref()?
            .nodes
            .iter()
            .find(|n| n.status == NodeStatus::Ready)
    }

    /// Mark `id` as launched under goal `goal_id`.
    pub fn mark_node_running(&mut self, id: &str, goal_id: String) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(node) = state.nodes.iter_mut().find(|n| n.id == id) {
            node.status = NodeStatus::Running;
            node.goal_id = Some(goal_id);
        }
        state.current_node = Some(id.to_owned());
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::NodeStarted, Some(id.to_owned()), None),
        );
    }

    /// Mark `id` achieved with its consumed rounds/tokens, clear the
    /// current-node pointer, and unlock dependents.
    pub fn mark_node_achieved(&mut self, id: &str, rounds: u32, tokens_used: i64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(node) = state.nodes.iter_mut().find(|n| n.id == id) {
            node.status = NodeStatus::Achieved;
            node.rounds = rounds;
            node.tokens_used = tokens_used;
        }
        state.tokens_spent_nodes = state.tokens_spent_nodes.saturating_add(tokens_used);
        state.current_node = None;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::NodeAchieved, Some(id.to_owned()), None),
        );
        self.recompute_ready();
    }

    /// Mark `id` failed with `detail`, clear the current-node pointer,
    /// and block every transitive dependent. The caller decides the
    /// graph-level consequence (pause/block).
    pub fn mark_node_failed(&mut self, id: &str, detail: String) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(node) = state.nodes.iter_mut().find(|n| n.id == id) {
            node.status = NodeStatus::Failed;
            node.failure = Some(detail.clone());
        }
        state.current_node = None;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::NodeFailed, Some(id.to_owned()), Some(detail)),
        );
        block_dependents(state, id);
    }

    /// All nodes `Achieved` — the graph's success condition.
    pub fn all_achieved(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            !s.nodes.is_empty() && s.nodes.iter().all(|n| n.status == NodeStatus::Achieved)
        })
    }

    /// True when no node can make progress: nothing `Ready`/`Running`
    /// and at least one node is not `Achieved`.
    pub fn is_wedged(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            !s.nodes.is_empty()
                && !s.nodes.iter().all(|n| n.status == NodeStatus::Achieved)
                && !s
                    .nodes
                    .iter()
                    .any(|n| matches!(n.status, NodeStatus::Ready | NodeStatus::Running))
        })
    }

    /// `Active -> paused-family`; `true` if the transition happened.
    pub fn pause(&mut self, reason: GoalPauseReason) -> bool {
        self.pause_inner(reason, None)
    }

    /// Like [`Self::pause`] but records a human-readable reason.
    pub fn pause_with_message(&mut self, reason: GoalPauseReason, message: String) -> bool {
        self.pause_inner(reason, Some(message))
    }

    fn pause_inner(&mut self, reason: GoalPauseReason, message: Option<String>) -> bool {
        self.account_elapsed();
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state.status != GoalStatus::Active {
            return false;
        }
        state.status = reason.to_status();
        state.pause_message = message;
        push_history(
            state,
            GraphHistoryEntry::now(
                GraphEvent::GraphPaused,
                state.current_node.clone(),
                Some(reason.history_detail().to_owned()),
            ),
        );
        self.last_probe = None;
        true
    }

    /// `paused-family -> Active`; `true` if the transition happened.
    /// Recomputes the ready set so a resume after restore re-arms roots.
    pub fn resume(&mut self) -> bool {
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if !state.status.is_paused() {
            return false;
        }
        state.status = GoalStatus::Active;
        state.pause_message = None;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::GraphResumed, None, None),
        );
        self.last_probe = Some(Instant::now());
        self.recompute_ready();
        true
    }

    /// `Active -> Complete`; `true` if the transition happened.
    pub fn complete(&mut self) -> bool {
        self.account_elapsed();
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state.status != GoalStatus::Active {
            return false;
        }
        state.status = GoalStatus::Complete;
        state.pause_message = None;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::GraphCompleted, None, None),
        );
        self.last_probe = None;
        true
    }

    /// `Active -> BudgetLimited`; `true` if the transition happened.
    /// Every in-flight node (the serial engine node AND any parallel
    /// batch node) demotes to `Ready`: a budget trip is a resource
    /// stop, not a verdict on the node — so no forever-`Running` node
    /// is ever persisted, and a later budget top-up
    /// ([`Self::resume_budget_limited`]) re-dispatches it naturally.
    pub fn budget_limit(&mut self) -> bool {
        self.account_elapsed();
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state.status != GoalStatus::Active {
            return false;
        }
        let in_flight = state.current_node.clone();
        for node in &mut state.nodes {
            if matches!(node.status, NodeStatus::Running | NodeStatus::Verifying) {
                node.status = NodeStatus::Ready;
            }
        }
        state.current_node = None;
        state.status = GoalStatus::BudgetLimited;
        state.pause_message = None;
        push_history(
            state,
            GraphHistoryEntry::now(GraphEvent::BudgetExceeded, in_flight, None),
        );
        self.last_probe = None;
        true
    }

    /// `BudgetLimited -> Active` with `extra` fresh headroom: the new
    /// budget becomes spent-so-far + extra. `true` if applied.
    pub fn resume_budget_limited(&mut self, extra: i64) -> bool {
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state.status != GoalStatus::BudgetLimited || extra <= 0 {
            return false;
        }
        state.token_budget = Some(state.tokens_spent_nodes.saturating_add(extra));
        state.status = GoalStatus::Active;
        state.pause_message = None;
        push_history(
            state,
            GraphHistoryEntry::now(
                GraphEvent::GraphResumed,
                None,
                Some(format!("budget topped up by {extra}")),
            ),
        );
        self.last_probe = Some(Instant::now());
        self.recompute_ready();
        true
    }

    /// Drop all graph state (history records the clear first so a
    /// final persisted snapshot, if any, carries it).
    pub fn clear(&mut self) {
        if let Some(state) = self.state.as_mut() {
            push_history(
                state,
                GraphHistoryEntry::now(GraphEvent::GraphCleared, None, None),
            );
        }
        self.state = None;
        self.last_probe = None;
    }

    /// Fold the wall-clock delta since the last probe into
    /// `elapsed_ms`. No-op unless `Active`.
    pub fn account_elapsed(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.status != GoalStatus::Active {
            return;
        }
        let now = Instant::now();
        if let Some(probe) = self.last_probe {
            state.elapsed_ms = state
                .elapsed_ms
                .saturating_add(now.duration_since(probe).as_millis() as u64);
        }
        self.last_probe = Some(now);
    }

    pub fn append_history(&mut self, entry: GraphHistoryEntry) {
        if let Some(state) = self.state.as_mut() {
            push_history(state, entry);
        }
    }

    /// Queue discoveries for the next replan boundary (records each in
    /// history so the audit trail survives even past the replan cap).
    pub fn queue_discoveries(&mut self, discoveries: Vec<Discovery>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        for d in discoveries {
            push_history(
                state,
                GraphHistoryEntry::now(
                    GraphEvent::Unknown,
                    Some(d.from_node.clone()),
                    Some(format!("discovered: {}", d.description)),
                ),
            );
            state.pending_discoveries.push(d);
        }
    }

    /// Install a validated replan appendix: bump `plan_version`, append
    /// the new nodes, gate the terminal node on them too (demoting it
    /// back to `Waiting` if it was already `Ready`), consume the pending
    /// discoveries, and recompute readiness. SGH discipline: versions
    /// are immutable — existing nodes are never touched here (the
    /// validator guarantees the appendix references them read-only).
    pub fn append_replan_nodes(&mut self, new_nodes: Vec<GraphNode>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let new_ids: Vec<String> = new_nodes.iter().map(|n| n.id.clone()).collect();
        state.nodes.extend(new_nodes);
        if let Some(final_node) = state.nodes.iter_mut().find(|n| n.id == FINAL_NODE_ID) {
            for id in &new_ids {
                if id != FINAL_NODE_ID && !final_node.deps.iter().any(|d| &d.on == id) {
                    final_node.deps.push(NodeDep {
                        on: id.clone(),
                        kind: DepKind::Blocks,
                    });
                }
            }
            // A Ready terminal node whose gate just grew must wait again.
            if final_node.status == NodeStatus::Ready {
                final_node.status = NodeStatus::Waiting;
            }
        }
        state.plan_version += 1;
        state.replan_runs += 1;
        state.pending_discoveries.clear();
        push_history(
            state,
            GraphHistoryEntry::now(
                GraphEvent::PlanningCompleted,
                None,
                Some(format!(
                    "replan v{}: +{} node(s)",
                    state.plan_version,
                    new_ids.len()
                )),
            ),
        );
        self.recompute_ready();
    }

    /// Install an optimizer-transformed node set: bump `plan_version`,
    /// consume a shared replan-cap slot, recompute readiness. The
    /// validator already guaranteed immutable nodes are untouched.
    pub fn install_optimized_nodes(&mut self, nodes: Vec<GraphNode>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.nodes = nodes;
        state.plan_version += 1;
        state.replan_runs += 1;
        push_history(
            state,
            GraphHistoryEntry::now(
                GraphEvent::PlanningCompleted,
                None,
                Some(format!("optimized to v{}", state.plan_version)),
            ),
        );
        self.recompute_ready();
    }

    /// Drain pending discoveries WITHOUT replanning (cap exhausted):
    /// they stay in history (queued there at capture time) only.
    pub fn drain_discoveries_to_history(&mut self) -> usize {
        let Some(state) = self.state.as_mut() else {
            return 0;
        };
        let n = state.pending_discoveries.len();
        state.pending_discoveries.clear();
        n
    }

    /// Demote in-flight (`Running`/`Verifying`) nodes whose executor is
    /// gone back to `Ready`, keeping `keep` (the node whose goal still
    /// lives in the engine, if any). Used by the IN-SESSION resume path:
    /// a cancel during a parallel batch aborts the batch future after
    /// nodes were marked Running, and — unlike a restart, where
    /// `from_snapshot` sanitizes — nothing else would ever demote them,
    /// wedging every subsequent resume. Re-running is safe by design
    /// (verifier-gated).
    pub fn demote_orphaned_in_flight(&mut self, keep: Option<&str>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        for node in &mut state.nodes {
            if matches!(node.status, NodeStatus::Running | NodeStatus::Verifying)
                && keep != Some(node.id.as_str())
            {
                node.status = NodeStatus::Ready;
            }
        }
        state.current_node = keep.map(str::to_owned);
        self.recompute_ready();
    }

    /// Charge a node's spend against the graph budget and stamp it on
    /// the node, independent of verdict — a FAILED node's tokens were
    /// still spent (budget integrity).
    pub fn charge_node_tokens(&mut self, id: &str, tokens: i64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Some(node) = state.nodes.iter_mut().find(|n| n.id == id) {
            node.tokens_used = tokens;
        }
        state.tokens_spent_nodes = state.tokens_spent_nodes.saturating_add(tokens);
    }

    /// Promote every `Waiting` node whose deps are all `Achieved` to
    /// `Ready`.
    pub fn recompute_ready(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let achieved: std::collections::HashSet<String> = state
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Achieved)
            .map(|n| n.id.clone())
            .collect();
        for node in &mut state.nodes {
            if node.status == NodeStatus::Waiting
                && node
                    .deps
                    .iter()
                    .filter(|d| d.kind == DepKind::Blocks)
                    .all(|d| achieved.contains(&d.on))
            {
                node.status = NodeStatus::Ready;
            }
        }
    }
}

/// Append with the history cap (oldest dropped).
fn push_history(state: &mut GraphOrchestration, entry: GraphHistoryEntry) {
    state.history.push(entry);
    if state.history.len() > GRAPH_HISTORY_MAX {
        let overflow = state.history.len() - GRAPH_HISTORY_MAX;
        state.history.drain(..overflow);
    }
}

/// Mark every transitive dependent of `failed_id` as `Blocked` (only
/// non-terminal nodes; an already-achieved dependent stays achieved).
fn block_dependents(state: &mut GraphOrchestration, failed_id: &str) {
    let mut blocked: std::collections::HashSet<String> = std::collections::HashSet::new();
    blocked.insert(failed_id.to_owned());
    // Fixed-point pass; node count is small (planner-capped), so the
    // quadratic sweep is simpler than building an adjacency index.
    loop {
        let mut changed = false;
        for node in &mut state.nodes {
            if node.status.is_terminal() || blocked.contains(&node.id) {
                continue;
            }
            if node
                .deps
                .iter()
                .filter(|d| d.kind == DepKind::Blocks)
                .any(|d| blocked.contains(&d.on))
            {
                node.status = NodeStatus::Blocked;
                node.failure = Some(format!("blocked: dependency chain failed at {failed_id}"));
                blocked.insert(node.id.clone());
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, deps: &[&str]) -> GraphNode {
        GraphNode {
            id: id.to_owned(),
            title: id.to_owned(),
            spec: format!("do {id}"),
            deps: deps
                .iter()
                .map(|d| NodeDep {
                    on: (*d).to_owned(),
                    kind: DepKind::Blocks,
                })
                .collect(),
            status: NodeStatus::Waiting,
            goal_id: None,
            rounds: 0,
            tokens_used: 0,
            failure: None,
        }
    }

    fn tracker_with(nodes: Vec<GraphNode>) -> GraphTracker {
        let mut t = GraphTracker::new(std::env::temp_dir());
        t.create_graph(
            "g1".into(),
            "objective".into(),
            None,
            "2026-07-20T00:00:00Z".into(),
        );
        t.install_nodes(nodes);
        t
    }

    #[test]
    fn install_promotes_roots_to_ready_in_storage_order() {
        let t = tracker_with(vec![node("a", &[]), node("b", &["a"]), node("c", &[])]);
        assert_eq!(t.node("a").unwrap().status, NodeStatus::Ready);
        assert_eq!(t.node("b").unwrap().status, NodeStatus::Waiting);
        assert_eq!(t.node("c").unwrap().status, NodeStatus::Ready);
        // Deterministic serial rule: first Ready in storage order.
        assert_eq!(t.next_ready_node().unwrap().id, "a");
    }

    #[test]
    fn achieved_unlocks_dependents_and_accumulates_tokens() {
        let mut t = tracker_with(vec![node("a", &[]), node("b", &["a"])]);
        t.mark_node_running("a", "goal-1".into());
        assert_eq!(t.current_node_id(), Some("a"));
        t.mark_node_achieved("a", 3, 1_000);
        assert_eq!(t.current_node_id(), None);
        assert_eq!(t.node("b").unwrap().status, NodeStatus::Ready);
        assert_eq!(t.snapshot().unwrap().tokens_spent_nodes, 1_000);
        t.mark_node_running("b", "goal-2".into());
        t.mark_node_achieved("b", 1, 500);
        assert!(t.all_achieved());
        assert_eq!(t.snapshot().unwrap().tokens_spent_nodes, 1_500);
    }

    #[test]
    fn failed_node_blocks_transitive_dependents_only() {
        let mut t = tracker_with(vec![
            node("a", &[]),
            node("b", &["a"]),
            node("c", &["b"]),
            node("d", &[]),
        ]);
        t.mark_node_running("a", "goal-1".into());
        t.mark_node_failed("a", "unachievable".into());
        assert_eq!(t.node("a").unwrap().status, NodeStatus::Failed);
        assert_eq!(t.node("b").unwrap().status, NodeStatus::Blocked);
        assert_eq!(t.node("c").unwrap().status, NodeStatus::Blocked);
        // Independent chain keeps going.
        assert_eq!(t.node("d").unwrap().status, NodeStatus::Ready);
        assert!(!t.is_wedged());
        t.mark_node_running("d", "goal-2".into());
        t.mark_node_achieved("d", 1, 10);
        assert!(t.is_wedged(), "no runnable node left, one chain dead");
        assert!(!t.all_achieved());
    }

    #[test]
    fn remaining_budget_saturates_and_tracks_node_spend() {
        let mut t = GraphTracker::new(std::env::temp_dir());
        t.create_graph("g".into(), "o".into(), Some(1_000), "t".into());
        t.install_nodes(vec![node("a", &[])]);
        assert_eq!(t.remaining_budget(), Some(1_000));
        t.mark_node_running("a", "goal-1".into());
        t.mark_node_achieved("a", 1, 1_500);
        assert_eq!(t.remaining_budget(), Some(0), "saturates at zero");
    }

    #[test]
    fn restore_demotes_active_and_running_for_safe_resume() {
        let mut t = tracker_with(vec![node("a", &[]), node("b", &["a"])]);
        t.mark_node_running("a", "goal-1".into());
        let snapshot = t.snapshot().unwrap().clone();
        let restored = GraphTracker::from_snapshot(std::env::temp_dir(), snapshot);
        let s = restored.snapshot().unwrap();
        assert_eq!(s.status, GoalStatus::UserPaused);
        assert!(
            s.pause_message
                .as_deref()
                .unwrap()
                .contains("/graph resume")
        );
        assert_eq!(s.current_node, None);
        assert_eq!(
            restored.node("a").unwrap().status,
            NodeStatus::Ready,
            "running node re-runs, verifier gates completion"
        );
    }

    #[test]
    fn pause_resume_round_trip_recomputes_ready() {
        let mut t = tracker_with(vec![node("a", &[]), node("b", &["a"])]);
        assert!(t.pause(GoalPauseReason::User));
        assert_eq!(t.status(), Some(GoalStatus::UserPaused));
        assert!(!t.pause(GoalPauseReason::User), "pause is Active-only");
        // Simulate an externally-restored snapshot where `a` is already
        // Achieved but `b` was never promoted: resume() must recompute.
        if let Some(s) = t.snapshot_mut() {
            s.nodes[0].status = NodeStatus::Achieved;
        }
        assert_eq!(t.node("b").unwrap().status, NodeStatus::Waiting);
        assert!(t.resume());
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert_eq!(
            t.node("b").unwrap().status,
            NodeStatus::Ready,
            "resume must promote unlocked Waiting nodes"
        );
        assert_eq!(t.next_ready_node().unwrap().id, "b");
    }

    #[test]
    fn budget_limit_demotes_in_flight_and_top_up_resumes() {
        let mut t = GraphTracker::new(std::env::temp_dir());
        t.create_graph("g".into(), "o".into(), Some(10), "t".into());
        t.install_nodes(vec![node("a", &[]), node("b", &["a"])]);
        t.mark_node_running("a", "goal-1".into());
        t.charge_node_tokens("a", 12);
        assert!(t.budget_limit());
        assert_eq!(t.status(), Some(GoalStatus::BudgetLimited));
        let s = t.snapshot().unwrap();
        assert_eq!(
            s.nodes[0].status,
            NodeStatus::Ready,
            "budget trip is a resource stop, not a node verdict — no \
             forever-Running node, runnable again after a top-up"
        );
        assert_eq!(s.current_node, None);

        assert!(!t.resume_budget_limited(0), "top-up must be positive");
        assert!(t.resume_budget_limited(100));
        assert_eq!(t.status(), Some(GoalStatus::Active));
        assert_eq!(
            t.snapshot().unwrap().token_budget,
            Some(112),
            "new budget = spent so far + extra headroom"
        );
        assert_eq!(t.remaining_budget(), Some(100));
        assert_eq!(t.next_ready_node().unwrap().id, "a");
    }

    #[test]
    fn unknown_node_status_restores_as_ready() {
        assert_eq!(NodeStatus::from_wire_str("half_done"), NodeStatus::Ready);
        assert_eq!(NodeStatus::from_wire_str("achieved"), NodeStatus::Achieved);
    }

    #[test]
    fn discovered_from_edges_never_gate_scheduling() {
        let mut t = tracker_with(vec![node("a", &[]), node("b", &[])]);
        t.mark_node_running("a", "g".into());
        t.mark_node_failed("a", "dead".into());
        // Appendix node whose ONLY edge is DiscoveredFrom on the failed
        // origin: must become Ready (audit edge, not a gate) and must
        // not be swept by block_dependents.
        t.append_replan_nodes(vec![GraphNode {
            id: "gn-doc".into(),
            title: "Docs".into(),
            spec: "s".into(),
            deps: vec![NodeDep {
                on: "a".into(),
                kind: DepKind::DiscoveredFrom,
            }],
            status: NodeStatus::Waiting,
            goal_id: None,
            rounds: 0,
            tokens_used: 0,
            failure: None,
        }]);
        assert_eq!(t.node("gn-doc").unwrap().status, NodeStatus::Ready);
    }

    #[test]
    fn replan_appendix_regates_the_final_node_and_bumps_version() {
        let mut t = tracker_with(vec![node("a", &[]), node(FINAL_NODE_ID, &["a"])]);
        t.mark_node_running("a", "g1".into());
        t.mark_node_achieved("a", 1, 10);
        // Final node unlocked…
        assert_eq!(t.node(FINAL_NODE_ID).unwrap().status, NodeStatus::Ready);
        assert_eq!(t.snapshot().unwrap().plan_version, 1);
        // …then a replan appendix lands: final must wait again.
        t.queue_discoveries(vec![Discovery {
            from_node: "a".into(),
            description: "docs".into(),
        }]);
        t.append_replan_nodes(vec![node("docs", &[])]);
        let s = t.snapshot().unwrap();
        assert_eq!(s.plan_version, 2);
        assert_eq!(s.replan_runs, 1);
        assert!(s.pending_discoveries.is_empty());
        let final_node = t.node(FINAL_NODE_ID).unwrap();
        assert_eq!(
            final_node.status,
            NodeStatus::Waiting,
            "a Ready terminal node whose gate grew must wait again"
        );
        assert!(final_node.deps.iter().any(|d| d.on == "docs"));
        assert_eq!(t.next_ready_node().unwrap().id, "docs");
    }

    #[test]
    fn history_is_capped_dropping_the_oldest() {
        let mut t = tracker_with(vec![node("a", &[])]);
        for i in 0..(GRAPH_HISTORY_MAX + 10) {
            t.append_history(GraphHistoryEntry::now(
                GraphEvent::Unknown,
                None,
                Some(format!("e{i}")),
            ));
        }
        let history = &t.snapshot().unwrap().history;
        assert_eq!(history.len(), GRAPH_HISTORY_MAX);
        // Setup pushed 3 entries (created/planning-started/completed) and
        // the loop 74 more; overflow 13 drops the setup entries + e0..e9,
        // so the oldest survivor is e10 and the newest is e73.
        assert_eq!(history.first().unwrap().detail.as_deref(), Some("e10"));
        assert_eq!(history.last().unwrap().detail.as_deref(), Some("e73"));
    }
}
