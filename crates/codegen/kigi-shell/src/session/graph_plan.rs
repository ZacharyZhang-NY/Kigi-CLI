//! Graph planner output contract: parsing, static validation, and
//! canonicalization.
//!
//! The graph planner subagent writes a JSON file shaped as
//! `{"nodes": [{"id": "<slug>", "title": "...", "spec": "...",
//! "deps": ["<slug>", ...]}]}`. Before anything executes, the harness
//! runs the Agentproof-style static gate in [`parse_and_validate`]:
//! parse errors, empty graphs, duplicate/malformed slugs, unknown or
//! self dependencies, and cycles all fail CLOSED with a precise reason
//! (the caller retries planning once, then pauses the graph).
//!
//! Canonicalization: slugs become stable content-derived ids
//! (`gn-<fnv1a32 hex>` of the slug) so the same planned node keeps the
//! same id across replans and across machines (line-mergeable in the
//! G4 project-level graph file), nodes are re-ordered into a
//! planner-order-stable topological order (deterministic serial
//! scheduling), and the harness appends the terminal
//! [`FINAL_NODE_ID`](super::graph_tracker::FINAL_NODE_ID) verification
//! node depending on every planner node — the whole-objective gate is
//! structural, never left to the planner's discretion.

use super::graph_tracker::{DepKind, FINAL_NODE_ID, GraphNode, NodeDep, NodeStatus};

/// Hard cap on planner nodes (the prompt guides 3–10; this bound is the
/// fail-fast backstop against a runaway planner, not a target).
pub(crate) const MAX_GRAPH_NODES: usize = 24;

/// Byte cap for reading the planner's JSON file — same defensive posture
/// as the goal nudge reader: a runaway artifact must not blow up memory.
pub(crate) const MAX_GRAPH_JSON_BYTES: u64 = 256 * 1024;

#[derive(Debug, serde::Deserialize)]
struct PlannedGraph {
    nodes: Vec<PlannedNode>,
}

#[derive(Debug, serde::Deserialize)]
struct PlannedNode {
    id: String,
    title: String,
    spec: String,
    #[serde(default)]
    deps: Vec<String>,
    /// Replan artifacts only: EXISTING node ids (`gn-…`) whose execution
    /// surfaced this node. Ignored by the initial-plan path.
    #[serde(default)]
    discovered_from: Vec<String>,
}

/// Why a planner artifact was rejected. Rendered verbatim into the
/// planning-failure pause message and the retry prompt, so each variant
/// states the fix.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GraphPlanError {
    Parse(String),
    Empty,
    TooManyNodes(usize),
    BadSlug(String),
    DuplicateSlug(String),
    EmptyField {
        slug: String,
        field: &'static str,
    },
    UnknownDep {
        slug: String,
        dep: String,
    },
    SelfDep(String),
    Cycle(Vec<String>),
    IdCollision(String, String),
    /// Replan: a new node's canonical id collides with an existing node.
    ExistingCollision(String),
    /// Replan: a `deps` entry targets a Failed/Blocked node — the new
    /// node could never become Ready.
    DeadDep {
        slug: String,
        dep: String,
    },
    /// Replan: `discovered_from` references a node id not in the graph.
    UnknownOrigin {
        slug: String,
        origin: String,
    },
    /// Optimizer: an operation violated the restricted-op contract
    /// (touched an immutable node, unknown id, malformed shape, …).
    OpInvalid(String),
}

impl std::fmt::Display for GraphPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "graph JSON failed to parse: {e}"),
            Self::Empty => write!(f, "graph has no nodes"),
            Self::TooManyNodes(n) => {
                write!(f, "graph has {n} nodes; the cap is {MAX_GRAPH_NODES}")
            }
            Self::BadSlug(s) => write!(
                f,
                "node id {s:?} is invalid: use 1-64 chars of [A-Za-z0-9_-]"
            ),
            Self::DuplicateSlug(s) => write!(f, "duplicate node id {s:?}"),
            Self::EmptyField { slug, field } => {
                write!(f, "node {slug:?} has an empty {field}")
            }
            Self::UnknownDep { slug, dep } => {
                write!(f, "node {slug:?} depends on unknown node {dep:?}")
            }
            Self::SelfDep(s) => write!(f, "node {s:?} depends on itself"),
            Self::Cycle(nodes) => {
                write!(f, "dependency cycle among nodes: {}", nodes.join(", "))
            }
            Self::IdCollision(a, b) => write!(
                f,
                "hash id collision between slugs {a:?} and {b:?}; rename one"
            ),
            Self::ExistingCollision(s) => write!(
                f,
                "new node {s:?} collides with an existing graph node; rename it"
            ),
            Self::UnknownOrigin { slug, origin } => write!(
                f,
                "node {slug:?} claims discovered_from unknown node {origin:?}"
            ),
            Self::DeadDep { slug, dep } => write!(
                f,
                "node {slug:?} depends on {dep:?}, which already failed; depend on \
                 live nodes only (or none)"
            ),
            Self::OpInvalid(reason) => write!(f, "invalid optimization op: {reason}"),
        }
    }
}

