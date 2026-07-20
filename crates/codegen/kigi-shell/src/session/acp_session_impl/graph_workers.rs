//! Parallel graph-node execution: worker/verifier subagent pairs.
//!
//! In parallel mode (`KIGI_GRAPH_CONCURRENCY > 1` with ≥2 `Ready`
//! nodes) a node does NOT run on the session goal engine — it runs as a
//! harness-internal `general-purpose` subagent (the implementer toolset)
//! in its OWN git worktree, adversarially checked by a read-only
//! verifier subagent, with a bounded worker↔verifier round loop
//! (`graph_node_rounds`). Achieved nodes merge back into the main tree
//! SEQUENTIALLY via `kigi_workspace`'s 3-way `apply_worktree`; a merge
//! conflict fails the node (its dependents block; other chains
//! continue). The terminal `gn-final` node always runs serially on the
//! full goal engine because it depends on every other node.
//!
//! Known ceiling: a worker round that outlives the foreground subagent
//! await budget (default 600s) is cancelled and counted as a failed
//! round with an explicit gap; the next round resumes the same child
//! session. Fetching results from auto-backgrounded children would need
//! completed-store plumbing — deferred until real usage demands it.

use std::sync::Arc;

use kigi_tools::implementations::kigi::task::types::{
    SubagentEvent, SubagentRequest, SubagentRuntimeOverrides,
};

use super::SessionActor;

const WORKER_PROMPT_TEMPLATE: &str = include_str!("../templates/graph_node_worker_prompt.md");
const VERIFIER_PROMPT_TEMPLATE: &str = include_str!("../templates/graph_node_verifier_prompt.md");

// Terminal-contract parsing

/// The worker's parsed claim, from the trailing `NODE_RESULT:` line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkerClaim {
    Done { summary: String },
    Blocked { reason: String },
    Unparseable,
}

/// Drop ``` fenced code blocks so a QUOTED marker (the templates
/// themselves contain fenced `NODE_RESULT:`/`NODE_VERDICT:` examples a
/// child may echo) can never be parsed as the real terminal line.
fn strip_fenced_blocks(output: &str) -> String {
    let mut kept = String::with_capacity(output.len());
    let mut in_fence = false;
    for line in output.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

/// The LAST line that STARTS with `marker` (line-anchored — a
/// mid-sentence mention never matches), plus everything after it.
fn last_marker_line(output: &str, marker: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = output.lines().collect();
    let idx = lines
        .iter()
        .rposition(|l| l.trim_start().trim_start_matches('`').starts_with(marker))?;
    let value = lines[idx]
        .trim_start()
        .trim_start_matches('`')
        .trim_start_matches(marker)
        .trim()
        .trim_matches('`')
        .to_owned();
    let tail = lines[idx + 1..].join("\n").trim().to_owned();
    Some((value, tail))
}

/// Parse the last line-anchored `NODE_RESULT:` marker outside fenced
/// blocks; text after the marker line is the summary/reason.
/// Fail-closed: no marker ⇒ `Unparseable`.
pub(crate) fn parse_worker_claim(output: &str) -> WorkerClaim {
    let stripped = strip_fenced_blocks(output);
    let Some((value, tail)) = last_marker_line(&stripped, "NODE_RESULT:") else {
        return WorkerClaim::Unparseable;
    };
    match value.as_str() {
        "done" => WorkerClaim::Done { summary: tail },
        "blocked" => WorkerClaim::Blocked {
            reason: if tail.is_empty() {
                "no reason given".to_owned()
            } else {
                tail
            },
        },
        _ => WorkerClaim::Unparseable,
    }
}

/// The verifier's parsed verdict, from the trailing `NODE_VERDICT:` line.
/// Fail-closed: anything unparseable is `NotAchieved` with that fact as
/// the gap.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NodeVerdict {
    Achieved,
    NotAchieved { gaps: Vec<String> },
}

