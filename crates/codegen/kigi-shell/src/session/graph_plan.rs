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
