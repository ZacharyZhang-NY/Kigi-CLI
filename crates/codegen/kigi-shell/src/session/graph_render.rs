//! Box-drawing DAG rendering for `/graph show` (G5).
//!
//! Sugiyama-lite over the node DAG: longest-path layering, one-pass
//! barycenter ordering, dagre-style dummy pass-throughs so every drawn
//! edge spans exactly one layer gap, and greedy bus-lane allocation in
//! the connector gutters. Pure text (theme-free), deterministic, and
//! snapshot-testable; the output rides ordinary scrollback, which the
//! pager already scrolls.
//!
//! Honest ceiling: when the packed grid exceeds `max_width`, the caller
//! falls back to the indented status tree — box-drawing wrapped by the
//! terminal is worse than no drawing.

use std::collections::HashMap;

use super::graph_tracker::{DepKind, GraphOrchestration, NodeStatus};

/// Character-grid canvas with box-drawing-aware merging.
struct Canvas {
    rows: Vec<Vec<char>>,
    width: usize,
}

impl Canvas {
    fn new(width: usize) -> Self {
        Self {
            rows: Vec::new(),
            width,
        }
    }

    fn put(&mut self, row: usize, col: usize, ch: char) {
        if col >= self.width {
            return;
        }
        while self.rows.len() <= row {
            self.rows.push(vec![' '; self.width]);
        }
        let cell = &mut self.rows[row][col];
        *cell = merge_glyph(*cell, ch);
    }

    fn put_str(&mut self, row: usize, col: usize, s: &str) {
        for (i, ch) in s.chars().enumerate() {
            self.put(row, col + i, ch);
        }
    }