pub(crate) fn parse_node_verdict(output: &str) -> NodeVerdict {
    let stripped = strip_fenced_blocks(output);
    let Some((value, tail)) = last_marker_line(&stripped, "NODE_VERDICT:") else {
        return NodeVerdict::NotAchieved {
            gaps: vec!["verifier response lacked a NODE_VERDICT line".to_owned()],
        };
    };
    match value.as_str() {
        "achieved" => NodeVerdict::Achieved,
        "not_achieved" => {
            let gaps: Vec<String> = tail
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && *l != "GAPS:")
                .map(|l| l.trim_start_matches('-').trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect();
            NodeVerdict::NotAchieved {
                gaps: if gaps.is_empty() {
                    vec!["verifier rejected without naming gaps".to_owned()]
                } else {
                    gaps
                },
            }
        }
        other => NodeVerdict::NotAchieved {
            gaps: vec![format!("unrecognized verdict token {other:?}")],
        },
    }
}

// Spawner seam (mockable in tests)

pub(crate) struct WorkerSpawnSpec {
    pub prompt: String,
    pub description: String,
    /// Explicit child cwd (verifiers run in the worker's worktree).
    pub cwd: Option<String>,
    /// Mint an isolated worktree for the child (first worker round).
    pub isolation_worktree: bool,
    /// Resume a prior child session (later worker rounds keep context
    /// AND the worktree).
    pub resume_from: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerSpawnOutcome {
    pub success: bool,
    pub cancelled: bool,
    pub backgrounded: bool,
    pub output: String,
    pub error: Option<String>,
    pub child_session_id: String,
    pub tokens_used: u64,
    pub worktree_path: Option<String>,
}

#[async_trait::async_trait]
pub(crate) trait GraphWorkerSpawner: Send + Sync {
    /// Spawn one child and await its terminal result. `Err` = transport
    /// failure (coordinator gone).
    async fn spawn(&self, id: &str, spec: WorkerSpawnSpec) -> Result<WorkerSpawnOutcome, String>;
    /// Best-effort cancel of a still-running child (budget overrun).
    async fn cancel(&self, subagent_id: &str);
}

/// Production spawner: raw harness-internal `SubagentEvent::Spawn`,
/// exactly the goal-classifier wire (`surface_completion: false`, no
/// fork), plus worktree isolation / cwd override for node work.
pub(crate) struct GraphWorkerChannelSpawner {
    pub event_tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    pub parent_session_id: String,
    pub parent_prompt_id: Option<String>,
}

#[async_trait::async_trait]
impl GraphWorkerSpawner for GraphWorkerChannelSpawner {
    async fn spawn(&self, id: &str, spec: WorkerSpawnSpec) -> Result<WorkerSpawnOutcome, String> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: id.to_string(),
            prompt: spec.prompt,
            description: spec.description,
            // The implementer toolset (full read/edit/bash inventory);
            // verifier read-only-ness is prompt-enforced, same as the
            // goal skeptic panel.
            subagent_type: "general-purpose".to_string(),
            parent_session_id: self.parent_session_id.clone(),
            parent_prompt_id: self.parent_prompt_id.clone(),
            resume_from: spec.resume_from,
            cwd: spec.cwd,
            runtime_overrides: SubagentRuntimeOverrides {
                isolation: spec
                    .isolation_worktree
                    .then_some(kigi_tool_types::SubagentIsolationMode::Worktree),
                ..Default::default()
            },
            run_in_background: false,
            // Harness-internal: never surfaces to the model's idle reminder.
            surface_completion: false,
            fork_context: false,
            result_tx,
        };
        if self
            .event_tx
            .send(SubagentEvent::Spawn(Box::new(request)))
            .is_err()
        {
            return Err("subagent coordinator channel closed".to_owned());
        }
        let result = result_rx
            .await
            .map_err(|_| "subagent result channel dropped".to_owned())?;
        Ok(WorkerSpawnOutcome {
            success: result.success,
            cancelled: result.cancelled,
            backgrounded: result.backgrounded,
            output: result.output.to_string(),
            error: result.error.clone(),
            child_session_id: result.child_session_id.clone(),
            tokens_used: result.tokens_used,
            worktree_path: result.worktree_path.clone(),
        })
    }

    async fn cancel(&self, subagent_id: &str) {
        use kigi_tools::implementations::kigi::task::types::{
            SubagentCancelRequest, SubagentCancelTarget,
        };
        let (respond_to, ack) = tokio::sync::oneshot::channel();
        let _ = self
            .event_tx
            .send(SubagentEvent::Cancel(SubagentCancelRequest {
                target: SubagentCancelTarget::SubagentId(subagent_id.to_string()),
                respond_to,
            }));
        let _ = ack.await;
    }
}

