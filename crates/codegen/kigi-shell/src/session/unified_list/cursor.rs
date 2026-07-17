use std::cmp::{Ordering, Reverse};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::envelope::SessionKind;
use super::row::UnifiedRow;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct CompositeCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct BoundaryKey {
    pub updated_at: String,
    pub kind: SessionKind,
    pub session_id: String,
}

impl CompositeCursor {
    pub fn decode(raw: Option<&str>) -> Self {
        raw.filter(|s| !s.is_empty())
            .and_then(|s| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(s)
                    .ok()
            })
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }
}

pub(super) struct Paginated {
    pub candidates: Vec<UnifiedRow>,
    pub emit_count: usize,
    pub next_cursor: Option<CompositeCursor>,
}

/// Sort local rows newest-first, resume after the cursor boundary, and cut
/// one page. `next_cursor` is set only when rows remain past the page.
pub(super) fn paginate(
    local: Vec<UnifiedRow>,
    cursor: &CompositeCursor,
    limit: usize,
) -> Paginated {
    let mut keyed: Vec<(SortKey, UnifiedRow)> = local
        .into_iter()
        .map(|row| (row_sort_key(&row), row))
        .collect();

    if let Some(boundary) = &cursor.boundary {
        let bkey = boundary_sort_key(boundary);
        keyed.retain(|(k, _)| k.cmp(&bkey) == Ordering::Greater);
    }

    keyed.sort_by(|(a, _), (b, _)| a.cmp(b));

    let emit_count = keyed.len().min(limit);
    let new_boundary = (emit_count > 0).then(|| boundary_of(&keyed[emit_count - 1].1));
    let has_more = keyed.len() > emit_count;

    let next_cursor = has_more.then(|| CompositeCursor {
        boundary: new_boundary.or_else(|| cursor.boundary.clone()),
    });

    let candidates: Vec<UnifiedRow> = keyed.into_iter().map(|(_, row)| row).collect();

    Paginated {
        candidates,
        emit_count,
        next_cursor,
    }
}

type SortKey = (
    Reverse<Option<chrono::DateTime<chrono::FixedOffset>>>,
    SessionKind,
    String,
);

fn row_sort_key(row: &UnifiedRow) -> SortKey {
    (
        Reverse(row.sort_timestamp()),
        row.kind,
        row.legacy.session_id.clone(),
    )
}

fn boundary_sort_key(boundary: &BoundaryKey) -> SortKey {
    (
        Reverse(parse_ts(&boundary.updated_at)),
        boundary.kind,
        boundary.session_id.clone(),
    )
}

fn boundary_of(row: &UnifiedRow) -> BoundaryKey {
    BoundaryKey {
        updated_at: row.updated_at.clone().unwrap_or_default(),
        kind: row.kind,
        session_id: row.legacy.session_id.clone(),
    }
}

fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

pub(super) fn timestamp_desc(
    a: Option<chrono::DateTime<chrono::FixedOffset>>,
    b: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub(super) fn cmp_total_order(a: &UnifiedRow, b: &UnifiedRow) -> Ordering {
    timestamp_desc(a.sort_timestamp(), b.sort_timestamp())
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.legacy.session_id.cmp(&b.legacy.session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::merge::MergedSession;
    use crate::session::unified_list::{facet_registry, merged_session_to_row};

    fn local(id: &str, ts: &str) -> UnifiedRow {
        let m = MergedSession {
            session_id: id.into(),
            summary: "s".into(),
            first_prompt: None,
            updated_at: ts.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            cwd: "/x".into(),
            hostname: None,
            source: "local".into(),
            model_id: None,
            num_messages: 1,
            last_active_at: Some(ts.into()),
            branch: None,
            repo_name: None,
            worktree_label: None,
            git_root_dir: None,
            git_remotes: Vec::new(),
            source_workspace_dir: None,
            session_kind: None,
        };
        merged_session_to_row(m, facet_registry())
    }

    fn ids(p: &Paginated) -> Vec<String> {
        p.candidates[..p.emit_count]
            .iter()
            .map(|r| r.legacy.session_id.clone())
            .collect()
    }

    #[test]
    fn cursor_roundtrip_boundary_only() {
        let c = CompositeCursor {
            boundary: Some(BoundaryKey {
                updated_at: "2026-02-01T00:00:00Z".into(),
                kind: SessionKind::Build,
                session_id: "a".into(),
            }),
        };
        let decoded = CompositeCursor::decode(Some(&c.encode()));
        let b = decoded.boundary.expect("boundary survives roundtrip");
        assert_eq!(b.session_id, "a");
        assert_eq!(b.updated_at, "2026-02-01T00:00:00Z");
    }

    #[test]
    fn decode_garbage_yields_default() {
        assert!(
            CompositeCursor::decode(Some("!!!not-base64!!!"))
                .boundary
                .is_none()
        );
        assert!(CompositeCursor::decode(None).boundary.is_none());
        assert!(CompositeCursor::decode(Some("")).boundary.is_none());
    }

    #[test]
    fn paginate_sorts_newest_first_and_cuts_page() {
        let rows = vec![
            local("old", "2026-01-01T00:00:00Z"),
            local("new", "2026-03-01T00:00:00Z"),
            local("mid", "2026-02-01T00:00:00Z"),
        ];
        let page = paginate(rows, &CompositeCursor::default(), 2);
        assert_eq!(ids(&page), vec!["new", "mid"]);
        assert!(page.next_cursor.is_some(), "a third row remains");
    }

    #[test]
    fn paginate_resumes_after_boundary_without_duplicates() {
        let rows: Vec<UnifiedRow> = vec![
            local("a", "2026-03-01T00:00:00Z"),
            local("b", "2026-02-01T00:00:00Z"),
            local("c", "2026-01-01T00:00:00Z"),
        ];
        let first = paginate(rows.clone(), &CompositeCursor::default(), 2);
        assert_eq!(ids(&first), vec!["a", "b"]);
        let cursor = first.next_cursor.expect("more rows remain");
        let second = paginate(rows, &cursor, 2);
        assert_eq!(ids(&second), vec!["c"]);
        assert!(second.next_cursor.is_none(), "list is exhausted");
    }

    #[test]
    fn paginate_exact_page_has_no_next_cursor() {
        let rows = vec![
            local("a", "2026-03-01T00:00:00Z"),
            local("b", "2026-02-01T00:00:00Z"),
        ];
        let page = paginate(rows, &CompositeCursor::default(), 2);
        assert_eq!(ids(&page).len(), 2);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn paginate_ties_break_stably_by_session_id() {
        let ts = "2026-02-01T00:00:00Z";
        let rows = vec![local("b", ts), local("a", ts), local("c", ts)];
        let first = paginate(rows.clone(), &CompositeCursor::default(), 2);
        assert_eq!(ids(&first), vec!["a", "b"]);
        let cursor = first.next_cursor.expect("one row remains");
        let second = paginate(rows, &cursor, 2);
        assert_eq!(ids(&second), vec!["c"]);
    }
}
