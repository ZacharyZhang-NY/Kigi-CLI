//! Startup janitor for workspace-owned on-disk session state.

use std::path::Path;
use std::time::Duration;

/// Default maximum age for a per-session state directory before the
/// [`cleanup_stale_sessions`] janitor reclaims it (7 days).
pub const DEFAULT_SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Remove per-session state directories under `<workspace_home>/sessions/`
/// whose mtime is older than `max_age`, bounding the unbounded growth a
/// long-lived workspace (or reused sandbox) would otherwise accumulate.
///
/// A directory's mtime advances on every atomic-rename persistence write (the
/// rename mutates the directory entry), so it tracks last activity closely
/// enough for a best-effort reclaim. All errors are swallowed — a startup
/// janitor must never fail boot. Only directories with a resolvable, expired
/// mtime are removed; stray files and future-mtime entries are left untouched.
pub async fn cleanup_stale_sessions(workspace_home: &Path, max_age: Duration) {
    let sessions_dir = workspace_home.join("sessions");
    let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await else {
        return; // No sessions dir yet (first boot).
    };

    let mut removed = 0u32;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if let Ok(metadata) = tokio::fs::metadata(&path).await
            && let Ok(modified) = metadata.modified()
            && let Ok(age) = modified.elapsed()
            && age > max_age
        {
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(
                        path = %path.display(),
                        age_secs = age.as_secs(),
                        "cleanup_stale_sessions: removed stale session dir"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "cleanup_stale_sessions: failed to remove stale session dir"
                    );
                }
            }
        }
    }

    if removed > 0 {
        tracing::info!(
            removed,
            "cleanup_stale_sessions: stale session-dir sweep complete"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A session dir older than `max_age` is removed.
    #[tokio::test]
    async fn cleanup_removes_stale_session_dir() {
        let home = tempfile::TempDir::new().unwrap();
        let stale = home.path().join("sessions").join("sess-old");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("tool_state.json"), b"{}").unwrap();
        // Guarantee `age > Duration::ZERO` regardless of mtime resolution.
        tokio::time::sleep(Duration::from_millis(15)).await;

        cleanup_stale_sessions(home.path(), Duration::ZERO).await;

        assert!(
            !stale.exists(),
            "a session dir older than max_age must be removed"
        );
    }

    /// A session dir younger than `max_age` is kept.
    #[tokio::test]
    async fn cleanup_keeps_fresh_session_dir() {
        let home = tempfile::TempDir::new().unwrap();
        let fresh = home.path().join("sessions").join("sess-new");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("tool_state.json"), b"{}").unwrap();

        cleanup_stale_sessions(home.path(), Duration::from_secs(3600)).await;

        assert!(
            fresh.exists(),
            "a session dir younger than max_age must be kept"
        );
        assert!(
            fresh.join("tool_state.json").exists(),
            "a kept session dir must retain its contents"
        );
    }

    /// No `sessions/` directory (first boot) is a silent no-op, not a panic.
    #[tokio::test]
    async fn cleanup_missing_sessions_dir_is_noop() {
        let home = tempfile::TempDir::new().unwrap();
        cleanup_stale_sessions(home.path(), Duration::ZERO).await;
        assert!(home.path().exists(), "home is left untouched");
    }

    /// Stray files under `sessions/` are never removed — directories only.
    #[tokio::test]
    async fn cleanup_ignores_non_dir_entries() {
        let home = tempfile::TempDir::new().unwrap();
        let sessions = home.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let stray = sessions.join("stray.txt");
        std::fs::write(&stray, b"not a session dir").unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;

        cleanup_stale_sessions(home.path(), Duration::ZERO).await;

        assert!(
            stray.exists(),
            "stray files under sessions/ must never be removed"
        );
    }

    /// Mixed sweep: the `max_age` comparison is per-entry, not all-or-nothing.
    #[tokio::test]
    async fn cleanup_removes_only_expired_dirs() {
        let home = tempfile::TempDir::new().unwrap();
        let sessions = home.path().join("sessions");
        let old = sessions.join("sess-old");
        std::fs::create_dir_all(&old).unwrap();
        // Wide gap (100ms) vs a 50ms threshold so neither side is sensitive to
        // scheduler jitter.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let max_age = Duration::from_millis(50);
        let fresh = sessions.join("sess-fresh");
        std::fs::create_dir_all(&fresh).unwrap();

        cleanup_stale_sessions(home.path(), max_age).await;

        assert!(!old.exists(), "the >max_age dir must be removed");
        assert!(
            fresh.exists(),
            "the <max_age dir must survive the same sweep"
        );
    }
}