// Per-node bounded closed loop

#[derive(Debug)]
pub(crate) struct NodeRunReport {
    pub node_id: String,
    pub achieved: bool,
    /// Worker summary on success; failure reason otherwise.
    pub detail: String,
    pub rounds: u32,
    pub tokens_used: i64,
    pub worktree_path: Option<String>,
    /// Last worker child session id (audit link, stored on the node).
    pub worker_session_id: Option<String>,
}

fn worker_prompt(node_objective: &str, gaps: &[String]) -> String {
    let mut p = String::with_capacity(WORKER_PROMPT_TEMPLATE.len() + node_objective.len() + 256);
    p.push_str(WORKER_PROMPT_TEMPLATE);
    p.push_str("\n\nNODE OBJECTIVE:\n");
    p.push_str(node_objective);
    if !gaps.is_empty() {
        p.push_str("\n\nGAPS (from the previous verification round — close exactly these):\n");
        for gap in gaps {
            p.push_str("- ");
            p.push_str(gap);
            p.push('\n');
        }
    }
    p
}

fn verifier_prompt(node_objective: &str, worker_summary: &str) -> String {
    // Neutralize terminal-contract tokens in the worker-controlled
    // summary so a lazy/adversarial claim cannot smuggle marker lines
    // into the verifier's context.
    let safe_summary = worker_summary
        .replace("NODE_VERDICT", "NODE-VERDICT")
        .replace("NODE_RESULT", "NODE-RESULT");
    format!(
        "{VERIFIER_PROMPT_TEMPLATE}\n\nNODE OBJECTIVE (the contract to judge):\n{node_objective}\n\n\
         IMPLEMENTER'S CLAIM (audit it, do not trust it):\n{safe_summary}\n"
    )
}

