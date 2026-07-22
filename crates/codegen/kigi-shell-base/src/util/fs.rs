//! Filesystem primitives shared across the shell.

use std::io;
use std::path::Path;

/// Replace `dest` with `tmp` — the commit step of every tmp+rename atomic
/// write in the product. This is the ONE place that knows how to make that
/// commit stick on Windows; call sites must never inline a bare
/// `fs::rename` replace again.
///
/// Unix `rename(2)` replaces atomically and needs no help. Windows
/// `MoveFileExW(REPLACE_EXISTING)` fails with a sharing violation while
/// ANOTHER process (antivirus scanner, search indexer, cloud sync) holds
/// `dest` open — the classic "persists on macOS, silently doesn't on
/// Windows" failure (a model switch that never sticks, a stale models
/// cache). On Windows a failed rename therefore deletes the destination
/// first (the pattern `auth/storage.rs` shipped first) and retries with two
/// short back-offs for scanners that hold the file for a few milliseconds.
///
/// On final failure the tmp file is removed (no litter) and the error is
/// returned — callers decide severity, but MUST at least log it (errors
/// never pass silently).
pub fn replace_file(tmp: &Path, dest: &Path) -> io::Result<()> {
    let result = replace_file_inner(tmp, dest);
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file_inner(tmp: &Path, dest: &Path) -> io::Result<()> {
    std::fs::rename(tmp, dest)
}

#[cfg(windows)]
fn replace_file_inner(tmp: &Path, dest: &Path) -> io::Result<()> {
    let mut last = match std::fs::rename(tmp, dest) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    for backoff_ms in [0u64, 10, 50] {
        if backoff_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
        }
        // Delete-first: marks an open-with-delete-sharing file for deletion
        // and clears the way for a plain rename; harmless when absent.
        let _ = std::fs::remove_file(dest);
        match std::fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common contract on every platform: replace over an existing
    /// destination, create a missing one, and error (cleaning the tmp)
    /// when the tmp itself is missing.
    #[test]
    fn replace_file_commits_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("target.json");
        let tmp = dir.path().join("target.json.tmp");

        // Create-missing.
        std::fs::write(&tmp, b"v1").unwrap();
        replace_file(&tmp, &dest).expect("create");
        assert_eq!(std::fs::read(&dest).unwrap(), b"v1");
        assert!(!tmp.exists(), "tmp must be consumed");

        // Replace-existing.
        std::fs::write(&tmp, b"v2").unwrap();
        replace_file(&tmp, &dest).expect("replace");
        assert_eq!(std::fs::read(&dest).unwrap(), b"v2");
        assert!(!tmp.exists());

        // Missing tmp → error, dest untouched.
        let err = replace_file(&tmp, &dest).expect_err("missing tmp must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(std::fs::read(&dest).unwrap(), b"v2");
    }
}
