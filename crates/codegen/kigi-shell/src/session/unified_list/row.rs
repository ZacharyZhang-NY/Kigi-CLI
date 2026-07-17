use serde::Serialize;

use super::envelope::{FacetMap, SessionKind, SessionMetaEnvelope};
use super::facets::{FacetRegistry, NormalizedItem};
use crate::session::merge::MergedSession;

#[derive(Debug, Clone)]
pub struct UnifiedRow {
    pub kind: SessionKind,
    pub legacy: MergedSession,
    pub title: String,
    pub updated_at: Option<String>,
    pub facets: FacetMap,
}

impl UnifiedRow {
    fn envelope(kind: SessionKind, facets: FacetMap) -> RowMeta {
        RowMeta {
            session: SessionMetaEnvelope { kind, facets },
        }
    }

    pub fn into_ext_superset(self) -> ExtSupersetRow {
        let UnifiedRow {
            kind,
            legacy,
            title,
            facets,
            updated_at: _,
        } = self;
        ExtSupersetRow {
            legacy,
            title,
            meta: Self::envelope(kind, facets),
        }
    }

    pub fn into_session_info(self) -> SessionInfo {
        let UnifiedRow {
            kind,
            legacy,
            title,
            updated_at,
            facets,
        } = self;
        SessionInfo {
            session_id: legacy.session_id,
            cwd: legacy.cwd,
            title: (!title.is_empty()).then_some(title),
            updated_at,
            meta: Self::envelope(kind, facets),
        }
    }

    pub(super) fn sort_timestamp(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        self.updated_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    }
}

pub fn merged_session_to_row(m: MergedSession, reg: &FacetRegistry) -> UnifiedRow {
    let facets = reg.extract_all(&NormalizedItem::from_merged(&m));
    let title = m.summary.clone();
    let updated_at = effective_local_ts(&m);
    UnifiedRow {
        kind: SessionKind::Build,
        legacy: m,
        title,
        updated_at,
        facets,
    }
}

fn effective_local_ts(m: &MergedSession) -> Option<String> {
    m.last_active_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .or_else(|| chrono::DateTime::parse_from_rfc3339(&m.updated_at).ok())
        .map(|dt| dt.to_rfc3339())
}

#[derive(Debug, Clone, Serialize)]
pub struct RowMeta {
    #[serde(rename = "x.ai/session")]
    pub session: SessionMetaEnvelope,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtSupersetRow {
    #[serde(flatten)]
    pub legacy: MergedSession,
    pub title: String,
    #[serde(rename = "_meta")]
    pub meta: RowMeta,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: RowMeta,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::unified_list::facet_registry;
}
