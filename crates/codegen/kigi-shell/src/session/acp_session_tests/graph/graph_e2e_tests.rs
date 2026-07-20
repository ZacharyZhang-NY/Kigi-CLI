//! End-to-end coverage for the `/graph` orchestration seam: serial
//! multi-node execution over the goal engine, restore/resume, budget and
//! pause cascades, goal-command mutual exclusion, and planning-failure
//! handling. Same single-thread + LocalSet + coordinator-stub pattern as
//! the goal e2e suites; the classifier is disabled so a node completes
//! on the classifier-disabled fast path.

use super::support::*;
use super::*;
use crate::session::goal_tracker::{GoalPauseReason, GoalStatus};
use crate::session::graph_tracker::{GraphTracker, NodeStatus};
use kigi_tools::implementations::kigi::task::types::{SubagentEvent, SubagentResult};
use kigi_tools::implementations::kigi::update_goal::{UpdateGoalInput, envelope_for_test};
use serial_test::serial;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrd};
use tempfile::TempDir;

const ENV_FLAG: &str = "KIGI_GOAL_CLASSIFIER";

/// A valid 3-node chain a → b → c (the harness appends `gn-final`).
fn chain_graph_json() -> Vec<u8> {
    serde_json::json!({
        "nodes": [
            {"id": "a", "title": "Node A", "spec": "do a", "deps": []},
            {"id": "b", "title": "Node B", "spec": "do b", "deps": ["a"]},
            {"id": "c", "title": "Node C", "spec": "do c", "deps": ["b"]},
        ]
    })
    .to_string()
    .into_bytes()
}

/// Self-dependency — fails static validation.
fn invalid_graph_json() -> Vec<u8> {
    br#"{"nodes":[{"id":"a","title":"A","spec":"s","deps":["a"]}]}"#.to_vec()
}

/// Coordinator stub for the GRAPH planner: on each `Spawn`, pops the
/// next body from the FIFO, writes it to the `graph.json` path parsed
/// out of the rendered prompt, and answers `Done`. Panics if spawned
/// more times than bodies were provided (a silent extra spawn would
/// hide a retry-loop bug).
fn spawn_graph_planner_coordinator(
    bodies: Vec<Vec<u8>>,
) -> (
    tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    StdArc<AtomicUsize>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let spawn_count = StdArc::new(AtomicUsize::new(0));
    let count_task = StdArc::clone(&spawn_count);
    tokio::task::spawn_local(async move {
        let mut bodies = std::collections::VecDeque::from(bodies);
        while let Some(ev) = rx.recv().await {
            if let SubagentEvent::Spawn(req) = ev {
                let n = count_task.fetch_add(1, SeqOrd::SeqCst);
                let body = bodies
                    .pop_front()
                    .unwrap_or_else(|| panic!("unexpected planner spawn #{}", n + 1));
                let graph_path = req.prompt.find("/graph.json").map(|end_idx| {
                    let end = end_idx + "/graph.json".len();
                    let start = req.prompt[..end_idx]
                        .rfind(|c: char| !c.is_ascii_graphic() || c == '`')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    req.prompt[start..end].to_string()
                });
                let p = graph_path.expect("graph planner prompt must embed the graph.json path");
                std::fs::create_dir_all(std::path::Path::new(&p).parent().unwrap()).unwrap();
                std::fs::write(&p, &body).unwrap();
                let _ = req.result_tx.send(SubagentResult {
                    success: true,
                    output: StdArc::from("Done"),
                    subagent_id: req.id.clone(),
                    child_session_id: req.id.clone(),
                    ..Default::default()
                });
            }
        }
    });
    (tx, spawn_count)
}

/// Actor with the graph harness fully armed: goal harness on, graph flag
/// on, per-node goal planner OFF (nodes need no plan file in these
/// tests), classifier disabled via `ENV_FLAG=0` at each test site.
async fn make_graph_actor(
    coordinator_tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
) -> (
    SessionActor,
    TempDir,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (mut actor, tmp, rx) = make_graph_actor_detached().await;
    actor.tool_context.subagent_event_tx = Some(coordinator_tx);
    (actor, tmp, rx)
}

/// Like [`make_graph_actor`] but with NO coordinator attached — used by
/// parallel tests whose coordinator needs the repo path (`tmp.path()`)
/// before it can mint worktrees.
async fn make_graph_actor_detached() -> (
    SessionActor,
    TempDir,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<kigi_acp_lib::AcpClientMessage>();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    // Parallel batches require a real git repo (worktree isolation +
    // merge-back); serial paths tolerate it. One empty commit suffices.
    init_git_repo(tmp.path());
    actor.tool_context.cwd = kigi_paths::AbsPathBuf::new(tmp.path().to_path_buf()).unwrap();
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_planner_enabled = false;
    actor.goal_tracker = Arc::new(parking_lot::Mutex::new(
        crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
    ));
    actor.graph_enabled = true;
    actor.graph_tracker = Arc::new(parking_lot::Mutex::new(GraphTracker::new(
        tmp.path().to_path_buf(),
    )));
    (actor, tmp, persistence_rx)
}

/// Drain every pending persistence message and return the payloads of
/// the `GraphModeState` ones, in order.
fn drain_graph_persistence(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) -> Vec<Option<crate::session::graph_tracker::GraphOrchestration>> {
    let mut states = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let PersistenceMsg::GraphModeState(state) = msg {
            states.push(state);
        }
    }
    states
}

/// Feed one `update_goal(completed: true)` claim and run the turn-end
/// drain — with the classifier disabled this lands the node goal in
/// `Complete` (the graph seam is then driven explicitly by each test).
async fn drive_node_goal_to_complete(actor: &SessionActor) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *actor.goal_update_rx.borrow_mut() = Some(rx);
    tx.send(envelope_for_test(UpdateGoalInput {
        completed: Some(true),
        message: Some("node done".into()),
        blocked_reason: None,
    }))
    .expect("send envelope");
    drop(tx);
    actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
    assert_eq!(
        actor.goal_tracker.lock().status(),
        Some(GoalStatus::Complete),
        "classifier-disabled completion must land the node goal in Complete",
    );
}