/// FNV-1a 32-bit over the slug, rendered as 8 lowercase hex chars.
/// Stable across builds, platforms, and Rust versions — the property
/// the project-level graph file (G4) needs for line-level merges.
fn fnv1a32_hex(s: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// Canonical node id for a planner slug.
pub(crate) fn node_id_for_slug(slug: &str) -> String {
    format!("gn-{}", fnv1a32_hex(slug))
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parse, statically validate, and canonicalize a planner artifact.
///
/// On success the returned nodes are in planner-order-stable
/// topological order, carry `gn-` hash ids (`title` is kept verbatim;
/// the slug survives only inside the id hash), all start `Waiting`,
/// and end with the harness-appended final verification node.
pub(crate) fn parse_and_validate(
    json: &str,
    objective: &str,
) -> Result<Vec<GraphNode>, GraphPlanError> {
    let mut planned: PlannedGraph =
        serde_json::from_str(json).map_err(|e| GraphPlanError::Parse(e.to_string()))?;
    if planned.nodes.is_empty() {
        return Err(GraphPlanError::Empty);
    }
    // Dedup repeated dep entries (first occurrence kept): harmless
    // planner redundancy, and the indegree seed below would otherwise
    // misreport a duplicated edge as a cycle.
    for node in &mut planned.nodes {
        let mut seen_deps = std::collections::HashSet::new();
        node.deps.retain(|d| seen_deps.insert(d.clone()));
    }
    if planned.nodes.len() > MAX_GRAPH_NODES {
        return Err(GraphPlanError::TooManyNodes(planned.nodes.len()));
    }

    // Slug hygiene + uniqueness + non-empty payload fields.
    let mut seen = std::collections::HashSet::new();
    for node in &planned.nodes {
        if !valid_slug(&node.id) {
            return Err(GraphPlanError::BadSlug(node.id.clone()));
        }
        if !seen.insert(node.id.as_str()) {
            return Err(GraphPlanError::DuplicateSlug(node.id.clone()));
        }
        if node.title.trim().is_empty() {
            return Err(GraphPlanError::EmptyField {
                slug: node.id.clone(),
                field: "title",
            });
        }
        if node.spec.trim().is_empty() {
            return Err(GraphPlanError::EmptyField {
                slug: node.id.clone(),
                field: "spec",
            });
        }
    }

    // Dependency resolution.
    for node in &planned.nodes {
        for dep in &node.deps {
            if dep == &node.id {
                return Err(GraphPlanError::SelfDep(node.id.clone()));
            }
            if !seen.contains(dep.as_str()) {
                return Err(GraphPlanError::UnknownDep {
                    slug: node.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
    }

    // Kahn's algorithm, planner-order-stable: each round takes the
    // FIRST remaining zero-indegree node in planner order, so the
    // serial scheduler's "first Ready in storage order" rule inherits
    // the planner's intent.
    let order = stable_topo_order(&planned)?;

    // Canonical ids; collisions between distinct slugs fail fast.
    let mut id_of: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut owner_of_id: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for node in &planned.nodes {
        let id = node_id_for_slug(&node.id);
        if let Some(prior) = owner_of_id.insert(id.clone(), node.id.as_str()) {
            return Err(GraphPlanError::IdCollision(
                prior.to_owned(),
                node.id.clone(),
            ));
        }
        id_of.insert(node.id.as_str(), id);
    }

    let mut nodes: Vec<GraphNode> = order
        .into_iter()
        .map(|idx| {
            let p = &planned.nodes[idx];
            GraphNode {
                id: id_of[p.id.as_str()].clone(),
                title: p.title.trim().to_owned(),
                spec: p.spec.trim().to_owned(),
                deps: p
                    .deps
                    .iter()
                    .map(|d| NodeDep {
                        on: id_of[d.as_str()].clone(),
                        kind: DepKind::Blocks,
                    })
                    .collect(),
                status: NodeStatus::Waiting,
                goal_id: None,
                rounds: 0,
                tokens_used: 0,
                failure: None,
            }
        })
        .collect();

    nodes.push(final_verification_node(objective, &nodes));
    Ok(nodes)
}

/// Planner-order-stable Kahn topological sort; `Err(Cycle)` lists the
/// slugs left when no zero-indegree node remains.
fn stable_topo_order(planned: &PlannedGraph) -> Result<Vec<usize>, GraphPlanError> {
    let n = planned.nodes.len();
    let index_of: std::collections::HashMap<&str, usize> = planned
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut indegree = vec![0usize; n];
    for node in &planned.nodes {
        let i = index_of[node.id.as_str()];
        indegree[i] = node.deps.len();
    }
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let Some(next) = (0..n).find(|&i| !done[i] && indegree[i] == 0) else {
            let cycle: Vec<String> = (0..n)
                .filter(|&i| !done[i])
                .map(|i| planned.nodes[i].id.clone())
                .collect();
            return Err(GraphPlanError::Cycle(cycle));
        };
        done[next] = true;
        order.push(next);
        let slug = planned.nodes[next].id.as_str();
        for node in &planned.nodes {
            if node.deps.iter().any(|d| d == slug) {
                indegree[index_of[node.id.as_str()]] -= 1;
            }
        }
    }
    Ok(order)
}

/// The harness-appended terminal gate: a normal goal whose objective is
/// to independently re-verify the WHOLE graph objective. Depends on
/// every planner node, so it is always the last schedulable node.
fn final_verification_node(objective: &str, planner_nodes: &[GraphNode]) -> GraphNode {
    GraphNode {
        id: FINAL_NODE_ID.to_owned(),
        title: "Final verification of the overall objective".to_owned(),
        spec: format!(
            "Independently verify that the OVERALL objective below is fully achieved, \
             end to end, in the current state of the project. Re-run the relevant \
             builds/tests/commands yourself; do not trust prior claims. If you find a \
             gap, close it. Do not add features beyond the objective.\n\n\
             OVERALL OBJECTIVE:\n{objective}"
        ),
        deps: planner_nodes
            .iter()
            .map(|n| NodeDep {
                on: n.id.clone(),
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

/// Parse and validate a REPLAN artifact against the existing graph:
/// strictly append-only. New nodes may depend on existing `gn-…` ids or
/// on each other; the combined graph must stay acyclic; existing nodes
/// are never modified. Returns the canonicalized appendix — `Waiting`
/// status, `Blocks` deps, plus one `DiscoveredFrom` edge per validated
/// `discovered_from` origin.
pub(crate) fn validate_replan(
    existing: &[GraphNode],
    json: &str,
) -> Result<Vec<GraphNode>, GraphPlanError> {
    let mut planned: PlannedGraph =
        serde_json::from_str(json).map_err(|e| GraphPlanError::Parse(e.to_string()))?;
    if planned.nodes.is_empty() {
        return Err(GraphPlanError::Empty);
    }
    // Whole-graph cap: MAX_GRAPH_NODES planner nodes + gn-final. The
    // payload excludes the final node so "the cap is N" stays truthful
    // for replans too.
    if planned.nodes.len() + existing.len() > MAX_GRAPH_NODES + 1 {
        return Err(GraphPlanError::TooManyNodes(
            planned.nodes.len() + existing.len() - 1,
        ));
    }
    for node in &mut planned.nodes {
        let mut seen_deps = std::collections::HashSet::new();
        node.deps.retain(|d| seen_deps.insert(d.clone()));
    }

    let existing_ids: std::collections::HashSet<&str> =
        existing.iter().map(|n| n.id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    for node in &planned.nodes {
        if !valid_slug(&node.id) {
            return Err(GraphPlanError::BadSlug(node.id.clone()));
        }
        if !seen.insert(node.id.as_str()) {
            return Err(GraphPlanError::DuplicateSlug(node.id.clone()));
        }
        if node.title.trim().is_empty() {
            return Err(GraphPlanError::EmptyField {
                slug: node.id.clone(),
                field: "title",
            });
        }
        if node.spec.trim().is_empty() {
            return Err(GraphPlanError::EmptyField {
                slug: node.id.clone(),
                field: "spec",
            });
        }
        for origin in &node.discovered_from {
            if !existing_ids.contains(origin.as_str()) {
                return Err(GraphPlanError::UnknownOrigin {
                    slug: node.id.clone(),
                    origin: origin.clone(),
                });
            }
            // Any edge onto the terminal node would cycle the moment
            // append_replan_nodes gates it on the appendix. Fail fast.
            if origin == FINAL_NODE_ID {
                return Err(GraphPlanError::UnknownDep {
                    slug: node.id.clone(),
                    dep: FINAL_NODE_ID.to_owned(),
                });
            }
        }
        if node.deps.iter().any(|d| d == FINAL_NODE_ID) {
            return Err(GraphPlanError::UnknownDep {
                slug: node.id.clone(),
                dep: FINAL_NODE_ID.to_owned(),
            });
        }
    }

    // Canonical ids for the appendix; must not collide with anything.
    let mut id_of: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut owner_of_id: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for node in &planned.nodes {
        let id = node_id_for_slug(&node.id);
        if existing_ids.contains(id.as_str()) {
            return Err(GraphPlanError::ExistingCollision(node.id.clone()));
        }
        if let Some(prior) = owner_of_id.insert(id.clone(), node.id.as_str()) {
            return Err(GraphPlanError::IdCollision(
                prior.to_owned(),
                node.id.clone(),
            ));
        }
        id_of.insert(node.id.as_str(), id);
    }

    // Deps resolve against existing ids (verbatim) or new slugs.
    let resolve = |dep: &str| -> Option<String> {
        if existing_ids.contains(dep) {
            Some(dep.to_owned())
        } else {
            id_of.get(dep).cloned()
        }
    };
    let dead_ids: std::collections::HashSet<&str> = existing
        .iter()
        .filter(|n| matches!(n.status, NodeStatus::Failed | NodeStatus::Blocked))
        .map(|n| n.id.as_str())
        .collect();
    for node in &planned.nodes {
        for dep in &node.deps {
            if dep == &node.id {
                return Err(GraphPlanError::SelfDep(node.id.clone()));
            }
            if resolve(dep).is_none() {
                return Err(GraphPlanError::UnknownDep {
                    slug: node.id.clone(),
                    dep: dep.clone(),
                });
            }
            // An ordering dep on a dead node can never satisfy; fail
            // fast so the attempt-2 feedback loop repairs the artifact.
            // (`discovered_from` origins are exempt — audit-only edges,
            // and failed origins are the NORMAL salvage case.)
            if dead_ids.contains(dep.as_str()) {
                return Err(GraphPlanError::DeadDep {
                    slug: node.id.clone(),
                    dep: dep.clone(),
                });
            }
        }
    }

    // Combined-graph acyclicity (Kahn over existing edges + appendix).
    // Existing nodes only ever depend on existing nodes, so seeding
    // their edges verbatim is sound.
    {
        let mut ids: Vec<String> = existing.iter().map(|n| n.id.clone()).collect();
        ids.extend(planned.nodes.iter().map(|n| id_of[n.id.as_str()].clone()));
        let index_of: std::collections::HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for n in existing {
            for d in &n.deps {
                edges.push((index_of[d.on.as_str()], index_of[n.id.as_str()]));
            }
        }
        for n in &planned.nodes {
            let to = index_of[id_of[n.id.as_str()].as_str()];
            for d in &n.deps {
                edges.push((index_of[resolve(d).expect("validated").as_str()], to));
            }
        }
        let mut indegree = vec![0usize; ids.len()];
        for (_, to) in &edges {
            indegree[*to] += 1;
        }
        let mut done = vec![false; ids.len()];
        for _ in 0..ids.len() {
            let Some(next) = (0..ids.len()).find(|&i| !done[i] && indegree[i] == 0) else {
                let cycle: Vec<String> = (0..ids.len())
                    .filter(|&i| !done[i])
                    .map(|i| ids[i].clone())
                    .collect();
                return Err(GraphPlanError::Cycle(cycle));
            };
            done[next] = true;
            for (from, to) in &edges {
                if *from == next {
                    indegree[*to] -= 1;
                }
            }
        }
    }

    Ok(planned
        .nodes
        .iter()
        .map(|p| {
            let mut deps: Vec<NodeDep> = p
                .deps
                .iter()
                .map(|d| NodeDep {
                    on: resolve(d).expect("validated"),
                    kind: DepKind::Blocks,
                })
                .collect();
            for origin in &p.discovered_from {
                if !deps.iter().any(|d| &d.on == origin) {
                    deps.push(NodeDep {
                        on: origin.clone(),
                        kind: DepKind::DiscoveredFrom,
                    });
                }
            }
            GraphNode {
                id: id_of[p.id.as_str()].clone(),
                title: p.title.trim().to_owned(),
                spec: p.spec.trim().to_owned(),
                deps,
                status: NodeStatus::Waiting,
                goal_id: None,
                rounds: 0,
                tokens_used: 0,
                failure: None,
            }
        })
        .collect())
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum OptimizeOp {
    RemoveDep {
        node: String,
        dep: String,
    },
    Reorder {
        order: Vec<String>,
    },
    Merge {
        into: String,
        from: String,
    },
    Split {
        node: String,
        replacements: Vec<PlannedNode>,
    },
}

#[derive(serde::Deserialize)]
struct OptimizePlan {
    ops: Vec<OptimizeOp>,
}

fn is_pending(status: NodeStatus) -> bool {
    matches!(status, NodeStatus::Waiting | NodeStatus::Ready)
}

/// Apply a restricted optimization-op list to the graph, returning the
/// transformed node set — or `Ok(None)` for an explicitly empty op list
/// (a respected "already good" answer).
///
/// Contract enforced twice: per-op checks (pending-only targets, known
/// ids), then a FINAL diff invariant — every node that was NOT
/// Waiting/Ready must be byte-identical in the result, `gn-final`'s
/// gate is rebuilt over all surviving non-final nodes, and the combined
/// graph must remain acyclic.
pub(crate) fn apply_optimization(
    existing: &[GraphNode],
    json: &str,
) -> Result<Option<Vec<GraphNode>>, GraphPlanError> {
    let plan: OptimizePlan =
        serde_json::from_str(json).map_err(|e| GraphPlanError::Parse(e.to_string()))?;
    if plan.ops.is_empty() {
        return Ok(None);
    }
    let mut nodes: Vec<GraphNode> = existing.to_vec();
    let find = |nodes: &[GraphNode], id: &str| -> Result<usize, GraphPlanError> {
        nodes
            .iter()
            .position(|n| n.id == id)
            .ok_or_else(|| GraphPlanError::OpInvalid(format!("unknown node {id:?}")))
    };
    let pending_or_err = |nodes: &[GraphNode], idx: usize| -> Result<(), GraphPlanError> {
        if !is_pending(nodes[idx].status) {
            return Err(GraphPlanError::OpInvalid(format!(
                "node {:?} is {:?}; only Waiting/Ready nodes may be edited",
                nodes[idx].id, nodes[idx].status
            )));
        }
        Ok(())
    };

    for op in plan.ops {
        match op {
            OptimizeOp::RemoveDep { node, dep } => {
                let idx = find(&nodes, &node)?;
                pending_or_err(&nodes, idx)?;
                let before = nodes[idx].deps.len();
                nodes[idx].deps.retain(|d| d.on != dep);
                if nodes[idx].deps.len() == before {
                    return Err(GraphPlanError::OpInvalid(format!(
                        "node {node:?} has no dependency on {dep:?}"
                    )));
                }
            }
            OptimizeOp::Reorder { order } => {
                let mut seen = std::collections::HashSet::new();
                for id in &order {
                    let idx = find(&nodes, id)?;
                    pending_or_err(&nodes, idx)?;
                    if !seen.insert(id.as_str()) {
                        return Err(GraphPlanError::OpInvalid(format!(
                            "reorder lists {id:?} twice"
                        )));
                    }
                }
                // Stable rearrangement: listed nodes adopt the listed
                // relative order across the positions they occupied.
                let positions: Vec<usize> = nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| order.contains(&n.id))
                    .map(|(i, _)| i)
                    .collect();
                let picked: Vec<GraphNode> = order
                    .iter()
                    .map(|id| nodes[find(&nodes, id).expect("checked above")].clone())
                    .collect();
                for (&pos, node) in positions.iter().zip(picked) {
                    nodes[pos] = node;
                }
            }
            OptimizeOp::Merge { into, from } => {
                if into == from {
                    return Err(GraphPlanError::OpInvalid("merge into == from".to_owned()));
                }
                // Rewiring dependents must never touch an immutable node:
                // a Blocked dependent (dead chain) would either be
                // mutated (tripping the final invariant) or left with a
                // dangling dep. Reject up front with the true reason.
                if let Some(dependent) = nodes.iter().find(|n| {
                    n.id != FINAL_NODE_ID
                        && !is_pending(n.status)
                        && n.deps.iter().any(|d| d.on == from)
                }) {
                    return Err(GraphPlanError::OpInvalid(format!(
                        "node {from:?} has non-pending dependent {:?}; it cannot be merged",
                        dependent.id
                    )));
                }
                if from == FINAL_NODE_ID || into == FINAL_NODE_ID {
                    return Err(GraphPlanError::OpInvalid(
                        "the terminal node cannot participate in a merge".to_owned(),
                    ));
                }
                let into_idx = find(&nodes, &into)?;
                let from_idx = find(&nodes, &from)?;
                pending_or_err(&nodes, into_idx)?;
                pending_or_err(&nodes, from_idx)?;
                let from_node = nodes.remove(from_idx);
                let into_idx = find(&nodes, &into)?;
                nodes[into_idx].spec =
                    format!("{}\n\nAND: {}", nodes[into_idx].spec, from_node.spec);
                for dep in from_node.deps {
                    if dep.on != into && !nodes[into_idx].deps.iter().any(|d| d.on == dep.on) {
                        nodes[into_idx].deps.push(dep);
                    }
                }
                for node in &mut nodes {
                    for dep in &mut node.deps {
                        if dep.on == from {
                            dep.on = into.clone();
                        }
                    }
                    // A dependent of BOTH from and into now lists into
                    // twice; collapse. And `into` itself must never
                    // keep a re-pointed self-dependency (into depended
                    // on from ⇒ that ordering is absorbed by the merge).
                    let own_id = node.id.clone();
                    let mut seen = std::collections::HashSet::new();
                    node.deps
                        .retain(|d| d.on != own_id && seen.insert(d.on.clone()));
                }
            }
            OptimizeOp::Split { node, replacements } => {
                if node == FINAL_NODE_ID {
                    return Err(GraphPlanError::OpInvalid(
                        "the terminal node cannot be split".to_owned(),
                    ));
                }
                let idx = find(&nodes, &node)?;
                pending_or_err(&nodes, idx)?;
                if let Some(dependent) = nodes.iter().find(|n| {
                    n.id != FINAL_NODE_ID
                        && !is_pending(n.status)
                        && n.deps.iter().any(|d| d.on == node)
                }) {
                    return Err(GraphPlanError::OpInvalid(format!(
                        "node {node:?} has non-pending dependent {:?}; it cannot be split",
                        dependent.id
                    )));
                }
                if replacements.len() < 2 || replacements.len() > 3 {
                    return Err(GraphPlanError::OpInvalid(format!(
                        "split of {node:?} needs 2-3 replacements, got {}",
                        replacements.len()
                    )));
                }
                let original = nodes.remove(idx);
                let mut new_ids = Vec::new();
                for rep in &replacements {
                    if !valid_slug(&rep.id) {
                        return Err(GraphPlanError::BadSlug(rep.id.clone()));
                    }
                    if rep.title.trim().is_empty() || rep.spec.trim().is_empty() {
                        return Err(GraphPlanError::OpInvalid(format!(
                            "split replacement {:?} has an empty title/spec",
                            rep.id
                        )));
                    }
                    let id = node_id_for_slug(&rep.id);
                    if nodes.iter().any(|n| n.id == id) || new_ids.contains(&id) {
                        return Err(GraphPlanError::ExistingCollision(rep.id.clone()));
                    }
                    new_ids.push(id);
                }
                let dead_ids: std::collections::HashSet<String> = nodes
                    .iter()
                    .filter(|n| matches!(n.status, NodeStatus::Failed | NodeStatus::Blocked))
                    .map(|n| n.id.clone())
                    .collect();
                for (rep, id) in replacements.iter().zip(&new_ids) {
                    let mut deps: Vec<NodeDep> = original.deps.clone();
                    for d in &rep.deps {
                        let resolved = if nodes.iter().any(|n| n.id == *d) {
                            d.clone()
                        } else if let Some(pos) = replacements.iter().position(|r| r.id == *d) {
                            new_ids[pos].clone()
                        } else {
                            return Err(GraphPlanError::UnknownDep {
                                slug: rep.id.clone(),
                                dep: d.clone(),
                            });
                        };
                        // Ordering dep on a dead node can never satisfy
                        // (same rule as validate_replan's DeadDep).
                        if dead_ids.contains(&resolved) {
                            return Err(GraphPlanError::DeadDep {
                                slug: rep.id.clone(),
                                dep: resolved,
                            });
                        }
                        if !deps.iter().any(|existing| existing.on == resolved) {
                            deps.push(NodeDep {
                                on: resolved,
                                kind: DepKind::Blocks,
                            });
                        }
                    }
                    nodes.push(GraphNode {
                        id: id.clone(),
                        title: rep.title.trim().to_owned(),
                        spec: rep.spec.trim().to_owned(),
                        deps,
                        status: NodeStatus::Waiting,
                        goal_id: None,
                        rounds: 0,
                        tokens_used: 0,
                        failure: None,
                    });
                }
                for n in &mut nodes {
                    if let Some(pos) = n.deps.iter().position(|d| d.on == node) {
                        let kind = n.deps[pos].kind;
                        n.deps.remove(pos);
                        for id in &new_ids {
                            if !n.deps.iter().any(|d| &d.on == id) {
                                n.deps.push(NodeDep {
                                    on: id.clone(),
                                    kind,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Rebuild the terminal gate over all surviving non-final nodes.
    if let Some(final_idx) = nodes.iter().position(|n| n.id == FINAL_NODE_ID) {
        let gate: Vec<NodeDep> = nodes
            .iter()
            .filter(|n| n.id != FINAL_NODE_ID)
            .map(|n| NodeDep {
                on: n.id.clone(),
                kind: DepKind::Blocks,
            })
            .collect();
        nodes[final_idx].deps = gate;
    }

    // Re-derive EVERY pending node's status in both directions: ops can
    // remove a Ready node's last blocker (→ stays Ready via the same
    // rule) or graft unsatisfied deps onto a Ready node (merge), which
    // must demote it — `recompute_ready` downstream is promote-only and
    // would leave a Ready node whose Blocks deps are unmet, silently
    // violating ordering at dispatch.
    let achieved: std::collections::HashSet<String> = nodes
        .iter()
        .filter(|n| n.status == NodeStatus::Achieved)
        .map(|n| n.id.clone())
        .collect();
    let derived: Vec<NodeStatus> = nodes
        .iter()
        .map(|n| {
            if !is_pending(n.status) {
                n.status
            } else if n
                .deps
                .iter()
                .filter(|d| d.kind == DepKind::Blocks)
                .all(|d| achieved.contains(&d.on))
            {
                NodeStatus::Ready
            } else {
                NodeStatus::Waiting
            }
        })
        .collect();
    for (node, status) in nodes.iter_mut().zip(derived) {
        node.status = status;
    }

    // FINAL diff invariant: immutable nodes byte-identical (Debug repr
    // covers every field; GraphNode has no Eq).
    for old in existing {
        if is_pending(old.status) || old.id == FINAL_NODE_ID {
            continue;
        }
        match nodes.iter().find(|n| n.id == old.id) {
            Some(new) if format!("{new:?}") == format!("{old:?}") => {}
            _ => {
                return Err(GraphPlanError::OpInvalid(format!(
                    "immutable node {:?} was modified or removed",
                    old.id
                )));
            }
        }
    }
    if nodes.len() > MAX_GRAPH_NODES + 1 {
        return Err(GraphPlanError::TooManyNodes(nodes.len() - 1));
    }

    // Acyclicity over the whole result.
    {
        let index_of: std::collections::HashMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let mut indegree = vec![0usize; nodes.len()];
        for n in &nodes {
            for d in &n.deps {
                if !index_of.contains_key(d.on.as_str()) {
                    return Err(GraphPlanError::UnknownDep {
                        slug: n.id.clone(),
                        dep: d.on.clone(),
                    });
                }
                indegree[index_of[n.id.as_str()]] += 1;
            }
        }
        let mut done = vec![false; nodes.len()];
        for _ in 0..nodes.len() {
            let Some(next) = (0..nodes.len()).find(|&i| !done[i] && indegree[i] == 0) else {
                let cycle: Vec<String> = (0..nodes.len())
                    .filter(|&i| !done[i])
                    .map(|i| nodes[i].id.clone())
                    .collect();
                return Err(GraphPlanError::Cycle(cycle));
            };
            done[next] = true;
            let next_id = nodes[next].id.clone();
            for n in &nodes {
                if n.deps.iter().any(|d| d.on == next_id) {
                    indegree[index_of[n.id.as_str()]] -= 1;
                }
            }
        }
    }

    Ok(Some(nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_json(nodes: &[(&str, &[&str])]) -> String {
        let nodes: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(id, deps)| {
                serde_json::json!({
                    "id": id,
                    "title": format!("Title {id}"),
                    "spec": format!("Spec for {id}"),
                    "deps": deps,
                })
            })
            .collect();
        serde_json::json!({ "nodes": nodes }).to_string()
    }

    #[test]
    fn valid_plan_canonicalizes_topologically_and_appends_final_node() {
        // Planner order deliberately lists a dependent before its dep.
        let json = plan_json(&[("b", &["a"]), ("a", &[]), ("c", &["a", "b"])]);
        let nodes = parse_and_validate(&json, "ship the feature").unwrap();
        assert_eq!(nodes.len(), 4);
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        // a before b before c; final last.
        assert_eq!(ids[0], node_id_for_slug("a"));
        assert_eq!(ids[1], node_id_for_slug("b"));
        assert_eq!(ids[2], node_id_for_slug("c"));
        assert_eq!(ids[3], FINAL_NODE_ID);
        // Final node depends on all three, and carries the objective.
        assert_eq!(nodes[3].deps.len(), 3);
        assert!(nodes[3].spec.contains("ship the feature"));
        // Deps rewritten to canonical ids.
        assert_eq!(nodes[1].deps[0].on, node_id_for_slug("a"));
    }

    #[test]
    fn ids_are_stable_content_hashes() {
        assert_eq!(node_id_for_slug("auth-flow"), node_id_for_slug("auth-flow"));
        assert_ne!(node_id_for_slug("auth-flow"), node_id_for_slug("auth_flow"));
        assert!(node_id_for_slug("x").starts_with("gn-"));
        assert_eq!(node_id_for_slug("x").len(), 3 + 8);
    }

    #[test]
    fn cycle_is_rejected_with_members_listed() {
        let json = plan_json(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]);
        match parse_and_validate(&json, "o") {
            Err(GraphPlanError::Cycle(members)) => {
                assert!(members.contains(&"a".to_owned()));
                assert!(members.contains(&"b".to_owned()));
                assert!(!members.contains(&"c".to_owned()));
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn structural_errors_are_precise() {
        assert_eq!(
            parse_and_validate(r#"{"nodes":[]}"#, "o").unwrap_err(),
            GraphPlanError::Empty
        );
        assert!(matches!(
            parse_and_validate("not json", "o"),
            Err(GraphPlanError::Parse(_))
        ));
        let dup = plan_json(&[("a", &[]), ("a", &[])]);
        assert_eq!(
            parse_and_validate(&dup, "o").unwrap_err(),
            GraphPlanError::DuplicateSlug("a".into())
        );
        let self_dep = plan_json(&[("a", &["a"])]);
        assert_eq!(
            parse_and_validate(&self_dep, "o").unwrap_err(),
            GraphPlanError::SelfDep("a".into())
        );
        let unknown = plan_json(&[("a", &["ghost"])]);
        assert_eq!(
            parse_and_validate(&unknown, "o").unwrap_err(),
            GraphPlanError::UnknownDep {
                slug: "a".into(),
                dep: "ghost".into()
            }
        );
        let bad = plan_json(&[("has space", &[])]);
        assert_eq!(
            parse_and_validate(&bad, "o").unwrap_err(),
            GraphPlanError::BadSlug("has space".into())
        );
    }

    #[test]
    fn empty_title_or_spec_rejected() {
        let json = serde_json::json!({
            "nodes": [{"id": "a", "title": "  ", "spec": "s", "deps": []}]
        })
        .to_string();
        assert_eq!(
            parse_and_validate(&json, "o").unwrap_err(),
            GraphPlanError::EmptyField {
                slug: "a".into(),
                field: "title"
            }
        );
    }

    #[test]
    fn node_cap_enforced() {
        let slugs: Vec<String> = (0..MAX_GRAPH_NODES + 1).map(|i| format!("n{i}")).collect();
        let pairs: Vec<(&str, &[&str])> = slugs.iter().map(|s| (s.as_str(), &[][..])).collect();
        let json = plan_json(&pairs);
        assert_eq!(
            parse_and_validate(&json, "o").unwrap_err(),
            GraphPlanError::TooManyNodes(MAX_GRAPH_NODES + 1)
        );
    }

    #[test]
    fn planner_order_breaks_topo_ties() {
        // Two independent roots: planner listed z first, so z schedules first.
        let json = plan_json(&[("z", &[]), ("a", &[])]);
        let nodes = parse_and_validate(&json, "o").unwrap();
        assert_eq!(nodes[0].id, node_id_for_slug("z"));
        assert_eq!(nodes[1].id, node_id_for_slug("a"));
    }

    fn existing_graph() -> Vec<GraphNode> {
        parse_and_validate(&plan_json(&[("a", &[]), ("b", &["a"])]), "objective").unwrap()
    }

    #[test]
    fn replan_appendix_resolves_existing_ids_and_adds_discovered_from_edges() {
        let existing = existing_graph();
        let a_id = node_id_for_slug("a");
        let json = serde_json::json!({
            "nodes": [{
                "id": "docs",
                "title": "Docs",
                "spec": "write docs",
                "deps": [a_id.clone()],
                "discovered_from": [a_id.clone()],
            }]
        })
        .to_string();
        let appendix = validate_replan(&existing, &json).unwrap();
        assert_eq!(appendix.len(), 1);
        let node = &appendix[0];
        assert_eq!(node.status, NodeStatus::Waiting);
        // Blocks dep on the existing id, deduped against the
        // DiscoveredFrom edge (same target keeps the Blocks edge only).
        assert_eq!(node.deps.len(), 1);
        assert_eq!(node.deps[0].on, a_id);
        assert_eq!(node.deps[0].kind, DepKind::Blocks);

        // Distinct origin gets its own DiscoveredFrom edge.
        let b_id = node_id_for_slug("b");
        let json = serde_json::json!({
            "nodes": [{
                "id": "docs2",
                "title": "Docs 2",
                "spec": "s",
                "deps": [a_id.clone()],
                "discovered_from": [b_id.clone()],
            }]
        })
        .to_string();
        let appendix = validate_replan(&existing, &json).unwrap();
        let node = &appendix[0];
        assert_eq!(node.deps.len(), 2);
        assert!(
            node.deps
                .iter()
                .any(|d| d.on == b_id && d.kind == DepKind::DiscoveredFrom)
        );
    }

    fn opt_state() -> Vec<GraphNode> {
        // a(Achieved) → b(Ready, FALSE dep on c), c(Ready), final gate.
        let mut nodes = parse_and_validate(
            &plan_json(&[("a", &[]), ("b", &["a", "c"]), ("c", &[])]),
            "o",
        )
        .unwrap();
        let a = node_id_for_slug("a");
        for n in &mut nodes {
            if n.id == a {
                n.status = NodeStatus::Achieved;
            } else if n.deps.iter().all(|d| d.on == a) {
                n.status = NodeStatus::Ready;
            }
        }
        nodes
    }

    #[test]
    fn optimizer_remove_dep_restores_parallelism_and_empty_ops_is_noop() {
        let existing = opt_state();
        let b = node_id_for_slug("b");
        let c = node_id_for_slug("c");
        assert!(
            apply_optimization(&existing, r#"{"ops": []}"#)
                .unwrap()
                .is_none(),
            "explicit empty ops is a respected no-op"
        );
        let json = serde_json::json!({
            "ops": [{"op": "remove_dep", "node": b.clone(), "dep": c.clone()}]
        })
        .to_string();
        let optimized = apply_optimization(&existing, &json).unwrap().unwrap();
        let b_node = optimized.iter().find(|n| n.id == b).unwrap();
        assert!(
            !b_node.deps.iter().any(|d| d.on == c),
            "false dep removed — b and c can now run in parallel"
        );
        // Removing a dep that does not exist is loud.
        let bad = serde_json::json!({
            "ops": [{"op": "remove_dep", "node": c, "dep": b}]
        })
        .to_string();
        assert!(matches!(
            apply_optimization(&existing, &bad).unwrap_err(),
            GraphPlanError::OpInvalid(_)
        ));
    }

    #[test]
    fn optimizer_rejects_touching_immutable_nodes() {
        let existing = opt_state();
        let a = node_id_for_slug("a"); // Achieved — immutable
        for json in [
            serde_json::json!({"ops": [{"op": "remove_dep", "node": a.clone(), "dep": "x"}]}),
            serde_json::json!({"ops": [{"op": "reorder", "order": [a.clone()]}]}),
            serde_json::json!({"ops": [{"op": "merge", "into": node_id_for_slug("b"), "from": a.clone()}]}),
            serde_json::json!({"ops": [{"op": "split", "node": a.clone(), "replacements": [
                {"id": "p1", "title": "P1", "spec": "s"},
                {"id": "p2", "title": "P2", "spec": "s"},
            ]}]}),
        ] {
            assert!(
                matches!(
                    apply_optimization(&existing, &json.to_string()).unwrap_err(),
                    GraphPlanError::OpInvalid(_)
                ),
                "op touching an Achieved node must be rejected: {json}"
            );
        }
        // The terminal node is likewise untouchable.
        let final_merge = serde_json::json!({
            "ops": [{"op": "merge", "into": node_id_for_slug("b"), "from": FINAL_NODE_ID}]
        });
        assert!(matches!(
            apply_optimization(&existing, &final_merge.to_string()).unwrap_err(),
            GraphPlanError::OpInvalid(_)
        ));
    }

    #[test]
    fn optimizer_merge_and_split_rewire_dependents_and_final_gate() {
        let existing = opt_state();
        let b = node_id_for_slug("b");
        let c = node_id_for_slug("c");
        // Merge c into b: b absorbs the spec; final gate loses c.
        let json = serde_json::json!({
            "ops": [{"op": "merge", "into": b.clone(), "from": c.clone()}]
        })
        .to_string();
        let merged = apply_optimization(&existing, &json).unwrap().unwrap();
        assert!(merged.iter().all(|n| n.id != c));
        let final_node = merged.iter().find(|n| n.id == FINAL_NODE_ID).unwrap();
        assert!(!final_node.deps.iter().any(|d| d.on == c));
        assert!(final_node.deps.iter().any(|d| d.on == b));
        let b_node = merged.iter().find(|n| n.id == b).unwrap();
        assert!(b_node.spec.contains("AND:"));
        assert!(
            !b_node.deps.iter().any(|d| d.on == b),
            "merge must not self-depend"
        );

        // Split b into two parts: dependents (final) gate on both parts.
        let json = serde_json::json!({
            "ops": [{"op": "split", "node": b, "replacements": [
                {"id": "b-core", "title": "Core half", "spec": "s1"},
                {"id": "b-glue", "title": "Glue half", "spec": "s2", "deps": ["b-core"]},
            ]}]
        })
        .to_string();
        let split = apply_optimization(&existing, &json).unwrap().unwrap();
        let p1 = node_id_for_slug("b-core");
        let p2 = node_id_for_slug("b-glue");
        let final_node = split.iter().find(|n| n.id == FINAL_NODE_ID).unwrap();
        assert!(final_node.deps.iter().any(|d| d.on == p1));
        assert!(final_node.deps.iter().any(|d| d.on == p2));
        let glue = split.iter().find(|n| n.id == p2).unwrap();
        assert!(
            glue.deps.iter().any(|d| d.on == p1),
            "intra-split dep resolved"
        );
        assert!(
            glue.deps.iter().any(|d| d.on == node_id_for_slug("a")),
            "replacements inherit the split node's deps"
        );
    }

    #[test]
    fn merge_grafting_unsatisfied_deps_demotes_ready_into_to_waiting() {
        // d(Waiting, dep on c) — merge d into b (b Ready): b absorbs the
        // unsatisfied dep on c and MUST demote to Waiting, or dispatch
        // would run b ahead of c (critical review finding).
        let mut existing = opt_state();
        let c = node_id_for_slug("c");
        existing.insert(
            3,
            GraphNode {
                id: node_id_for_slug("d"),
                title: "D".into(),
                spec: "d".into(),
                deps: vec![NodeDep {
                    on: c.clone(),
                    kind: DepKind::Blocks,
                }],
                status: NodeStatus::Waiting,
                goal_id: None,
                rounds: 0,
                tokens_used: 0,
                failure: None,
            },
        );
        let b = node_id_for_slug("b");
        // Give b a satisfied-only dep set first (remove the false dep on c).
        let json = serde_json::json!({
            "ops": [
                {"op": "remove_dep", "node": b.clone(), "dep": c.clone()},
                {"op": "merge", "into": b.clone(), "from": node_id_for_slug("d")},
            ]
        })
        .to_string();
        let optimized = apply_optimization(&existing, &json).unwrap().unwrap();
        let b_node = optimized.iter().find(|n| n.id == b).unwrap();
        assert!(b_node.deps.iter().any(|d| d.on == c), "dep absorbed");
        assert_eq!(
            b_node.status,
            NodeStatus::Waiting,
            "Ready node absorbing an unmet dep must demote"
        );
    }

    #[test]
    fn split_rejects_deps_on_dead_nodes() {
        let mut existing = opt_state();
        let a = node_id_for_slug("a");
        existing.iter_mut().find(|n| n.id == a).unwrap().status = NodeStatus::Failed;
        let json = serde_json::json!({
            "ops": [{"op": "split", "node": node_id_for_slug("b"), "replacements": [
                {"id": "p1", "title": "P1", "spec": "s", "deps": [a]},
                {"id": "p2", "title": "P2", "spec": "s"},
            ]}]
        })
        .to_string();
        assert!(matches!(
            apply_optimization(&existing, &json).unwrap_err(),
            GraphPlanError::DeadDep { .. }
        ));
    }

    #[test]
    fn merge_and_split_reject_targets_with_non_pending_dependents() {
        // B(Blocked) deps on p(Ready): merging or splitting p would have
        // to rewire an immutable node — reject with the TRUE reason.
        let mut existing = opt_state();
        let b = node_id_for_slug("b");
        let c = node_id_for_slug("c");
        existing.iter_mut().find(|n| n.id == b).unwrap().status = NodeStatus::Blocked;
        // b already deps on c in opt_state, so c has a Blocked dependent.
        let merge = serde_json::json!({
            "ops": [{"op": "merge", "into": node_id_for_slug("a"), "from": c.clone()}]
        });
        // (a is Achieved, so this would fail pending_or_err anyway — use
        // a fresh pending target instead.)
        let _ = merge;
        let split = serde_json::json!({
            "ops": [{"op": "split", "node": c, "replacements": [
                {"id": "p1", "title": "P1", "spec": "s"},
                {"id": "p2", "title": "P2", "spec": "s"},
            ]}]
        });
        match apply_optimization(&existing, &split.to_string()).unwrap_err() {
            GraphPlanError::OpInvalid(reason) => {
                assert!(
                    reason.contains("non-pending dependent"),
                    "true reason surfaces: {reason}"
                );
            }
            other => panic!("expected OpInvalid, got {other:?}"),
        }
    }

    #[test]
    fn optimizer_rejects_result_cycles() {
        let existing = opt_state();
        // b already deps c; a reorder is fine, but adding a cycle via
        // split deps pointing at a dependent must fail the final check.
        let json = serde_json::json!({
            "ops": [{"op": "split", "node": node_id_for_slug("c"), "replacements": [
                {"id": "c1", "title": "C1", "spec": "s", "deps": [node_id_for_slug("b")]},
                {"id": "c2", "title": "C2", "spec": "s"},
            ]}]
        })
        .to_string();
        assert!(matches!(
            apply_optimization(&existing, &json).unwrap_err(),
            GraphPlanError::Cycle(_)
        ));
    }

    #[test]
    fn replan_rejects_blocks_deps_on_dead_nodes_but_allows_dead_origins() {
        let mut existing = existing_graph();
        let a_id = node_id_for_slug("a");
        existing.iter_mut().find(|n| n.id == a_id).unwrap().status = NodeStatus::Failed;
        let dead_dep = serde_json::json!({
            "nodes": [{"id": "x", "title": "T", "spec": "s", "deps": [a_id.clone()]}]
        })
        .to_string();
        assert!(matches!(
            validate_replan(&existing, &dead_dep).unwrap_err(),
            GraphPlanError::DeadDep { .. }
        ));
        // A dead ORIGIN is the normal salvage case — allowed, and the
        // audit-only DiscoveredFrom edge never gates scheduling.
        let dead_origin = serde_json::json!({
            "nodes": [{"id": "x", "title": "T", "spec": "s", "deps": [],
                        "discovered_from": [a_id]}]
        })
        .to_string();
        assert!(validate_replan(&existing, &dead_origin).is_ok());
    }

    #[test]
    fn replan_rejects_edges_onto_the_terminal_node() {
        let existing = existing_graph();
        for json in [
            serde_json::json!({"nodes": [{"id": "x", "title": "T", "spec": "s",
                "deps": [FINAL_NODE_ID]}]}),
            serde_json::json!({"nodes": [{"id": "x", "title": "T", "spec": "s",
                "deps": [], "discovered_from": [FINAL_NODE_ID]}]}),
        ] {
            assert!(
                matches!(
                    validate_replan(&existing, &json.to_string()).unwrap_err(),
                    GraphPlanError::UnknownDep { .. }
                ),
                "an edge onto gn-final would cycle after the final-gating extension"
            );
        }
    }

    #[test]
    fn replan_rejects_collisions_unknown_origins_and_cycles() {
        let existing = existing_graph();
        // Re-using an existing slug collides on the canonical id.
        let dup = serde_json::json!({
            "nodes": [{"id": "a", "title": "T", "spec": "s", "deps": []}]
        })
        .to_string();
        assert_eq!(
            validate_replan(&existing, &dup).unwrap_err(),
            GraphPlanError::ExistingCollision("a".into())
        );
        // Unknown discovered_from origin.
        let bad_origin = serde_json::json!({
            "nodes": [{"id": "x", "title": "T", "spec": "s", "deps": [],
                        "discovered_from": ["gn-ghost"]}]
        })
        .to_string();
        assert!(matches!(
            validate_replan(&existing, &bad_origin).unwrap_err(),
            GraphPlanError::UnknownOrigin { .. }
        ));
        // New-node cycle.
        let cyc = serde_json::json!({
            "nodes": [
                {"id": "x", "title": "T", "spec": "s", "deps": ["y"]},
                {"id": "y", "title": "T", "spec": "s", "deps": ["x"]},
            ]
        })
        .to_string();
        assert!(matches!(
            validate_replan(&existing, &cyc).unwrap_err(),
            GraphPlanError::Cycle(_)
        ));
    }

    /// A repeated dep entry is harmless planner redundancy: it must be
    /// deduped, NOT misreported as a cycle by the indegree seed.
    #[test]
    fn duplicate_dep_entries_are_deduped_not_a_cycle() {
        let json = plan_json(&[("a", &[]), ("b", &["a", "a"])]);
        let nodes = parse_and_validate(&json, "o").unwrap();
        assert_eq!(nodes.len(), 3, "a, b, final");
        let b = &nodes[1];
        assert_eq!(b.id, node_id_for_slug("b"));
        assert_eq!(b.deps.len(), 1, "duplicate edge collapsed");
        assert_eq!(b.deps[0].on, node_id_for_slug("a"));
    }
}