/// Drive one node through bounded worker↔verifier rounds. Never panics;
/// every failure path returns a `NodeRunReport` with a precise reason.
pub(crate) async fn run_node_to_verdict(
    spawner: &Arc<dyn GraphWorkerSpawner>,
    node_id: &str,
    node_objective: &str,
    rounds_cap: u32,
) -> NodeRunReport {
    let mut tokens: i64 = 0;
    let mut gaps: Vec<String> = Vec::new();
    let mut resume_from: Option<String> = None;
    let mut worktree_path: Option<String> = None;
    let mut last_gaps_summary = String::new();

    for round in 1..=rounds_cap {
        let spawn_id = format!("graph-{node_id}-w{round}-{}", uuid::Uuid::now_v7());
        let spec_was_isolated = resume_from.is_none();
        let spec = WorkerSpawnSpec {
            prompt: worker_prompt(node_objective, &gaps),
            description: format!("graph node worker ({node_id})"),
            cwd: None,
            // Fresh worktree only on the first round; resumes reuse it.
            isolation_worktree: spec_was_isolated,
            resume_from: resume_from.clone(),
        };
        tracing::info!(%node_id, round, resumed = resume_from.is_some(), "graph worker: round start");
        let outcome = match spawner.spawn(&spawn_id, spec).await {
            Ok(o) => o,
            Err(err) => {
                return NodeRunReport {
                    node_id: node_id.to_owned(),
                    achieved: false,
                    detail: format!("worker transport failure: {err}"),
                    rounds: round,
                    tokens_used: tokens,
                    worktree_path,
                    worker_session_id: resume_from,
                };
            }
        };
        let round_requested_isolation = spec_was_isolated;
        tokens = tokens.saturating_add(outcome.tokens_used as i64);
        if outcome.worktree_path.is_some() {
            worktree_path = outcome.worktree_path.clone();
        }
        // Adopt-guard: an in-band spawn failure carries an EMPTY child id;
        // adopting it would make the next round a fresh UNISOLATED spawn
        // in the shared tree while verify/merge still target the stale
        // worktree. Keep the last valid id (or None ⇒ re-mint isolation).
        if !outcome.child_session_id.is_empty() {
            resume_from = Some(outcome.child_session_id.clone());
        }

        if outcome.cancelled {
            return NodeRunReport {
                node_id: node_id.to_owned(),
                achieved: false,
                detail: "worker cancelled".to_owned(),
                rounds: round,
                tokens_used: tokens,
                worktree_path,
                worker_session_id: resume_from,
            };
        }
        if outcome.backgrounded {
            // Ceiling (see module doc): cancel the runaway child and
            // burn the round; the resume keeps its context.
            tracing::warn!(%node_id, round, "graph worker: exceeded foreground await budget; cancelling round");
            // Cancel by the SPAWN REQUEST id — the coordinator's cancel
            // maps are keyed by it, not by the child session id.
            spawner.cancel(&spawn_id).await;
            gaps = vec![
                "the previous round exceeded the foreground time budget and was cancelled; \
                 split the remaining work into smaller, faster steps"
                    .to_owned(),
            ];
            last_gaps_summary = gaps.join("; ");
            continue;
        }
        if !outcome.success {
            let err = outcome.error.unwrap_or_else(|| "unknown error".to_owned());
            tracing::warn!(%node_id, round, %err, "graph worker: round failed");
            gaps = vec![format!("the previous round failed with an error: {err}")];
            last_gaps_summary = gaps.join("; ");
            continue;
        }

        // Isolation guard: a SUCCESSFUL first round that came back with
        // no worktree means isolation silently degraded (non-git dir,
        // worktree creation failure, or the snapshot-disposal flag
        // deleted it before we saw it). Running parallel writers in the
        // shared tree — or merging a disposed tree — is never OK.
        if round_requested_isolation && outcome.worktree_path.is_none() {
            return NodeRunReport {
                node_id: node_id.to_owned(),
                achieved: false,
                detail: "worktree isolation unavailable for this node (non-git directory, \
                         worktree creation failure, or KIGI_SUBAGENT_WORKTREE_SNAPSHOT \
                         disposal); parallel execution requires isolation"
                    .to_owned(),
                rounds: round,
                tokens_used: tokens,
                worktree_path: None,
                worker_session_id: resume_from,
            };
        }

        match parse_worker_claim(&outcome.output) {
            WorkerClaim::Blocked { reason } => {
                return NodeRunReport {
                    node_id: node_id.to_owned(),
                    achieved: false,
                    detail: format!("worker reported blocked: {reason}"),
                    rounds: round,
                    tokens_used: tokens,
                    worktree_path,
                    worker_session_id: resume_from,
                };
            }
            WorkerClaim::Unparseable => {
                gaps = vec![
                    "the previous round's final message lacked the required NODE_RESULT line"
                        .to_owned(),
                ];
                last_gaps_summary = gaps.join("; ");
                continue;
            }
            WorkerClaim::Done { summary } => {
                let verify_id = format!("graph-{node_id}-v{round}-{}", uuid::Uuid::now_v7());
                let verify_spec = WorkerSpawnSpec {
                    prompt: verifier_prompt(node_objective, &summary),
                    description: format!("graph node verifier ({node_id})"),
                    // The verifier inspects the worker's worktree.
                    cwd: worktree_path.clone(),
                    isolation_worktree: false,
                    resume_from: None,
                };
                let verdict = match spawner.spawn(&verify_id, verify_spec).await {
                    Ok(v) => {
                        tokens = tokens.saturating_add(v.tokens_used as i64);
                        if v.success {
                            parse_node_verdict(&v.output)
                        } else {
                            // Fail CLOSED: an unverified claim never passes.
                            NodeVerdict::NotAchieved {
                                gaps: vec![format!(
                                    "verifier run failed ({}); the claim is unverified",
                                    v.error.unwrap_or_else(|| "unknown error".to_owned())
                                )],
                            }
                        }
                    }
                    Err(err) => NodeVerdict::NotAchieved {
                        gaps: vec![format!("verifier transport failure: {err}")],
                    },
                };
                match verdict {
                    NodeVerdict::Achieved => {
                        tracing::info!(%node_id, round, tokens, "graph worker: node verified achieved");
                        return NodeRunReport {
                            node_id: node_id.to_owned(),
                            achieved: true,
                            detail: summary,
                            rounds: round,
                            tokens_used: tokens,
                            worktree_path,
                            worker_session_id: resume_from,
                        };
                    }
                    NodeVerdict::NotAchieved { gaps: new_gaps } => {
                        tracing::info!(%node_id, round, gap_count = new_gaps.len(), "graph worker: verifier rejected round");
                        last_gaps_summary = new_gaps.join("; ");
                        gaps = new_gaps;
                    }
                }
            }
        }
    }
    NodeRunReport {
        node_id: node_id.to_owned(),
        achieved: false,
        detail: format!(
            "verification rejected after {rounds_cap} rounds; last gaps: {last_gaps_summary}"
        ),
        rounds: rounds_cap,
        tokens_used: tokens,
        worktree_path,
        worker_session_id: resume_from,
    }
}