fn node_statuses(actor: &SessionActor) -> Vec<(String, NodeStatus)> {
    actor
        .graph_tracker
        .lock()
        .snapshot()
        .map(|s| s.nodes.iter().map(|n| (n.id.clone(), n.status)).collect())
        .unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn graph_set_executes_all_nodes_serially_and_completes() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, spawn_count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, mut persistence_rx) = make_graph_actor(coord_tx).await;

            // /graph <objective> — plans, installs a→b→c+final, launches a.
            let outcome = actor.setup_graph("ship the widget", None).await;
            let reminder = match outcome {
                graph::GraphSetupOutcome::Inference { reminder, .. } => reminder,
                graph::GraphSetupOutcome::Message(msg) => panic!("expected Inference, got: {msg}"),
            };
            assert!(
                reminder.contains("Graph node 1/4"),
                "first node reminder must carry graph position: {reminder}"
            );
            assert!(
                reminder.contains("do a"),
                "node spec in objective: {reminder}"
            );
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1, "one planner spawn");
            {
                let statuses = node_statuses(&actor);
                assert_eq!(statuses.len(), 4, "3 planner nodes + final");
                assert_eq!(statuses[0].1, NodeStatus::Running);
                assert_eq!(statuses[1].1, NodeStatus::Waiting);
                assert_eq!(statuses[3].0, crate::session::graph_tracker::FINAL_NODE_ID);
            }
            assert_eq!(actor.goal_tracker.lock().status(), Some(GoalStatus::Active));

            // Seed a node-1 plan artifact so the archive path is exercised
            // (the per-node planner is off in this fixture).
            let plan_path = actor.goal_tracker.lock().plan_path();
            std::fs::create_dir_all(plan_path.parent().unwrap()).unwrap();
            std::fs::write(&plan_path, "NODE1-PLAN").unwrap();
            if let Some(o) = actor.goal_tracker.lock().snapshot_mut() {
                o.plan_file = Some(plan_path.clone());
            }
            let node1_id = actor
                .graph_tracker
                .lock()
                .current_node_id()
                .unwrap()
                .to_owned();

            // Drive all four nodes through complete → seam → next.
            for expected_next in ["Graph node 2/4", "Graph node 3/4", "Graph node 4/4"] {
                drive_node_goal_to_complete(&actor).await;
                let next = actor.run_graph_round_end().await;
                let next = next
                    .unwrap_or_else(|| panic!("expected next-node reminder for {expected_next}"));
                assert!(
                    next.contains(expected_next),
                    "expected {expected_next} in: {next}"
                );
                assert_eq!(
                    actor.goal_tracker.lock().status(),
                    Some(GoalStatus::Active),
                    "fresh node goal must be active"
                );
            }
            // Final node completes → graph Complete, engine cleared, turn ends.
            drive_node_goal_to_complete(&actor).await;
            let end = actor.run_graph_round_end().await;
            assert!(end.is_none(), "graph finished — turn must end");
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Complete)
            );
            assert!(
                actor.goal_tracker.lock().snapshot().is_none(),
                "goal engine must be cleared after graph completion"
            );
            assert!(
                node_statuses(&actor)
                    .iter()
                    .all(|(_, s)| *s == NodeStatus::Achieved),
                "every node achieved: {:?}",
                node_statuses(&actor)
            );
            // Immutable baseline v1 is the pristine pre-execution plan.
            let baseline = actor.graph_tracker.lock().baseline_path(1);
            let frozen: Vec<crate::session::graph_tracker::GraphNode> =
                serde_json::from_slice(&std::fs::read(&baseline).unwrap()).unwrap();
            assert_eq!(frozen.len(), 4);
            assert!(
                frozen.iter().all(|n| n.status == NodeStatus::Waiting),
                "baseline must snapshot the plan BEFORE execution"
            );
            assert_eq!(frozen[3].id, crate::session::graph_tracker::FINAL_NODE_ID);

            // Node-1 goal artifacts were archived before the engine reset.
            let archived = actor
                .graph_tracker
                .lock()
                .node_archive_dir(&node1_id)
                .join("plan.md");
            assert_eq!(
                std::fs::read_to_string(&archived).unwrap(),
                "NODE1-PLAN",
                "node artifacts must be archived before the engine resets"
            );

            // Every transition was checkpointed; the last snapshot is the
            // completed graph with node 1 achieved.
            let states = drain_graph_persistence(&mut persistence_rx);
            assert!(
                states.len() >= 5,
                "one checkpoint per transition, got {}",
                states.len()
            );
            let last = states.last().unwrap().as_ref().expect("last state is Some");
            assert_eq!(last.status, GoalStatus::Complete);
            assert!(
                last.nodes.iter().all(|n| n.status == NodeStatus::Achieved),
                "persisted final snapshot must show all nodes achieved"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn mid_turn_completion_reaches_the_seam_via_the_goal_inactive_break() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", None).await;

            // Classifier disabled: a MID-turn drain applies the completion
            // immediately (the fast path runs before the MidTurn deferral),
            // flipping the goal out of Active mid-round.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(envelope_for_test(UpdateGoalInput {
                completed: Some(true),
                message: None,
                blocked_reason: None,
            }))
            .unwrap();
            drop(tx);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::Complete),
                "classifier-disabled fast path completes mid-turn"
            );

            // The in-turn loop's !goal_active break now consults the seam:
            // the graph must advance to node 2 instead of stranding Active.
            let next = actor
                .run_graph_round_end()
                .await
                .expect("seam must advance the graph after a mid-turn completion");
            assert!(next.contains("Graph node 2/4"), "{next}");
            assert_eq!(node_statuses(&actor)[0].1, NodeStatus::Achieved);
            assert_eq!(node_statuses(&actor)[1].1, NodeStatus::Running);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn graph_over_a_complete_goal_replaces_it_cleanly() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            // A standalone goal driven to Complete stays in the engine.
            let _ = actor.setup_goal("old standalone goal", None).await;
            let old_goal_id = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .goal_id
                .clone();
            drive_node_goal_to_complete(&actor).await;
            // The Complete goal does not block /graph: the per-node engine
            // reset scrubs it before node 1 launches.
            match actor.setup_graph("ship the widget", None).await {
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    assert!(reminder.contains("Graph node 1/4"), "{reminder}");
                }
                graph::GraphSetupOutcome::Message(msg) => {
                    panic!("expected Inference over a Complete goal, got: {msg}")
                }
            }
            let new_goal_id = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .goal_id
                .clone();
            assert_ne!(old_goal_id, new_goal_id, "node 1 must run as a NEW goal");
            assert_eq!(actor.goal_tracker.lock().status(), Some(GoalStatus::Active));
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn graph_restore_demotes_running_node_and_resume_relaunches_it() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", None).await;
            // Node a achieved, node b running.
            drive_node_goal_to_complete(&actor).await;
            let _ = actor.run_graph_round_end().await;
            let snapshot = actor.graph_tracker.lock().snapshot().cloned().unwrap();

            // Simulate a process restart: restore into a fresh actor
            // whose goal engine is EMPTY (goal state does not survive).
            let (coord_tx2, _count2) = spawn_graph_planner_coordinator(vec![]);
            let (mut restored, tmp2, _rx2) = make_graph_actor(coord_tx2).await;
            restored.graph_tracker = Arc::new(parking_lot::Mutex::new(
                GraphTracker::from_snapshot(tmp2.path().to_path_buf(), snapshot),
            ));
            assert_eq!(
                restored.graph_tracker.lock().status(),
                Some(GoalStatus::UserPaused),
                "restore must demote Active to UserPaused"
            );
            let statuses = node_statuses(&restored);
            assert_eq!(statuses[0].1, NodeStatus::Achieved, "a stays achieved");
            assert_eq!(
                statuses[1].1,
                NodeStatus::Ready,
                "running b demotes to Ready for a verifier-gated re-run"
            );

            // /graph resume relaunches node b as a fresh goal.
            match restored.resume_graph(None).await {
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    assert!(
                        reminder.contains("Graph node 2/4"),
                        "resume must relaunch node b: {reminder}"
                    );
                }
                graph::GraphSetupOutcome::Message(msg) => {
                    panic!("expected Inference on resume, got: {msg}")
                }
            }
            assert_eq!(
                restored.graph_tracker.lock().status(),
                Some(GoalStatus::Active)
            );
            assert_eq!(node_statuses(&restored)[1].1, NodeStatus::Running);
            assert_eq!(
                restored.goal_tracker.lock().status(),
                Some(GoalStatus::Active),
                "node b runs as a fresh goal"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn node_budget_is_graph_remaining_and_trip_cascades() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, mut persistence_rx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", Some(500)).await;
            assert_eq!(
                actor.goal_tracker.lock().token_budget(),
                Some(500),
                "node goal must be armed with the remaining graph budget"
            );
            // Trip the goal-side enforcement — the graph must trip with it.
            let tripped = actor.enforce_goal_token_budget(1_000_000).await;
            assert!(tripped, "spend past the budget must trip");
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::BudgetLimited)
            );
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::BudgetLimited),
                "node budget trip IS the graph budget trip"
            );
            // The in-flight node demotes to Ready — a budget trip is a
            // resource stop, not a node verdict; a top-up re-runs it.
            assert_eq!(node_statuses(&actor)[0].1, NodeStatus::Ready);
            assert_eq!(actor.graph_tracker.lock().current_node_id(), None);

            // A terminal graph no longer owns the engine: the user may
            // start a standalone /goal, and /graph clear must NOT destroy it.
            assert!(!actor.graph_owns_goal_engine());
            let _ = actor.setup_goal("fresh standalone goal", None).await;
            assert_eq!(actor.goal_tracker.lock().status(), Some(GoalStatus::Active));
            let actor = StdArc::new(actor);
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GraphClear)
                .await;
            assert!(actor.graph_tracker.lock().snapshot().is_none());
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::Active),
                "/graph clear on a terminal graph must not touch an unrelated goal"
            );
            let states = drain_graph_persistence(&mut persistence_rx);
            assert!(
                matches!(states.last(), Some(None)),
                "/graph clear must persist the tombstone (None) last"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_auto_pause_cascades_to_graph_at_the_chokepoint() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", None).await;
            let paused = actor
                .auto_pause_goal_if_active_with_message(
                    GoalPauseReason::Infra,
                    "sampler exploded".to_owned(),
                )
                .await;
            assert!(paused);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::InfraPaused)
            );
            let graph_status = actor.graph_tracker.lock().status();
            assert_eq!(
                graph_status,
                Some(GoalStatus::InfraPaused),
                "goal pause must cascade to the owning graph"
            );
            let msg = actor
                .graph_tracker
                .lock()
                .snapshot()
                .and_then(|s| s.pause_message.clone())
                .unwrap_or_default();
            assert!(
                msg.contains("gn-"),
                "graph pause message names the node: {msg}"
            );
            // The seam must NOT relaunch anything on a paused graph.
            assert!(actor.run_graph_round_end().await.is_none());
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_commands_are_refused_while_graph_owns_the_engine() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, mut persistence_rx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", None).await;
            let actor = StdArc::new(actor);
            assert!(actor.graph_owns_goal_engine());

            // /goal pause and /goal clear must be refused, leaving the
            // node goal untouched.
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalPause)
                .await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::Active),
                "/goal pause must be refused while a graph owns the engine"
            );
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalClear)
                .await;
            assert!(
                actor.goal_tracker.lock().snapshot().is_some(),
                "/goal clear must be refused while a graph owns the engine"
            );

            // /graph pause pauses BOTH the graph and the node goal.
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GraphPause)
                .await;
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::UserPaused)
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(GoalStatus::UserPaused),
                "/graph pause must stop the node goal too"
            );

            // /graph clear drops both trackers.
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GraphClear)
                .await;
            assert!(actor.graph_tracker.lock().snapshot().is_none());
            assert!(actor.goal_tracker.lock().snapshot().is_none());
            let states = drain_graph_persistence(&mut persistence_rx);
            assert!(
                matches!(states.last(), Some(None)),
                "/graph clear must persist the tombstone (None) last"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn planning_invalid_twice_pauses_and_resume_replans() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Both attempts write a self-dep graph → validation fails twice.
            let (coord_tx, spawn_count) =
                spawn_graph_planner_coordinator(vec![invalid_graph_json(), invalid_graph_json()]);
            let (mut actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            match actor.setup_graph("ship the widget", None).await {
                graph::GraphSetupOutcome::Message(msg) => {
                    assert!(
                        msg.contains("failed validation twice"),
                        "precise failure reason surfaces: {msg}"
                    );
                }
                graph::GraphSetupOutcome::Inference { .. } => {
                    panic!("invalid plan must not reach inference")
                }
            }
            assert_eq!(
                spawn_count.load(SeqOrd::SeqCst),
                2,
                "exactly one validation retry"
            );
            assert!(
                actor
                    .graph_tracker
                    .lock()
                    .status()
                    .is_some_and(|s| s.is_paused()),
                "planning failure pauses the graph"
            );

            // /graph resume re-plans; a now-valid artifact launches node 1.
            let (good_tx, _good_count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            actor.tool_context.subagent_event_tx = Some(good_tx);
            match actor.resume_graph(None).await {
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    assert!(reminder.contains("Graph node 1/4"), "{reminder}");
                }
                graph::GraphSetupOutcome::Message(msg) => {
                    panic!("expected planning retry to succeed on resume: {msg}")
                }
            }
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Active)
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn graph_status_renders_glyphs_deps_tokens_and_pause() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![chain_graph_json()]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            let _ = actor.setup_graph("ship the widget", Some(9_000)).await;
            // Node a achieved (with cost), node b running, c waiting on b.
            drive_node_goal_to_complete(&actor).await;
            let _ = actor.run_graph_round_end().await;
            {
                let mut tracker = actor.graph_tracker.lock();
                let s = tracker.snapshot_mut().unwrap();
                s.nodes[0].tokens_used = 1_000;
                s.nodes[0].rounds = 3;
            }
            let status = actor.graph_status_message().await;
            assert!(status.contains("Graph: ship the widget"), "{status}");
            assert!(status.contains("Nodes: 1/4 achieved"), "{status}");
            assert!(status.contains("(1000 tokens, 3 rounds)"), "{status}");
            assert!(status.contains("[x]"), "achieved glyph: {status}");
            assert!(status.contains("[>]"), "running glyph: {status}");
            assert!(status.contains("[.]"), "waiting glyph: {status}");
            assert!(status.contains("(waiting on "), "dep rendering: {status}");
            assert!(status.contains("| Budget: 9000"), "{status}");
            assert!(status.contains("Current node: "), "{status}");

            // Pause: message line appears.
            let _ = actor
                .auto_pause_goal_if_active_with_message(
                    GoalPauseReason::Infra,
                    "sampler exploded".to_owned(),
                )
                .await;
            let paused = actor.graph_status_message().await;
            assert!(paused.contains("Paused: "), "{paused}");
            assert!(paused.contains("sampler exploded"), "{paused}");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── G1: parallel fan-out ────────────────────────────────────────────

/// One captured harness spawn, for post-hoc assertions.
#[derive(Debug, Clone)]
struct CapturedSpawn {
    prompt: String,
    resume_from: Option<String>,
    cwd: Option<String>,
    isolation_worktree: bool,
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_git_repo(dir: &std::path::Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join(".gitkeep"), "x").unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-qm", "base"]);
}

/// Scripted coordinator: the closure decides each spawn's reply from
/// the request; every spawn is captured. Also tracks the max number of
/// WORKER spawns in flight at once (reply to the first worker is held
/// until a second worker arrives when `require_two_workers` is set —
/// proving genuine fan-out, not sequential dispatch).
fn spawn_scripted_coordinator(
    repo: std::path::PathBuf,
    mut script: impl FnMut(&kigi_tools::implementations::kigi::task::types::SubagentRequest) -> String
    + 'static,
    require_two_workers: bool,
) -> (
    tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    StdArc<std::sync::Mutex<Vec<CapturedSpawn>>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let captured: StdArc<std::sync::Mutex<Vec<CapturedSpawn>>> =
        StdArc::new(std::sync::Mutex::new(Vec::new()));
    let cap_task = StdArc::clone(&captured);
    tokio::task::spawn_local(async move {
        let mut held: Option<(tokio::sync::oneshot::Sender<SubagentResult>, SubagentResult)> = None;
        while let Some(ev) = rx.recv().await {
            if let SubagentEvent::Spawn(req) = ev {
                cap_task.lock().unwrap().push(CapturedSpawn {
                    prompt: req.prompt.clone(),
                    resume_from: req.resume_from.clone(),
                    cwd: req.cwd.clone(),
                    isolation_worktree: matches!(
                        req.runtime_overrides.isolation,
                        Some(kigi_tool_types::SubagentIsolationMode::Worktree)
                    ),
                });
                let output = script(&req);
                let is_worker = req.prompt.contains("Graph Node Worker");
                // Honor the isolation contract with a REAL worktree so the
                // round-1 guard and the merge-back run the true path
                // (empty diff ⇒ apply Success ⇒ worktree cleanup).
                let worktree_path = (is_worker
                    && matches!(
                        req.runtime_overrides.isolation,
                        Some(kigi_tool_types::SubagentIsolationMode::Worktree)
                    ))
                .then(|| {
                    let wt = repo.join(format!("wt-{}", req.id));
                    run_git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
                    wt.to_string_lossy().into_owned()
                });
                let result = SubagentResult {
                    success: true,
                    output: StdArc::from(output.as_str()),
                    subagent_id: req.id.clone(),
                    child_session_id: req.id.clone(),
                    tokens_used: 10,
                    worktree_path,
                    ..Default::default()
                };
                if require_two_workers && is_worker && held.is_none() {
                    // Hold the first worker's reply until the second
                    // worker spawn arrives — join_all must have BOTH in
                    // flight for this to make progress (fan-out proof).
                    held = Some((req.result_tx, result));
                    continue;
                }
                if is_worker && let Some((held_tx, held_result)) = held.take() {
                    let _ = held_tx.send(held_result);
                }
                let _ = req.result_tx.send(result);
            }
        }
    });
    (tx, captured)
}

fn diamond_graph_json() -> Vec<u8> {
    serde_json::json!({
        "nodes": [
            {"id": "a", "title": "Node A", "spec": "do a", "deps": []},
            {"id": "b", "title": "Node B", "spec": "do b", "deps": []},
            {"id": "c", "title": "Node C", "spec": "do c", "deps": ["a", "b"]},
        ]
    })
    .to_string()
    .into_bytes()
}

/// Route a scripted reply by spawn kind: planner writes the DAG, workers
/// claim done, verifiers approve.
fn happy_reply(
    req: &kigi_tools::implementations::kigi::task::types::SubagentRequest,
    dag: &[u8],
) -> String {
    if req.prompt.contains("Graph Plan Writer") {
        let path = req
            .prompt
            .find("/graph.json")
            .map(|end| {
                let end = end + "/graph.json".len();
                let start = req.prompt[..end - "/graph.json".len()]
                    .rfind(|c: char| !c.is_ascii_graphic() || c == '`')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                req.prompt[start..end].to_string()
            })
            .expect("planner prompt embeds path");
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, dag).unwrap();
        "Done".to_owned()
    } else if req.prompt.contains("Graph Node Worker") {
        "NODE_RESULT: done\nImplemented per spec; checks run.".to_owned()
    } else {
        "NODE_VERDICT: achieved".to_owned()
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn parallel_batch_fans_out_then_serial_tail_completes() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        local.run_until(async {
            let dag = diamond_graph_json();
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| happy_reply(req, &dag),
                true,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;

            // Roots a+b run as a parallel batch; c is the serial tail.
            let outcome = actor.setup_graph("build the diamond", None).await;
            let reminder = match outcome {
                graph::GraphSetupOutcome::Inference { reminder, .. } => reminder,
                graph::GraphSetupOutcome::Message(msg) => panic!("expected Inference: {msg}"),
            };
            assert!(
                reminder.contains("Node C"),
                "serial tail must be node c: {reminder}"
            );
            let statuses = node_statuses(&actor);
            assert_eq!(statuses[0].1, NodeStatus::Achieved, "a via batch");
            assert_eq!(statuses[1].1, NodeStatus::Achieved, "b via batch");
            assert_eq!(statuses[2].1, NodeStatus::Running, "c on the goal engine");
            {
                let caps = captured.lock().unwrap();
                let workers: Vec<_> = caps
                    .iter()
                    .filter(|c| c.prompt.contains("Graph Node Worker"))
                    .collect();
                let verifiers: Vec<_> = caps
                    .iter()
                    .filter(|c| c.prompt.contains("Graph Node Verifier"))
                    .collect();
                assert_eq!(workers.len(), 2, "one worker per batch node");
                assert_eq!(verifiers.len(), 2, "one verifier per claim");
                assert!(
                    workers.iter().all(|w| w.isolation_worktree),
                    "first worker rounds must request worktree isolation"
                );
                // The held-reply gate above proves both were in flight
                // concurrently, or this test would have hung.
            }
            // Worker session ids stamped for audit.
            assert!(
                actor
                    .graph_tracker
                    .lock()
                    .node(&crate::session::graph_plan::node_id_for_slug("a"))
                    .unwrap()
                    .goal_id
                    .is_some(),
                "worker session id must be recorded on the node"
            );

            // Finish c and gn-final serially (G0 machinery).
            for _ in 0..2 {
                drive_node_goal_to_complete(&actor).await;
                let _ = actor.run_graph_round_end().await;
            }
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Complete)
            );
        }),
    )
    .await
    .expect("parallel batch did not fan out (held-reply gate starved)");
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn verifier_rejection_iterates_worker_with_resume_and_gaps() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dag = diamond_graph_json();
            let mut a_verify_rejections = 0u32;
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| {
                    if req.prompt.contains("Graph Plan Writer") {
                        return happy_reply(req, &dag);
                    }
                    if req.prompt.contains("Graph Node Worker") {
                        return "NODE_RESULT: done\nwork done.".to_owned();
                    }
                    // Verifier: reject node A's FIRST attempt only.
                    if req.prompt.contains("do a") && a_verify_rejections == 0 {
                        a_verify_rejections += 1;
                        return "NODE_VERDICT: not_achieved\nGAPS:\n- tests were not run"
                            .to_owned();
                    }
                    "NODE_VERDICT: achieved".to_owned()
                },
                false,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            let _ = actor.setup_graph("build the diamond", None).await;

            let a_id = crate::session::graph_plan::node_id_for_slug("a");
            let a = actor.graph_tracker.lock().node(&a_id).cloned().unwrap();
            assert_eq!(a.status, NodeStatus::Achieved);
            assert_eq!(a.rounds, 2, "one rejection ⇒ two worker rounds");

            let caps = captured.lock().unwrap();
            let a_workers: Vec<_> = caps
                .iter()
                .filter(|c| c.prompt.contains("Graph Node Worker") && c.prompt.contains("do a"))
                .collect();
            assert_eq!(a_workers.len(), 2);
            assert!(a_workers[0].resume_from.is_none());
            assert!(
                a_workers[1].resume_from.is_some(),
                "round 2 must resume the round-1 child session"
            );
            assert!(
                !a_workers[1].isolation_worktree,
                "resume keeps the existing worktree; no fresh isolation"
            );
            assert!(
                a_workers[1].prompt.contains("tests were not run"),
                "round 2 must carry the verifier's gaps"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn blocked_worker_fails_node_and_blocks_dependent_chain() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dag = diamond_graph_json();
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, _captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| {
                    if req.prompt.contains("Graph Plan Writer") {
                        return happy_reply(req, &dag);
                    }
                    if req.prompt.contains("Graph Node Worker") {
                        if req.prompt.contains("do a") {
                            return "NODE_RESULT: blocked\nimpossible in this environment"
                                .to_owned();
                        }
                        return "NODE_RESULT: done\nok".to_owned();
                    }
                    "NODE_VERDICT: achieved".to_owned()
                },
                false,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            match actor.setup_graph("build the diamond", None).await {
                graph::GraphSetupOutcome::Message(_) => {}
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    panic!("wedged graph must not reach inference: {reminder}")
                }
            }
            let statuses = node_statuses(&actor);
            assert_eq!(statuses[0].1, NodeStatus::Failed, "a blocked by worker");
            assert_eq!(statuses[1].1, NodeStatus::Achieved, "b unaffected");
            assert_eq!(statuses[2].1, NodeStatus::Blocked, "c depends on a");
            assert_eq!(statuses[3].1, NodeStatus::Blocked, "final depends on all");
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Blocked),
                "wedged graph pauses as Blocked"
            );
            let a = actor
                .graph_tracker
                .lock()
                .node(&crate::session::graph_plan::node_id_for_slug("a"))
                .cloned()
                .unwrap();
            assert!(
                a.failure.as_deref().unwrap().contains("impossible"),
                "{:?}",
                a.failure
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn merge_conflict_on_real_worktree_fails_the_node() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("git unavailable; skipping merge-conflict test");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            fn git(dir: &std::path::Path, args: &[&str]) {
                let out = std::process::Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t")
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let repo = TempDir::new().unwrap();
            git(repo.path(), &["init", "-q", "-b", "main"]);
            std::fs::write(repo.path().join("f.txt"), "base\n").unwrap();
            git(repo.path(), &["add", "."]);
            git(repo.path(), &["commit", "-qm", "base"]);
            let wt = repo.path().join("wt");
            git(
                repo.path(),
                &["worktree", "add", "-q", wt.to_str().unwrap()],
            );
            // Conflicting edits: main tree and worktree both diverge from base.
            std::fs::write(repo.path().join("f.txt"), "ours\n").unwrap();
            std::fs::write(wt.join("f.txt"), "theirs\n").unwrap();

            let (coord_tx, _count) = spawn_graph_planner_coordinator(vec![]);
            let (actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            let err = actor
                .merge_node_worktree("gn-test", Some(wt.to_str().unwrap()), None)
                .await
                .expect_err("conflicting edits must surface as a merge failure");
            assert!(err.contains("f.txt"), "conflict names the file: {err}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn parallel_budget_gate_trips_between_batches_and_charges_all_nodes() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dag = diamond_graph_json();
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, _captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| {
                    if req.prompt.contains("Graph Plan Writer") {
                        return happy_reply(req, &dag);
                    }
                    if req.prompt.contains("Graph Node Worker") {
                        // Node A succeeds; node B claims blocked (fails) —
                        // BOTH must charge the budget.
                        if req.prompt.contains("do b") {
                            return "NODE_RESULT: blocked\nnope".to_owned();
                        }
                        return "NODE_RESULT: done\nok".to_owned();
                    }
                    "NODE_VERDICT: achieved".to_owned()
                },
                false,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            // Stub charges 10 tokens per spawn: batch = worker a + verifier a
            // + worker b (blocked, no verifier) = 30 > budget 25 → the gate
            // trips at the next dispatch iteration.
            match actor.setup_graph("build the diamond", Some(25)).await {
                graph::GraphSetupOutcome::Message(_) => {}
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    panic!("budget-dead graph must not reach inference: {reminder}")
                }
            }
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::BudgetLimited),
                "inter-batch gate must trip"
            );
            let s = actor.graph_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                s.tokens_spent_nodes, 30,
                "achieved (20) AND failed (10) nodes must both charge the budget"
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn cap_trims_batch_and_leftover_root_goes_serial() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let triple_root = serde_json::json!({
        "nodes": [
            {"id": "a", "title": "Node A", "spec": "do a", "deps": []},
            {"id": "b", "title": "Node B", "spec": "do b", "deps": []},
            {"id": "c", "title": "Node C", "spec": "do c", "deps": []},
        ]
    })
    .to_string()
    .into_bytes();
    let local = tokio::task::LocalSet::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        local.run_until(async {
            let dag = triple_root;
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| happy_reply(req, &dag),
                true,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            let outcome = actor.setup_graph("three roots", None).await;
            // Batch 1 = {a, b} (cap-trimmed); leftover root c is the sole
            // Ready node afterwards → serial launch on the goal engine.
            let reminder = match outcome {
                graph::GraphSetupOutcome::Inference { reminder, .. } => reminder,
                graph::GraphSetupOutcome::Message(msg) => panic!("expected Inference: {msg}"),
            };
            assert!(reminder.contains("Node C"), "{reminder}");
            let workers = captured
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.prompt.contains("Graph Node Worker"))
                .count();
            assert_eq!(workers, 2, "take(cap) must trim the third root");
            assert_eq!(node_statuses(&actor)[2].1, NodeStatus::Running, "c serial");
        }),
    )
    .await
    .expect("cap-trim batch starved");
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn resume_after_cancelled_batch_demotes_orphaned_running_nodes() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dag = diamond_graph_json();
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, _captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| happy_reply(req, &dag),
                false,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            let _ = actor.setup_graph("build the diamond", None).await;

            // Simulate an Esc mid-batch: nodes a+b marked Running with no
            // executor (batch future dropped), cascade paused the graph.
            {
                let mut tracker = actor.graph_tracker.lock();
                if let Some(s) = tracker.snapshot_mut() {
                    s.nodes[0].status = NodeStatus::Running;
                    s.nodes[1].status = NodeStatus::Running;
                    s.current_node = None;
                }
            }
            actor.reset_goal_engine_state().await;
            actor
                .graph_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);

            // /graph resume must demote the orphans and re-dispatch — NOT
            // wedge-pause with "no runnable node".
            match actor.resume_graph(None).await {
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    assert!(reminder.contains("Node C"), "{reminder}");
                }
                graph::GraphSetupOutcome::Message(msg) => {
                    panic!("resume must re-dispatch orphaned batch nodes, got: {msg}")
                }
            }
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Active)
            );
            let statuses = node_statuses(&actor);
            assert_eq!(statuses[0].1, NodeStatus::Achieved, "a re-ran via batch");
            assert_eq!(statuses[1].1, NodeStatus::Achieved, "b re-ran via batch");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn batch_merges_real_worktrees_and_cleans_them_up() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("git unavailable; skipping real-worktree batch merge test");
        return;
    }
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            fn git(dir: &std::path::Path, args: &[&str]) {
                let out = std::process::Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t")
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let repo = TempDir::new().unwrap();
            git(repo.path(), &["init", "-q", "-b", "main"]);
            std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
            git(repo.path(), &["add", "."]);
            git(repo.path(), &["commit", "-qm", "base"]);
            let wt_a = repo.path().join("wt_a");
            let wt_b = repo.path().join("wt_b");
            git(
                repo.path(),
                &["worktree", "add", "-q", wt_a.to_str().unwrap()],
            );
            git(
                repo.path(),
                &["worktree", "add", "-q", wt_b.to_str().unwrap()],
            );
            // Disjoint node outputs.
            std::fs::write(wt_a.join("a_out.txt"), "from a\n").unwrap();
            std::fs::write(wt_b.join("b_out.txt"), "from b\n").unwrap();

            let dag = diamond_graph_json();
            let wt_a_str = wt_a.to_str().unwrap().to_owned();
            let wt_b_str = wt_b.to_str().unwrap().to_owned();
            // Scripted coordinator that returns REAL worktree paths on
            // worker results (bypasses spawn_scripted_coordinator's
            // default-None worktree_path).
            let (coord_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
            tokio::task::spawn_local(async move {
                while let Some(ev) = rx.recv().await {
                    if let SubagentEvent::Spawn(req) = ev {
                        let (output, wt): (String, Option<String>) = if req
                            .prompt
                            .contains("Graph Plan Writer")
                        {
                            let path = req
                                .prompt
                                .find("/graph.json")
                                .map(|end| {
                                    let end = end + "/graph.json".len();
                                    let start = req.prompt[..end - "/graph.json".len()]
                                        .rfind(|c: char| !c.is_ascii_graphic() || c == '`')
                                        .map(|i| i + 1)
                                        .unwrap_or(0);
                                    req.prompt[start..end].to_string()
                                })
                                .unwrap();
                            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                                .unwrap();
                            std::fs::write(&path, &dag).unwrap();
                            ("Done".to_owned(), None)
                        } else if req.prompt.contains("Graph Node Worker") {
                            let wt = if req.prompt.contains("do a") {
                                wt_a_str.clone()
                            } else {
                                wt_b_str.clone()
                            };
                            ("NODE_RESULT: done\nwrote output file".to_owned(), Some(wt))
                        } else {
                            ("NODE_VERDICT: achieved".to_owned(), None)
                        };
                        let _ = req.result_tx.send(SubagentResult {
                            success: true,
                            output: StdArc::from(output.as_str()),
                            subagent_id: req.id.clone(),
                            child_session_id: req.id.clone(),
                            tokens_used: 10,
                            worktree_path: wt,
                            ..Default::default()
                        });
                    }
                }
            });
            let (mut actor, _tmp, _prx) = make_graph_actor(coord_tx).await;
            actor.graph_concurrency = 2;
            // Point the actor's cwd-based HEAD guard at the real repo.
            actor.tool_context.cwd =
                kigi_paths::AbsPathBuf::new(repo.path().to_path_buf()).unwrap();

            let _ = actor.setup_graph("build the diamond", None).await;
            let statuses = node_statuses(&actor);
            assert_eq!(statuses[0].1, NodeStatus::Achieved);
            assert_eq!(statuses[1].1, NodeStatus::Achieved);
            // SEQUENTIAL merge landed both nodes' files in the MAIN tree.
            assert_eq!(
                std::fs::read_to_string(repo.path().join("a_out.txt")).unwrap(),
                "from a\n"
            );
            assert_eq!(
                std::fs::read_to_string(repo.path().join("b_out.txt")).unwrap(),
                "from b\n"
            );
            // Storage discipline: merged worktrees are removed.
            assert!(!wt_a.exists(), "merged worktree a must be cleaned up");
            assert!(!wt_b.exists(), "merged worktree b must be cleaned up");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn budget_top_up_resumes_a_budget_limited_graph_to_completion() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dag = diamond_graph_json();
            let (mut actor, _tmp, _prx) = make_graph_actor_detached().await;
            let (coord_tx, _captured) = spawn_scripted_coordinator(
                _tmp.path().to_path_buf(),
                move |req| happy_reply(req, &dag),
                false,
            );
            actor.tool_context.subagent_event_tx = Some(coord_tx);
            actor.graph_concurrency = 2;
            // Tiny budget: the a+b batch spends 40 (4 spawns × 10) > 30,
            // so the inter-batch gate trips before c.
            let _ = actor.setup_graph("build the diamond", Some(30)).await;
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::BudgetLimited)
            );

            // Plain resume must refuse with the top-up hint.
            match actor.resume_graph(None).await {
                graph::GraphSetupOutcome::Message(msg) => {
                    assert!(msg.contains("--budget"), "{msg}");
                }
                graph::GraphSetupOutcome::Inference { .. } => {
                    panic!("budget-limited graph must not resume without a top-up")
                }
            }

            // Top-up resumes and reaches the serial tail (node c).
            match actor.resume_graph(Some(1_000)).await {
                graph::GraphSetupOutcome::Inference { reminder, .. } => {
                    assert!(reminder.contains("Node C"), "{reminder}");
                }
                graph::GraphSetupOutcome::Message(msg) => {
                    panic!("top-up must resume the graph, got: {msg}")
                }
            }
            // Finish c + gn-final serially.
            for _ in 0..2 {
                drive_node_goal_to_complete(&actor).await;
                let _ = actor.run_graph_round_end().await;
            }
            assert_eq!(
                actor.graph_tracker.lock().status(),
                Some(GoalStatus::Complete)
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── handle_prompt-level coverage (real interception wiring) ────────────

fn agent_text(n: &acp::SessionNotification) -> Option<String> {
    match &n.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            acp::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn capture_gateway(
    mut gateway_rx: tokio::sync::mpsc::UnboundedReceiver<kigi_acp_lib::AcpClientMessage>,
) -> StdArc<tokio::sync::Mutex<Vec<acp::SessionNotification>>> {
    let sent = StdArc::new(tokio::sync::Mutex::new(Vec::new()));
    let sent_for_task = sent.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let kigi_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                sent_for_task.lock().await.push(args.request);
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    sent
}

fn drain_replay(
    actor: StdArc<SessionActor>,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let settings = actor.buffering_settings.clone();
    tokio::task::spawn_local(async move {
        let mut replay_buffer = ReplayBuffer::new(settings);
        while let Some(event) = event_rx.recv().await {
            match event {
                SessionEvent::Notification(notification) => {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                SessionEvent::FlushReplay { respond_to } => {
                    if let Some(notification) = replay_buffer.flush() {
                        actor.emit_buffered(notification).await;
                    }
                    if let Some(tx) = respond_to {
                        let _ = tx.send(());
                    }
                }
            }
        }
    });
}

/// Actor whose `/graph` slash commands resolve through the REAL
/// `handle_prompt` path: `update_goal` registered (goal gate),
/// `graph_enabled` set, gateway capture + replay drainer wired.
async fn make_graph_turn_actor() -> (
    StdArc<SessionActor>,
    StdArc<tokio::sync::Mutex<Vec<acp::SessionNotification>>>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<kigi_acp_lib::AcpClientMessage>();
    let sent = capture_gateway(gateway_rx);
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let (mut actor, event_rx) =
        create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;
    actor.goal_enabled = true;
    actor.graph_enabled = true;
    let actor = StdArc::new(actor);
    drain_replay(actor.clone(), event_rx);
    (actor, sent)
}

async fn drive_terminal_slash(
    actor: &StdArc<SessionActor>,
    sent: &StdArc<tokio::sync::Mutex<Vec<acp::SessionNotification>>>,
    prompt: &str,
) -> String {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        actor.handle_prompt(
            &format!("graph-slash-{}", prompt.replace([' ', '/'], "-")),
            vec![acp::ContentBlock::Text(acp::TextContent::new(
                prompt.to_string(),
            ))],
            PromptMode::Agent,
            None,
            None,
            false,
            None,
            None,
        ),
    )
    .await
    .expect("terminal slash outcome must end the turn without inference");
    assert!(result.is_ok(), "turn must succeed: {result:?}");
    tokio::task::yield_now().await;
    let sent = sent.lock().await;
    sent.iter()
        .filter_map(agent_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn graph_slash_terminal_outcomes_through_handle_prompt() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, sent) = make_graph_turn_actor().await;

            // /graph status with no graph: terminal message, no inference.
            let text = drive_terminal_slash(&actor, &sent, "/graph status").await;
            assert!(
                text.contains("No graph is set"),
                "status message must reach the gateway: {text}"
            );

            // /graph resume with no graph.
            let text = drive_terminal_slash(&actor, &sent, "/graph resume").await;
            assert!(text.contains("No graph is set"), "{text}");

            // Seed an Active graph that owns the engine; /goal commands
            // must be refused through the REAL interception guards.
            actor.graph_tracker.lock().create_graph(
                "g-1".into(),
                "obj".into(),
                None,
                "2026-01-01T00:00:00Z".into(),
            );
            let text = drive_terminal_slash(&actor, &sent, "/goal pause").await;
            assert!(
                text.contains("graph owns the goal engine"),
                "/goal pause refusal must surface: {text}"
            );
            let text = drive_terminal_slash(&actor, &sent, "/goal clear").await;
            assert!(text.contains("graph owns the goal engine"), "{text}");

            // /graph pause on the planning-phase Active graph pauses it.
            let text = drive_terminal_slash(&actor, &sent, "/graph pause").await;
            assert!(text.contains("Graph paused"), "{text}");
            assert!(
                actor
                    .graph_tracker
                    .lock()
                    .status()
                    .is_some_and(|s| s.is_paused())
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
