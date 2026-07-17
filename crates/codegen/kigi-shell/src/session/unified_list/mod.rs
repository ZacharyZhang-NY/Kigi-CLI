mod cursor;
mod envelope;
mod facets;
mod row;
use crate::agent::session_registry_client::SessionRegistryClient;
use cursor::{CompositeCursor, Paginated, paginate};
pub use envelope::{FacetMap, FacetValue, SessionKind, SessionMetaEnvelope};
pub use facets::{
    BRANCH_FACET_KEY, BranchFacet, CWD_FACET_KEY, CwdFacet, FacetProvider, FacetRegistry,
    FacetSummary, FacetSummaryKey, FacetSummaryValue, GIT_ROOT_FACET_KEY, GitRootFacet,
    KIND_FACET_KEY, KindFacet, NormalizedItem, Pushdown, REPO_FACET_KEY, RepoFacet,
    SOURCE_WORKSPACE_FACET_KEY, STARRED_FACET_KEY, SourceQuery, SourceWorkspaceFacet, StarredFacet,
    WORKSPACE_FACET_KEY, WORKTREE_FACET_KEY, WorkspaceFacet, WorktreeFacet, build_facet_registry,
};
pub use row::{ExtSupersetRow, RowMeta, SessionInfo, UnifiedRow, merged_session_to_row};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;
pub const DEFAULT_LIMIT: usize = 30;
const CONV_PAGE_HEADROOM: usize = 5;
static FACET_REGISTRY: LazyLock<FacetRegistry> = LazyLock::new(build_facet_registry);
pub fn facet_registry() -> &'static FacetRegistry {
    &FACET_REGISTRY
}
pub fn parse_list_req(raw: &str) -> Result<ListReq, serde_json::Error> {
    serde_json::from_str(raw)
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListReq {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}
pub struct UnifiedListResult {
    pub rows: Vec<UnifiedRow>,
    pub next_cursor: Option<String>,
    pub facets: FacetSummary,
}
#[derive(Debug, Default)]
struct ParsedMeta {
    facet_filters: BTreeMap<String, Vec<serde_json::Value>>,
    query: Option<String>,
    limit: Option<usize>,
}
impl ParsedMeta {
    fn parse(meta: Option<&serde_json::Value>) -> Self {
        let Some(meta) = meta else {
            return Self::default();
        };
        let facet_filters = meta
            .get("x.ai/facetFilters")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), value_list(v)))
                    .collect()
            })
            .unwrap_or_default();
        let query = meta
            .get("x.ai/query")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let limit = meta
            .get("x.ai/limit")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        Self {
            facet_filters,
            query,
            limit,
        }
    }
}
fn value_list(v: &serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(arr) => arr.clone(),
        other => vec![other.clone()],
    }
}
pub async fn build_unified_list(
    registry_client: Option<&SessionRegistryClient>,
    req: ListReq,
) -> UnifiedListResult {
    let reg = facet_registry();
    let ParsedMeta {
        facet_filters,
        query: meta_query,
        limit: meta_limit,
    } = ParsedMeta::parse(req.meta.as_ref());
    let limit = req.limit.or(meta_limit).unwrap_or(DEFAULT_LIMIT);
    let query = req.query.or(meta_query);
    let cursor = CompositeCursor::decode(req.cursor.as_deref());
    let mut source_query = SourceQuery::default();
    reg.apply_pushdown(&facet_filters, &mut source_query);
    let exclude_build = excludes_build(&facet_filters);
    let over = (limit * 3).max(100);
    let local_rows = if exclude_build {
        Vec::new()
    } else {
        crate::session::merge::fetch_merged(
            registry_client,
            req.cwd.as_deref(),
            query.as_deref(),
            over,
        )
        .await
        .into_iter()
        .map(|m| merged_session_to_row(m, reg))
        .collect::<Vec<UnifiedRow>>()
    };
    tracing::debug!(
        local_lane_skipped = exclude_build,
        local_rows = local_rows.len(),
        "session list"
    );
    let local_rows = reg.apply_in_memory_filters(&facet_filters, local_rows);
    let Paginated {
        candidates,
        emit_count,
        next_cursor,
    } = paginate(local_rows, &cursor, limit);
    let mut rows = candidates;
    rows.truncate(emit_count);
    let facets = reg.summarize_window(&rows);
    UnifiedListResult {
        rows,
        next_cursor: next_cursor.map(|c| c.encode()),
        facets,
    }
}
/// Mirror of [`excludes_conversations`]: `true` when a non-empty `kind`
/// allow-list does not include `"build"`, so the local lane can be skipped.
fn excludes_build(filters: &BTreeMap<String, Vec<serde_json::Value>>) -> bool {
    match filters.get(KIND_FACET_KEY) {
        Some(allowed) if !allowed.is_empty() => !allowed
            .iter()
            .any(|v| v.as_str() == Some(SessionKind::Build.as_str())),
        _ => false,
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ExtListResponse {
    pub sessions: Vec<ExtSupersetRow>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: ExtListResponseMeta,
}
#[derive(Debug, Clone, Serialize)]
pub struct ExtListResponseMeta {
    #[serde(rename = "x.ai/facets")]
    pub facets: FacetSummary,
}
pub fn ext_list_response(result: UnifiedListResult) -> ExtListResponse {
    let UnifiedListResult {
        rows,
        next_cursor,
        facets,
    } = result;
    ExtListResponse {
        sessions: rows
            .into_iter()
            .map(UnifiedRow::into_ext_superset)
            .collect(),
        next_cursor,
        meta: ExtListResponseMeta { facets },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::merge::MergedSession;
    fn local(session_id: &str, updated_at: &str) -> MergedSession {
        MergedSession {
            session_id: session_id.into(),
            summary: "a summary".into(),
            first_prompt: Some("first prompt".into()),
            updated_at: updated_at.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            cwd: "/Users/me/xai".into(),
            hostname: Some("devbox".into()),
            source: "local".into(),
            model_id: Some("grok-build".into()),
            num_messages: 7,
            last_active_at: Some(updated_at.into()),
            branch: Some("main".into()),
            repo_name: Some("xai".into()),
            worktree_label: Some("wt".into()),
            git_root_dir: Some("/Users/me/xai".into()),
            git_remotes: vec!["git@github.com:example/repo.git".into()],
            source_workspace_dir: Some("/Users/me/xai-src".into()),
            session_kind: Some("worktree".into()),
        }
    }
    fn row(session_id: &str, updated_at: &str) -> UnifiedRow {
        merged_session_to_row(local(session_id, updated_at), facet_registry())
    }
    #[test]
    fn ext_superset_preserves_every_legacy_field_and_adds_title_and_meta() {
        let value = serde_json::to_value(row("s1", "2026-06-18T20:10:00Z").into_ext_superset())
            .expect("serialize");
        for field in [
            "sessionId",
            "summary",
            "firstPrompt",
            "updatedAt",
            "createdAt",
            "cwd",
            "hostname",
            "source",
            "modelId",
            "numMessages",
            "lastActiveAt",
            "branch",
            "repoName",
            "worktreeLabel",
        ] {
            assert!(value.get(field).is_some(), "missing legacy field: {field}");
        }
        assert_eq!(value["sessionId"], "s1");
        assert_eq!(value["source"], "local");
        assert_eq!(value["numMessages"], 7);
        assert_eq!(value["title"], "a summary");
        assert_eq!(value["_meta"]["x.ai/session"]["kind"], "build");
        assert_eq!(value["gitRootDir"], "/Users/me/xai");
        assert_eq!(value["gitRemotes"][0], "git@github.com:example/repo.git");
        assert_eq!(value["sourceWorkspaceDir"], "/Users/me/xai-src");
        assert_eq!(value["sessionKind"], "worktree");
    }
    #[test]
    fn facets_carry_kind_and_cwd() {
        let r = row("s1", "2026-06-18T20:10:00Z");
        assert!(matches!(r.facets.get(KIND_FACET_KEY),
            Some(FacetValue::One(serde_json::Value::String(k))) if k == "build"));
        assert!(matches!(r.facets.get(CWD_FACET_KEY),
            Some(FacetValue::One(serde_json::Value::String(c))) if c == "/Users/me/xai"));
    }
    #[test]
    fn bare_session_info_is_minimal_plus_meta() {
        let value =
            serde_json::to_value(row("s1", "2026-06-18T20:10:00Z").into_session_info()).unwrap();
        assert_eq!(value["sessionId"], "s1");
        assert_eq!(value["cwd"], "/Users/me/xai");
        assert_eq!(value["title"], "a summary");
        assert_eq!(value["_meta"]["x.ai/session"]["kind"], "build");
        assert!(value.get("summary").is_none());
        assert!(value.get("source").is_none());
    }
    #[test]
    fn total_order_is_updated_at_desc_then_session_id() {
        let mut rows = [
            row("b", "2026-01-01T00:00:00Z"),
            row("a", "2026-06-01T00:00:00Z"),
            row("c", "2026-06-01T00:00:00Z"),
        ];
        rows.sort_by(super::cursor::cmp_total_order);
        let ids: Vec<&str> = rows.iter().map(|r| r.legacy.session_id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }
    #[test]
    fn kind_filter_local_keeps_local_rows() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), vec![serde_json::json!("build")]);
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn kind_filter_conversation_drops_local_rows() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), vec![serde_json::json!("chat")]);
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert!(kept.is_empty());
    }
    #[test]
    fn cwd_filter_is_skipped_in_memory() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(
            CWD_FACET_KEY.to_owned(),
            vec![serde_json::json!("/some/other/dir")],
        );
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn empty_allow_list_is_a_no_op() {
        let reg = facet_registry();
        let rows = vec![row("s1", "2026-06-01T00:00:00Z")];
        let mut filters = BTreeMap::new();
        filters.insert(KIND_FACET_KEY.to_owned(), Vec::new());
        let kept = reg.apply_in_memory_filters(&filters, rows);
        assert_eq!(kept.len(), 1);
    }
    #[test]
    fn parsed_meta_reads_facet_filters_query_and_limit() {
        let meta = serde_json::json!(
            { "x.ai/facetFilters" : { "kind" : ["build"], "starred" : true },
            "x.ai/query" : "antelope", "x.ai/limit" : 5, }
        );
        let parsed = ParsedMeta::parse(Some(&meta));
        assert_eq!(parsed.query.as_deref(), Some("antelope"));
        assert_eq!(parsed.limit, Some(5));
        assert_eq!(
            parsed.facet_filters.get("kind"),
            Some(&vec![serde_json::json!("build")])
        );
        assert_eq!(
            parsed.facet_filters.get("starred"),
            Some(&vec![serde_json::json!(true)])
        );
    }
    fn kind_filter(values: &[&str]) -> BTreeMap<String, Vec<serde_json::Value>> {
        let mut filters = BTreeMap::new();
        filters.insert(
            KIND_FACET_KEY.to_owned(),
            values.iter().map(|v| serde_json::json!(v)).collect(),
        );
        filters
    }
    /// The forced `kind` REPLACES a client-sent `kind: ["build"]` (never
    /// unions), so the local lane stays excluded.
    fn xai_auth_manager(dir: &std::path::Path) -> std::sync::Arc<crate::auth::AuthManager> {
        let am = std::sync::Arc::new(crate::auth::AuthManager::new(
            dir,
            crate::auth::KimiCodeConfig::default(),
        ));
        am.hot_swap(crate::auth::KimiAuth {
            auth_mode: crate::auth::AuthMode::OAuth,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..crate::auth::KimiAuth::test_default()
        });
        am
    }
    /// Minimal HTTP/1.1 responder serving `body` as JSON to every request.
    async fn spawn_conversations_stub(body: String) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }
}
