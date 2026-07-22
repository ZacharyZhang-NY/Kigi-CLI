//! Project-level shared graph file (G4): `.kigi/graph.jsonl` at the git
//! root, so a graph follows the REPOSITORY, not the session.
//!
//! The session tracker remains the single source of truth; this file is
//! a PROJECTION refreshed at every checkpoint. Format (beads-style,
//! line-mergeable thanks to content-hash node ids):
//!
//! - line 1: the orchestration header (everything except `nodes`)
//! - lines 2..: one `GraphNode` per line
//!
//! Concurrency: an advisory `flock` on a sidecar `.lock` file makes the
//! session that CREATED or RESUMED the graph the single writer; other
//! kigi instances get a read-only view (`/graph status`) with an
//! explicit notice. The lock is held for the graph's lifetime in that
//! session and released on `/graph clear` (or process exit).
//!
//! Git discipline: kigi only WRITES the file — committing it is the
//! user's decision, never automated.

use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::graph_tracker::{GraphNode, GraphOrchestration};

/// Header line: the orchestration minus its nodes (which follow one per
/// line). The shadow `nodes` field is skipped on write and REJECTED on
/// read when non-empty — nodes embedded in the header would silently
/// duplicate the per-line entries.
#[derive(serde::Serialize, serde::Deserialize)]
struct ProjectGraphHeader {
    #[serde(flatten)]
    orchestration: GraphOrchestration,
}

fn header_has_inline_nodes(header: &ProjectGraphHeader) -> bool {
    !header.orchestration.nodes.is_empty()
}

/// Held exclusive advisory lock on the project graph. Dropping releases.
#[derive(Debug)]
pub struct ProjectGraphLock {
    _file: std::fs::File,
}

#[derive(Debug)]
pub enum LockOutcome {
    Acquired(ProjectGraphLock),
    /// Another kigi instance holds the lock.
    Busy,
}

/// `.kigi` dir under the git root of `cwd`; `None` outside a git repo
/// (the project-graph feature is git-scoped by design).
pub fn project_graph_dir(cwd: &Path) -> Option<PathBuf> {
    kigi_workspace::session::git::find_git_root_from_path(cwd)
        .ok()
        .map(|root| root.join(".kigi"))
}

pub fn graph_file_path(dir: &Path) -> PathBuf {
    dir.join("graph.jsonl")
}

fn lock_file_path(dir: &Path) -> PathBuf {
    dir.join("graph.jsonl.lock")
}

/// Try to become the project graph's single writer. Fail-fast: any I/O
/// error other than "already locked" propagates.
pub fn try_acquire_writer(dir: &Path) -> std::io::Result<LockOutcome> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_file_path(dir))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(LockOutcome::Acquired(ProjectGraphLock { _file: file })),
        // fs2 maps contention differently per platform (EWOULDBLOCK on
        // unix, ERROR_LOCK_VIOLATION on Windows); its
        // `lock_contended_error()` is the portable classifier.
        Err(err)
            if err.kind() == std::io::ErrorKind::WouldBlock
                || err.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(LockOutcome::Busy)
        }
        Err(err) => Err(err),
    }
}

/// Atomically project the orchestration to `.kigi/graph.jsonl`
/// (tmp + rename, same discipline as the session state file).
pub fn project(dir: &Path, state: &GraphOrchestration) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut header_state = state.clone();
    let nodes = std::mem::take(&mut header_state.nodes);
    let mut header_value = serde_json::to_value(&header_state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(obj) = header_value.as_object_mut() {
        // The contract says "minus nodes"; drop the empty vec the
        // struct serializer would otherwise emit.
        obj.remove("nodes");
    }
    let mut buf = Vec::with_capacity(4096);
    serde_json::to_writer(&mut buf, &header_value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    for node in &nodes {
        serde_json::to_writer(&mut buf, node)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        buf.push(b'\n');
    }
    let target = graph_file_path(dir);
    let tmp = target.with_extension("jsonl.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    crate::util::fs::replace_file(&tmp, &target)
}

/// Load the projected graph, `Ok(None)` when absent. Malformed content
/// is an ERROR (never silently treated as "no graph") — the file is
/// user-visible, git-merged state; corruption must surface.
pub fn load(dir: &Path) -> std::io::Result<Option<GraphOrchestration>> {
    let path = graph_file_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
    let Some(header_line) = lines.next() else {
        return Ok(None);
    };
    let header: ProjectGraphHeader = serde_json::from_str(header_line).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} line 1: {e}", path.display()),
        )
    })?;
    if header_has_inline_nodes(&header) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} line 1 embeds nodes inline; nodes belong one per line",
                path.display()
            ),
        ));
    }
    let mut state = header.orchestration;
    for (idx, line) in lines.enumerate() {
        let node: GraphNode = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} node line {}: {e}", path.display(), idx + 2),
            )
        })?;
        state.nodes.push(node);
    }
    Ok(Some(state))
}