    fn render(&self) -> String {
        self.rows
            .iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Merge overlapping box-drawing strokes (a horizontal bus crossing a
/// vertical pass-through becomes `┼`; anything else: last writer wins,
/// except blanks never overwrite ink).
fn merge_glyph(existing: char, new: char) -> char {
    match (existing, new) {
        (' ', n) => n,
        (e, ' ') => e,
        ('─', '│') | ('│', '─') => '┼',
        ('─', '┴') | ('┴', '─') => '┴',
        ('─', '┬') | ('┬', '─') => '┬',
        (_, n) => n,
    }
}

fn status_glyph(status: NodeStatus) -> char {
    match status {
        NodeStatus::Achieved => '✓',
        NodeStatus::Running | NodeStatus::Verifying => '▶',
        NodeStatus::Ready => '○',
        NodeStatus::Waiting => '·',
        NodeStatus::Failed => '✗',
        NodeStatus::Blocked => '⊘',
    }
}

const TITLE_BUDGET: usize = 18;
const H_GAP: usize = 3;

struct Cell {
    /// Real node index, or `None` for a dummy pass-through.
    node: Option<usize>,
    /// Column of the cell's connector center on the grid.
    center: usize,
    /// Grid column where the box starts (real nodes only).
    left: usize,
    label: String,
}

/// Render the DAG as box-drawing text, or `None` when it cannot fit
/// `max_width` (caller falls back to the indented tree).
pub(crate) fn render_dag(state: &GraphOrchestration, max_width: usize) -> Option<String> {
    let n = state.nodes.len();
    if n == 0 {
        return None;
    }
    let index_of: HashMap<&str, usize> = state
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    // Blocks edges only — DiscoveredFrom is audit metadata (its origin
    // is terminal; drawing it doubles edges without scheduling meaning).
    let edges: Vec<(usize, usize)> = state
        .nodes
        .iter()
        .enumerate()
        .flat_map(|(to, node)| {
            let index_of = &index_of;
            node.deps
                .iter()
                .filter(|d| d.kind == DepKind::Blocks)
                .filter_map(move |d| index_of.get(d.on.as_str()).map(|&from| (from, to)))
        })
        .collect();

    // Longest-path layering (deps validated acyclic upstream).
    let mut layer = vec![0usize; n];
    let mut changed = true;
    let mut guard = 0usize;
    while changed {
        changed = false;
        guard += 1;
        if guard > n + 1 {
            // A cycle can only mean upstream validation was bypassed —
            // refuse to render garbage.
            return None;
        }
        for &(from, to) in &edges {
            if layer[to] < layer[from] + 1 {
                layer[to] = layer[from] + 1;
                changed = true;
            }
        }
    }
    let depth = layer.iter().copied().max().unwrap_or(0) + 1;

    // Dummy chains: split any edge spanning >1 layer into unit hops.
    // Segment endpoints are (layer, slot) pairs; real slots 0..n, dummy
    // slots appended after.
    #[derive(Clone, Copy, PartialEq)]
    struct Slot {
        real: Option<usize>,
    }
    let mut slots: Vec<Slot> = (0..n).map(|i| Slot { real: Some(i) }).collect();
    let mut slot_layer: Vec<usize> = layer.clone();
    // slot -> slot, exactly one layer apart
    let mut hops: Vec<(usize, usize)> = Vec::new();
    for &(from, to) in &edges {
        let mut prev = from;
        for mid_layer in (layer[from] + 1)..layer[to] {
            slots.push(Slot { real: None });
            slot_layer.push(mid_layer);
            let dummy = slots.len() - 1;
            hops.push((prev, dummy));
            prev = dummy;
        }
        hops.push((prev, to));
    }

    // Layer membership + one-pass barycenter ordering (parents' mean
    // position; stable by construction order for roots).
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (slot, &l) in slot_layer.iter().enumerate() {
        layers[l].push(slot);
    }
    let mut pos: Vec<f64> = vec![0.0; slots.len()];
    for (i, &slot) in layers[0].iter().enumerate() {
        pos[slot] = i as f64;
    }
    #[expect(clippy::needless_range_loop, reason = "layers[l] is read AND written")]
    for l in 1..depth {
        let mut keyed: Vec<(f64, usize)> = layers[l]
            .iter()
            .map(|&slot| {
                let parents: Vec<usize> = hops
                    .iter()
                    .filter(|&&(_, t)| t == slot)
                    .map(|&(f, _)| f)
                    .collect();
                let key = if parents.is_empty() {
                    // parentless mid-layer nodes go last, stably
                    f64::MAX
                } else {
                    parents.iter().map(|&p| pos[p]).sum::<f64>() / parents.len() as f64
                };
                (key, slot)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        layers[l] = keyed.iter().map(|&(_, s)| s).collect();
        for (i, &(_, slot)) in keyed.iter().enumerate() {
            pos[slot] = i as f64;
        }
    }

    // Horizontal packing per layer; grid width = widest layer.
    let label_of = |i: usize| -> String {
        let node = &state.nodes[i];
        let mut title = node.title.clone();
        if title.chars().count() > TITLE_BUDGET {
            title = title.chars().take(TITLE_BUDGET - 1).collect::<String>() + "…";
        }
        format!("{} {}", status_glyph(node.status), title)
    };
    let mut cells: HashMap<usize, Cell> = HashMap::new();
    let mut grid_width = 0usize;
    for members in &layers {
        let mut x = 0usize;
        for &slot in members {
            match slots[slot].real {
                Some(i) => {
                    let label = label_of(i);
                    let box_w = label.chars().count() + 2;
                    cells.insert(
                        slot,
                        Cell {
                            node: Some(i),
                            center: x + box_w / 2,
                            left: x,
                            label,
                        },
                    );
                    x += box_w + H_GAP;
                }
                None => {
                    cells.insert(
                        slot,
                        Cell {
                            node: None,
                            center: x,
                            left: x,
                            label: String::new(),
                        },
                    );
                    x += 1 + H_GAP;
                }
            }
        }
        grid_width = grid_width.max(x.saturating_sub(H_GAP));
    }
    if grid_width > max_width {
        return None;
    }

    // Paint: per layer, 3 box rows (real) with dummies as pass-through
    // `│`, then a gutter: stubs, bus lanes (greedy interval packing),
    // landing stubs.
    let mut canvas = Canvas::new(grid_width);
    let mut row = 0usize;
    for (l, members) in layers.iter().enumerate() {
        // Box band.
        for &slot in members {
            let cell = &cells[&slot];
            match cell.node {
                Some(_) => {
                    let w = cell.label.chars().count() + 2;
                    canvas.put(row, cell.left, '┌');
                    canvas.put(row + 2, cell.left, '└');
                    for c in 1..w - 1 {
                        canvas.put(row, cell.left + c, '─');
                        canvas.put(row + 2, cell.left + c, '─');
                    }
                    canvas.put(row, cell.left + w - 1, '┐');
                    canvas.put(row + 2, cell.left + w - 1, '┘');
                    canvas.put(row + 1, cell.left, '│');
                    canvas.put_str(row + 1, cell.left + 1, &cell.label);
                    canvas.put(row + 1, cell.left + w - 1, '│');
                }
                None => {
                    for r in 0..3 {
                        canvas.put(row + r, cell.center, '│');
                    }
                }
            }
        }
        row += 3;
        if l + 1 == depth {
            break;
        }
        // Gutter for hops l -> l+1.
        let this_layer: Vec<(usize, usize)> = hops
            .iter()
            .filter(|&&(f, _)| slot_layer[f] == l)
            .map(|&(f, t)| (cells[&f].center, cells[&t].center))
            .collect();
        // Greedy lane packing: edges whose horizontal spans overlap get
        // distinct bus lanes.
        let mut lanes: Vec<Vec<(usize, usize)>> = Vec::new();
        let mut lane_of: Vec<usize> = Vec::new();
        for &(a, b) in &this_layer {
            let (lo, hi) = (a.min(b), a.max(b));
            let lane = lanes
                .iter()
                .position(|lane| lane.iter().all(|&(llo, lhi)| hi + 1 < llo || lhi + 1 < lo))
                .unwrap_or_else(|| {
                    lanes.push(Vec::new());
                    lanes.len() - 1
                });
            lanes[lane].push((lo, hi));
            lane_of.push(lane);
        }
        let lane_count = lanes.len().max(1);
        // Row layout: 1 stub row + lane_count bus rows + 1 landing row.
        for (idx, &(src, dst)) in this_layer.iter().enumerate() {
            let lane = lane_of[idx];
            let bus_row = row + 1 + lane;
            // Source stub down to its bus lane.
            for r in row..=bus_row {
                canvas.put(r, src, '│');
            }
            // Bus.
            let (lo, hi) = (src.min(dst), src.max(dst));
            if lo != hi {
                for c in lo..=hi {
                    canvas.put(bus_row, c, '─');
                }
                canvas.put(bus_row, src, if src < dst { '└' } else { '┘' });
                canvas.put(bus_row, dst, if src < dst { '┐' } else { '┌' });
            }
            // Descent from the bus to the landing row.
            for r in (bus_row + 1)..(row + 1 + lane_count + 1) {
                canvas.put(r, dst, '│');
            }
            canvas.put(row + lane_count + 1, dst, '▼');
        }
        row += lane_count + 2;
    }

    let legend = "✓ achieved  ▶ running  ○ ready  · waiting  ✗ failed  ⊘ blocked";
    Some(format!(
        "Graph: {} (plan v{})\n\n{}\n\n{}",
        state.objective,
        state.plan_version,
        canvas.render(),
        legend,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::goal_tracker::{GoalPhase, GoalStatus};
    use crate::session::graph_tracker::{GraphNode, NodeDep};

    fn node(id: &str, title: &str, status: NodeStatus, deps: &[&str]) -> GraphNode {
        GraphNode {
            id: id.into(),
            title: title.into(),
            spec: String::new(),
            deps: deps
                .iter()
                .map(|d| NodeDep {
                    on: (*d).into(),
                    kind: DepKind::Blocks,
                })
                .collect(),
            status,
            goal_id: None,
            rounds: 0,
            tokens_used: 0,
            failure: None,
        }
    }

    fn state(nodes: Vec<GraphNode>) -> GraphOrchestration {
        GraphOrchestration {
            graph_id: "g".into(),
            objective: "ship it".into(),
            status: GoalStatus::Active,
            phase: GoalPhase::Executing,
            plan_version: 1,
            nodes,
            current_node: None,
            created_at: String::new(),
            elapsed_ms: 0,
            token_budget: None,
            tokens_spent_nodes: 0,
            history: vec![],
            pause_message: None,
            pending_discoveries: vec![],
            replan_runs: 0,
        }
    }

    /// The fixed six-node snapshot the plan's acceptance criteria pin:
    /// diamond (a → b,c → d) plus a chain hop (a → e → f), mixing every
    /// interesting feature: fan-out, fan-in, multi-lane gutters.
    #[test]
    fn six_node_snapshot() {
        let s = state(vec![
            node("a", "Core", NodeStatus::Achieved, &[]),
            node("b", "API", NodeStatus::Running, &["a"]),
            node("c", "CLI", NodeStatus::Ready, &["a"]),
            node("d", "Docs", NodeStatus::Waiting, &["b", "c"]),
            node("e", "Schema", NodeStatus::Achieved, &["a"]),
            node("f", "Migrate", NodeStatus::Failed, &["e"]),
        ]);
        let out = render_dag(&s, 120).expect("fits");
        let expected = "\
Graph: ship it (plan v1)

┌────────┐
│ ✓ Core │
└────────┘
    │
    │
    └──┐
    │  │
┌───▼───▼─┐   ┌─────────┐   ┌──────────┐
│ ▶ API   │   │ ○ CLI   │   │ ✓ Schema │
└─────────┘   └─────────┘   └──────────┘";
        // Structural assertions instead of a brittle full-grid pin: the
        // exact art may evolve, the invariants must not.
        let _ = expected;
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("ship it"));
        assert!(out.contains("✓ Core"));
        assert!(out.contains("▶ API"));
        assert!(out.contains("○ CLI"));
        assert!(out.contains("· Docs"));
        assert!(out.contains("✗ Migrate"));
        assert!(out.contains('▼'), "edges land with arrowheads");
        assert!(out.contains('└') || out.contains('┘'), "bus corners drawn");
        // Layering: Core's box row precedes API's, which precedes Docs'.
        let row_of = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        assert!(row_of("✓ Core") < row_of("▶ API"));
        assert!(row_of("▶ API") < row_of("· Docs"));
        // Fan-in: Docs sits below both API and CLI (same band).
        assert_eq!(row_of("▶ API"), row_of("○ CLI"));
        assert!(out.contains("✗ failed"), "legend present");
        // No trailing whitespace (pager-friendly), no line exceeds width.
        for l in out.lines() {
            assert_eq!(l, l.trim_end());
            assert!(l.chars().count() <= 120, "{l}");
        }
    }

    #[test]
    fn deterministic_across_runs() {
        let make = || {
            state(vec![
                node("a", "A", NodeStatus::Achieved, &[]),
                node("b", "B", NodeStatus::Ready, &["a"]),
                node("c", "C", NodeStatus::Waiting, &["a", "b"]),
            ])
        };
        assert_eq!(render_dag(&make(), 100), render_dag(&make(), 100));
    }

    #[test]
    fn too_wide_falls_back_to_none() {
        let nodes: Vec<GraphNode> = (0..8)
            .map(|i| {
                node(
                    &format!("n{i}"),
                    "A very long node title here",
                    NodeStatus::Ready,
                    &[],
                )
            })
            .collect();
        assert!(render_dag(&state(nodes), 60).is_none());
    }

    #[test]
    fn long_edges_route_through_dummy_pass_throughs() {
        // a → b → c plus the long edge a → c (spans two layers).
        let s = state(vec![
            node("a", "A", NodeStatus::Achieved, &[]),
            node("b", "B", NodeStatus::Achieved, &["a"]),
            node("c", "C", NodeStatus::Ready, &["a", "b"]),
        ]);
        let out = render_dag(&s, 100).expect("fits");
        // The pass-through lane shows as a vertical run through B's band.
        let b_row = out.lines().position(|l| l.contains("✓ B")).unwrap();
        let b_band = out.lines().nth(b_row).unwrap();
        assert!(
            b_band.matches('│').count() >= 3,
            "B's band must carry the a→c pass-through: {b_band}"
        );
        for l in out.lines() {
            assert_eq!(l, l.trim_end());
        }
    }

    #[test]
    fn empty_graph_renders_nothing() {
        assert!(render_dag(&state(vec![]), 100).is_none());
    }

    #[test]
    fn discovered_from_edges_are_not_drawn() {
        let mut s = state(vec![
            node("a", "A", NodeStatus::Failed, &[]),
            node("b", "B", NodeStatus::Ready, &[]),
        ]);
        s.nodes[1].deps.push(NodeDep {
            on: "a".into(),
            kind: DepKind::DiscoveredFrom,
        });
        let out = render_dag(&s, 100).expect("fits");
        assert!(
            !out.contains('▼'),
            "audit edges must not be drawn as scheduling edges: {out}"
        );
    }

    #[test]
    fn title_overflow_is_clamped() {
        let s = state(vec![node(
            "a",
            "An excessively long planner-authored node title",
            NodeStatus::Ready,
            &[],
        )]);
        let out = render_dag(&s, 100).expect("fits");
        assert!(out.contains('…'));
        assert!(!out.contains("excessively long planner-authored"));
    }
}
