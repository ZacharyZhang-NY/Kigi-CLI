//! Local bootstrap-marker bookkeeping for session search.
//!
//! Tracks the `last_bootstrap_at` timestamp in the search index's sqlite
//! meta table so incremental bootstrap can tell whether a full rebuild is
//! needed.

use std::io;
use std::path::Path;

use super::search_fts::SessionSearchIndex;

/// SQLite meta key for the last successful bootstrap timestamp (unix secs).
const META_KEY_LAST_BOOTSTRAP: &str = "last_bootstrap_at";

/// Read `last_bootstrap_at` from the sqlite meta table, preserving read
/// failures, so callers can tell "marker genuinely absent" apart from "could
/// not read the DB" (transient busy/locked/I/O). A missing DB file is a true
/// absence, not an error.
pub fn try_read_last_bootstrap_at(db_path: &Path) -> Result<Option<i64>, String> {
    if !db_path.exists() {
        return Ok(None);
    }
    let index = SessionSearchIndex::open_or_create(db_path).map_err(|e| e.to_string())?;
    let value = index
        .get_meta(META_KEY_LAST_BOOTSTRAP)
        .map_err(|e| e.to_string())?;
    Ok(value.and_then(|v| v.parse::<i64>().ok()))
}

/// Write `last_bootstrap_at` into the sqlite meta table.
pub fn write_last_bootstrap_at(db_path: &Path) -> io::Result<()> {
    let index =
        SessionSearchIndex::open_or_create(db_path).map_err(|e| io::Error::other(e.to_string()))?;
    let now = chrono::Utc::now().timestamp();
    index
        .set_meta(META_KEY_LAST_BOOTSTRAP, &now.to_string())
        .map_err(|e| io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_write_last_bootstrap_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("session_search.sqlite");

        assert_eq!(try_read_last_bootstrap_at(&db_path).unwrap(), None);

        write_last_bootstrap_at(&db_path).unwrap();

        let ts = try_read_last_bootstrap_at(&db_path).unwrap().unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!(
            (now - ts).abs() < 5,
            "timestamp should be within 5 seconds of now"
        );
    }
}