/// Remove the projection (on `/graph clear`). Missing file is fine.
pub fn remove(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(graph_file_path(dir)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::goal_tracker::{GoalPhase, GoalStatus};
    use crate::session::graph_tracker::{DepKind, NodeDep, NodeStatus};
    use tempfile::TempDir;

    fn sample_state() -> GraphOrchestration {
        GraphOrchestration {
            graph_id: "g-1".into(),
            objective: "ship it".into(),
            status: GoalStatus::Active,
            phase: GoalPhase::Executing,
            plan_version: 2,
            nodes: vec![
                GraphNode {
                    id: "gn-aaaa".into(),
                    title: "A".into(),
                    spec: "do a".into(),
                    deps: vec![],
                    status: NodeStatus::Achieved,
                    goal_id: Some("goal-1".into()),
                    rounds: 2,
                    tokens_used: 100,
                    failure: None,
                },
                GraphNode {
                    id: "gn-bbbb".into(),
                    title: "B".into(),
                    spec: "do b".into(),
                    deps: vec![NodeDep {
                        on: "gn-aaaa".into(),
                        kind: DepKind::DiscoveredFrom,
                    }],
                    status: NodeStatus::Running,
                    goal_id: None,
                    rounds: 0,
                    tokens_used: 0,
                    failure: None,
                },
            ],
            current_node: Some("gn-bbbb".into()),
            created_at: "2026-07-20T00:00:00Z".into(),
            elapsed_ms: 12,
            token_budget: Some(1_000),
            tokens_spent_nodes: 100,
            history: vec![],
            pause_message: None,
            pending_discoveries: vec![],
            replan_runs: 1,
        }
    }

    #[test]
    fn project_load_round_trip_is_line_per_node() {
        let tmp = TempDir::new().unwrap();
        let state = sample_state();
        project(tmp.path(), &state).unwrap();
        let raw = std::fs::read_to_string(graph_file_path(tmp.path())).unwrap();
        assert_eq!(raw.lines().count(), 3, "header + one line per node");
        assert!(
            !raw.lines().next().unwrap().contains("\"nodes\""),
            "header must omit nodes entirely"
        );
        assert!(raw.lines().nth(1).unwrap().contains("gn-aaaa"));
        let loaded = load(tmp.path()).unwrap().expect("present");
        assert_eq!(loaded.graph_id, state.graph_id);
        assert_eq!(loaded.plan_version, 2);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.nodes[1].deps[0].kind, DepKind::DiscoveredFrom);
        assert_eq!(loaded.current_node.as_deref(), Some("gn-bbbb"));
    }

    #[test]
    fn load_absent_is_none_and_remove_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        assert!(load(tmp.path()).unwrap().is_none());
        remove(tmp.path()).unwrap();
        project(tmp.path(), &sample_state()).unwrap();
        remove(tmp.path()).unwrap();
        assert!(load(tmp.path()).unwrap().is_none());
        remove(tmp.path()).unwrap();
    }

    #[test]
    fn header_with_inline_nodes_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut bad = serde_json::to_value(sample_state()).unwrap();
        // Keep nodes inline in the header — a hand-edited/merged file.
        bad.as_object_mut().unwrap().remove("current_node");
        std::fs::write(graph_file_path(tmp.path()), format!("{bad}\n")).unwrap();
        let err = load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("inline"), "{err}");
    }

    #[test]
    fn malformed_content_is_a_loud_error_not_a_missing_graph() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(graph_file_path(tmp.path()), "not json\n").unwrap();
        let err = load(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 1"), "{err}");
    }

    #[test]
    fn writer_lock_is_exclusive_within_and_across_handles() {
        let tmp = TempDir::new().unwrap();
        let first = try_acquire_writer(tmp.path()).unwrap();
        let LockOutcome::Acquired(_guard) = first else {
            panic!("first acquire must win");
        };
        match try_acquire_writer(tmp.path()).unwrap() {
            LockOutcome::Busy => {}
            LockOutcome::Acquired(_) => {
                // flock is per-fd on some platforms within one process;
                // if this arm is reached the platform lets the same
                // process re-lock, which is still safe for our
                // cross-INSTANCE contract — but on macOS/Linux flock
                // between distinct fds does contend, so treat as bug.
                panic!("second handle must observe Busy");
            }
        }
        drop(_guard);
        assert!(matches!(
            try_acquire_writer(tmp.path()).unwrap(),
            LockOutcome::Acquired(_)
        ));
    }
}
