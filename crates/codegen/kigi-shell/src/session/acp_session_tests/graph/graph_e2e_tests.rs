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
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<kigi_acp_lib::AcpClientMessage>();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
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
    actor.tool_context.subagent_event_tx = Some(coordinator_tx);
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
            match restored.resume_graph().await {
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
            // The in-flight node is terminally resolved — never a
            // forever-Running node on a budget-dead graph.
            assert_eq!(node_statuses(&actor)[0].1, NodeStatus::Failed);
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
            match actor.resume_graph().await {
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