/// `git rev-parse HEAD` of `dir`, `None` outside a git repo.
async fn git_head(dir: &std::path::Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

// SessionActor integration

impl SessionActor {
    /// Production worker spawner wired to this session's coordinator.
    fn graph_worker_spawner(&self) -> Option<Arc<dyn GraphWorkerSpawner>> {
        let event_tx = self.tool_context.subagent_event_tx.clone()?;
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        Some(Arc::new(GraphWorkerChannelSpawner {
            event_tx,
            parent_session_id: self.session_id_string(),
            parent_prompt_id,
        }))
    }

    /// Run one parallel batch of `Ready` nodes to their verdicts, then
    /// merge achieved worktrees back SEQUENTIALLY in batch order. All
    /// tracker mutations + persistence happen here; the caller re-reads
    /// the tracker afterwards.
    pub(super) async fn run_graph_parallel_batch(&self, node_ids: Vec<String>) {
        let Some(spawner) = self.graph_worker_spawner() else {
            tracing::error!("graph batch: no subagent coordinator; pausing graph");
            self.graph_tracker.lock().pause_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                "No subagent coordinator available for parallel execution".to_owned(),
            );
            self.persist_graph_state();
            return;
        };
        // Compose objectives + mark Running under one lock pass.
        let mut jobs: Vec<(String, String)> = Vec::with_capacity(node_ids.len());
        {
            let mut tracker = self.graph_tracker.lock();
            let Some(snapshot) = tracker.snapshot() else {
                return;
            };
            let total = snapshot.nodes.len();
            let objective = snapshot.objective.clone();
            for id in &node_ids {
                if let Some(pos) = snapshot.nodes.iter().position(|n| n.id == *id) {
                    let node = &snapshot.nodes[pos];
                    jobs.push((
                        id.clone(),
                        super::graph::node_goal_objective(&objective, node, pos + 1, total),
                    ));
                }
            }
            for (id, _) in &jobs {
                tracker.mark_node_running(id, String::new());
            }
            // `current_node` means "the node on the serial goal engine";
            // batch nodes are tracked by their own Running status.
            if let Some(s) = tracker.snapshot_mut() {
                s.current_node = None;
            }
        }
        self.persist_graph_state();
        // Merge-base integrity: if the main repo HEAD moves during the
        // batch (external commit), apply_worktree would diff against the
        // wrong base and silently reverse-apply those commits. Capture
        // HEAD now; every merge re-checks it.
        let head_at_fanout = git_head(self.tool_context.cwd.as_path()).await;
        let rounds_cap = self.graph_node_rounds;
        tracing::info!(batch = jobs.len(), rounds_cap, "graph batch: fan-out");

        let reports = futures::future::join_all(jobs.iter().map(|(id, objective)| {
            let spawner = spawner.clone();
            async move { run_node_to_verdict(&spawner, id, objective, rounds_cap).await }
        }))
        .await;

        // Sequential merge + tracker resolution in batch order.
        let mut achieved = 0usize;
        let mut failed = 0usize;
        for report in reports {
            // Stamp the worker session id for audit (goal_id slot).
            if let Some(worker_id) = &report.worker_session_id
                && let Some(node) = self
                    .graph_tracker
                    .lock()
                    .snapshot_mut()
                    .and_then(|s| s.nodes.iter_mut().find(|n| n.id == report.node_id))
            {
                node.goal_id = Some(worker_id.clone());
            }
            if !report.achieved {
                failed += 1;
                {
                    let mut tracker = self.graph_tracker.lock();
                    // Budget integrity: a failed node's tokens were still
                    // spent — charge them before failing the node.
                    tracker.charge_node_tokens(&report.node_id, report.tokens_used);
                    tracker.mark_node_failed(&report.node_id, report.detail.clone());
                }
                self.persist_graph_state();
                continue;
            }
            match self
                .merge_node_worktree(
                    &report.node_id,
                    report.worktree_path.as_deref(),
                    head_at_fanout.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    achieved += 1;
                    self.graph_tracker.lock().mark_node_achieved(
                        &report.node_id,
                        report.rounds,
                        report.tokens_used,
                    );
                }
                Err(detail) => {
                    failed += 1;
                    self.graph_tracker
                        .lock()
                        .mark_node_failed(&report.node_id, detail);
                }
            }
            self.persist_graph_state();
        }
        tracing::info!(achieved, failed, "graph batch: settled");
        self.send_slash_command_output(&format!(
            "Graph batch settled: {achieved} node(s) achieved, {failed} failed."
        ))
        .await;
    }

    /// Merge one achieved node's worktree back into the main tree with
    /// the 3-way apply. `None` worktree (isolation soft-fallback) means
    /// the worker already wrote in the shared tree — nothing to merge.
    pub(super) async fn merge_node_worktree(
        &self,
        node_id: &str,
        worktree_path: Option<&str>,
        expected_main_head: Option<&str>,
    ) -> Result<(), String> {
        let Some(worktree_path) = worktree_path else {
            tracing::warn!(
                %node_id,
                "graph merge: worker ran without worktree isolation (soft fallback); nothing to merge"
            );
            return Ok(());
        };
        use kigi_workspace::worktree::{
            ApplyMode, ApplyWorktreeRequest, ApplyWorktreeResponse, apply_worktree,
        };
        // apply_worktree diffs against the main repo HEAD AT APPLY TIME;
        // if HEAD moved since fan-out, that diff would silently
        // reverse-apply the external commits. Fail the node loudly.
        if let Some(expected) = expected_main_head {
            let current = git_head(self.tool_context.cwd.as_path()).await;
            if current.as_deref() != Some(expected) {
                return Err(format!(
                    "main repository HEAD moved during the batch (was {expected}, now {}); \
                     merge aborted for safety — /graph resume re-runs the node",
                    current.as_deref().unwrap_or("unknown")
                ));
            }
        }
        let request = ApplyWorktreeRequest {
            session_id: self.session_id_string(),
            worktree_path: worktree_path.to_owned(),
            mode: ApplyMode::Merge,
        };
        match apply_worktree(&request).await {
            Ok(ApplyWorktreeResponse::Success { files, .. }) => {
                tracing::info!(%node_id, files = files.len(), "graph merge: applied");
                // Storage discipline: the changes now live in the main
                // tree, so the worktree is dead weight — remove it.
                // Best-effort (a failed removal only leaks disk, never
                // progress) but always logged. Failed nodes KEEP their
                // worktree for postmortem.
                if let Err(err) = kigi_workspace::worktree::remove_subagent_worktree(
                    std::path::Path::new(worktree_path),
                )
                .await
                {
                    tracing::warn!(%node_id, %err, "graph merge: worktree cleanup failed");
                }
                Ok(())
            }
            Ok(ApplyWorktreeResponse::Conflicts { conflicts, .. }) => {
                let names: Vec<String> = conflicts.iter().map(|c| c.path.clone()).collect();
                tracing::warn!(%node_id, ?names, "graph merge: conflicts; failing node");
                Err(format!("merge conflict in: {}", names.join(", ")))
            }
            Err(err) => {
                tracing::warn!(%node_id, %err, "graph merge: apply failed");
                Err(format!("worktree apply failed: {err}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_claim_parses_done_blocked_and_garbage() {
        assert_eq!(
            parse_worker_claim("work...\nNODE_RESULT: done\nBuilt X; tests pass."),
            WorkerClaim::Done {
                summary: "Built X; tests pass.".to_owned()
            }
        );
        assert_eq!(
            parse_worker_claim("NODE_RESULT: blocked\nno compiler available"),
            WorkerClaim::Blocked {
                reason: "no compiler available".to_owned()
            }
        );
        assert_eq!(
            parse_worker_claim("all done, promise!"),
            WorkerClaim::Unparseable
        );
        // Last marker wins (a quoted earlier marker cannot spoof).
        assert_eq!(
            parse_worker_claim(
                "NODE_RESULT: done\nold\n...more work...\nNODE_RESULT: blocked\nreal"
            ),
            WorkerClaim::Blocked {
                reason: "real".to_owned()
            }
        );
    }

    #[test]
    fn verdict_parses_achieved_gaps_and_fails_closed() {
        assert_eq!(
            parse_node_verdict("checked\nNODE_VERDICT: achieved"),
            NodeVerdict::Achieved
        );
        assert_eq!(
            parse_node_verdict(
                "NODE_VERDICT: not_achieved\nGAPS:\n- test suite not run\n- claim B unverified"
            ),
            NodeVerdict::NotAchieved {
                gaps: vec![
                    "test suite not run".to_owned(),
                    "claim B unverified".to_owned()
                ]
            }
        );
        assert!(matches!(
            parse_node_verdict("looks good to me"),
            NodeVerdict::NotAchieved { gaps } if gaps[0].contains("lacked a NODE_VERDICT")
        ));
        assert!(matches!(
            parse_node_verdict("NODE_VERDICT: maybe"),
            NodeVerdict::NotAchieved { gaps } if gaps[0].contains("unrecognized verdict")
        ));
        assert!(matches!(
            parse_node_verdict("NODE_VERDICT: not_achieved"),
            NodeVerdict::NotAchieved { gaps } if gaps[0].contains("without naming gaps")
        ));
    }

    #[test]
    fn fenced_template_echo_cannot_spoof_markers() {
        // A worker echoing the template's fenced examples must stay
        // unparseable; only its own line-anchored terminal marker counts.
        let echoed = "Here is my plan:\n```\nNODE_RESULT: done\n```\nstill working...";
        assert_eq!(parse_worker_claim(echoed), WorkerClaim::Unparseable);
        let real = "```\nNODE_RESULT: blocked\n```\n...work...\nNODE_RESULT: done\nall built";
        assert_eq!(
            parse_worker_claim(real),
            WorkerClaim::Done {
                summary: "all built".to_owned()
            }
        );
        // Mid-sentence mention is not a marker (line-anchored scan).
        assert_eq!(
            parse_worker_claim("I will print NODE_RESULT: done when finished"),
            WorkerClaim::Unparseable
        );
        // Same discipline for the verifier.
        assert!(matches!(
            parse_node_verdict("quoting:\n```\nNODE_VERDICT: achieved\n```\nhmm"),
            NodeVerdict::NotAchieved { .. }
        ));
    }

    struct MockSpawner {
        replies: std::sync::Mutex<std::collections::VecDeque<WorkerSpawnOutcome>>,
        specs: std::sync::Mutex<Vec<(String, Option<String>, bool)>>,
        cancels: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl GraphWorkerSpawner for MockSpawner {
        async fn spawn(
            &self,
            id: &str,
            spec: WorkerSpawnSpec,
        ) -> Result<WorkerSpawnOutcome, String> {
            self.specs.lock().unwrap().push((
                id.to_owned(),
                spec.resume_from.clone(),
                spec.isolation_worktree,
            ));
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra spawn"))
        }
        async fn cancel(&self, subagent_id: &str) {
            self.cancels.lock().unwrap().push(subagent_id.to_owned());
        }
    }

    fn outcome(output: &str) -> WorkerSpawnOutcome {
        WorkerSpawnOutcome {
            success: true,
            cancelled: false,
            backgrounded: false,
            output: output.to_owned(),
            error: None,
            child_session_id: "child-1".to_owned(),
            tokens_used: 5,
            worktree_path: Some("/wt".to_owned()),
        }
    }

    #[tokio::test]
    async fn backgrounded_round_cancels_by_spawn_id_and_resumes_next_round() {
        let mut bg = outcome("");
        bg.backgrounded = true;
        let replies = std::collections::VecDeque::from(vec![
            bg,                                     // round 1: budget overrun
            outcome("NODE_RESULT: done\nfinished"), // round 2: worker done
            outcome("NODE_VERDICT: achieved"),      // round 2: verifier
        ]);
        // Keep a concrete handle for assertions; hand the trait object in.
        let mock = Arc::new(MockSpawner {
            replies: std::sync::Mutex::new(replies),
            specs: std::sync::Mutex::new(Vec::new()),
            cancels: std::sync::Mutex::new(Vec::new()),
        });
        let spawner: Arc<dyn GraphWorkerSpawner> = mock.clone();
        let report = run_node_to_verdict(&spawner, "gn-x", "do x", 3).await;
        assert!(report.achieved, "{}", report.detail);
        assert_eq!(report.rounds, 2, "backgrounded round burned, retry won");
        assert_eq!(report.tokens_used, 15, "all three spawns charged");

        let cancels = mock.cancels.lock().unwrap().clone();
        assert_eq!(cancels.len(), 1, "runaway child cancelled once");
        assert!(
            cancels[0].starts_with("graph-gn-x-w1-"),
            "cancel must target the SPAWN REQUEST id (coordinator map key), got {}",
            cancels[0]
        );
        let specs = mock.specs.lock().unwrap().clone();
        assert_eq!(specs.len(), 3);
        assert!(
            specs[0].1.is_none() && specs[0].2,
            "round 1: fresh + isolated"
        );
        assert_eq!(
            specs[1].1.as_deref(),
            Some("child-1"),
            "round 2 resumes the backgrounded child's session"
        );
        assert!(!specs[1].2, "resume never re-mints isolation");
    }

    #[tokio::test]
    async fn empty_child_id_is_never_adopted_as_resume_target() {
        // In-band spawn failure: success=false, child_session_id="".
        let failed = WorkerSpawnOutcome {
            success: false,
            cancelled: false,
            backgrounded: false,
            output: String::new(),
            error: Some("boom".to_owned()),
            child_session_id: String::new(),
            tokens_used: 0,
            worktree_path: None,
        };
        let replies = std::collections::VecDeque::from(vec![
            failed,                                 // round 1: in-band failure
            outcome("NODE_RESULT: done\nfinished"), // round 2: worker (fresh, isolated)
            outcome("NODE_VERDICT: achieved"),      // round 2: verifier
        ]);
        let mock = Arc::new(MockSpawner {
            replies: std::sync::Mutex::new(replies),
            specs: std::sync::Mutex::new(Vec::new()),
            cancels: std::sync::Mutex::new(Vec::new()),
        });
        let spawner: Arc<dyn GraphWorkerSpawner> = mock.clone();
        let report = run_node_to_verdict(&spawner, "gn-y", "do y", 3).await;
        assert!(report.achieved, "{}", report.detail);
        let specs = mock.specs.lock().unwrap().clone();
        assert!(
            specs[1].1.is_none(),
            "an empty child id must NOT be adopted; retry is a fresh spawn"
        );
        assert!(
            specs[1].2,
            "fresh retry re-mints worktree isolation (no unisolated escape)"
        );
    }

    #[tokio::test]
    async fn successful_isolated_round_without_worktree_fails_the_node() {
        let mut no_wt = outcome("NODE_RESULT: done\nfinished");
        no_wt.worktree_path = None;
        let replies = std::collections::VecDeque::from(vec![no_wt]);
        let mock = Arc::new(MockSpawner {
            replies: std::sync::Mutex::new(replies),
            specs: std::sync::Mutex::new(Vec::new()),
            cancels: std::sync::Mutex::new(Vec::new()),
        });
        let spawner: Arc<dyn GraphWorkerSpawner> = mock.clone();
        let report = run_node_to_verdict(&spawner, "gn-z", "do z", 3).await;
        assert!(!report.achieved);
        assert!(
            report.detail.contains("isolation unavailable"),
            "{}",
            report.detail
        );
    }
}
