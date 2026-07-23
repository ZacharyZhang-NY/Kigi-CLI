use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use kigi_workspace::util::is_lock_contended;

use crate::util::kigi_home::kigi_home;

/// Env var that overrides the leader socket path (and, by extension, the lock
/// path — the sibling `.lock`). Set by the `--leader-socket` flag, or exported
/// directly. Lets a developer sandbox a leader instance away from the default
/// `~/.kigi/leader.sock` — e.g. run a local branch build's leader without
/// colliding with an installed stable leader on the same machine. Honored by
/// BOTH the client (`connect_or_spawn`) and the leader (`run_leader`), and
/// inherited by the spawned leader subprocess, so all parties bind the same
/// path.
pub const LEADER_SOCKET_ENV: &str = "KIGI_LEADER_SOCKET";

fn leader_socket_override() -> Option<PathBuf> {
    std::env::var_os(LEADER_SOCKET_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The lock path paired with a given socket path: the sibling file with a
/// `.lock` extension (`/x/leader-foo.sock` → `/x/leader-foo.lock`). Matches the
/// default `leader.sock`/`leader.lock` pairing so the two never disagree.
fn lock_path_for_socket(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// Resolve the socket path: the explicit override wins, else the default
/// `leader.sock` under `root`. Pure (the override is passed in) so it is
/// unit-testable without touching process env.
fn resolve_socket_path(override_socket: Option<PathBuf>, root: &Path) -> PathBuf {
    override_socket.unwrap_or_else(|| default_socket_path_in(root))
}

/// Resolve the lock path: the sibling `.lock` of the override socket if set,
/// else the default `leader.lock` under `root`. Pure (see
/// [`resolve_socket_path`]).
fn resolve_lock_path(override_socket: Option<PathBuf>, root: &Path) -> PathBuf {
    match override_socket {
        Some(socket) => lock_path_for_socket(&socket),
        None => default_lock_path_in(root),
    }
}

pub fn default_lock_path_in(root: &Path) -> PathBuf {
    root.join("leader.lock")
}

/// Effective leader lock path: the [`LEADER_SOCKET_ENV`] override's sibling
/// `.lock` when set, else the default under kigi home.
pub fn default_lock_path() -> PathBuf {
    resolve_lock_path(leader_socket_override(), &kigi_home())
}

pub fn default_socket_path_in(root: &Path) -> PathBuf {
    root.join("leader.sock")
}

/// Effective leader socket path: the [`LEADER_SOCKET_ENV`] override when set,
/// else the default under kigi home.
pub fn default_socket_path() -> PathBuf {
    resolve_socket_path(leader_socket_override(), &kigi_home())
}

/// The instance suffix encoded in a lock/socket file-name pair
/// (`leader<suffix>.lock` / `leader<suffix>.sock`). Empty suffix = the default
/// instance; non-default instances only arise via [`LEADER_SOCKET_ENV`].
/// `None` when the two file names disagree.
pub fn socket_suffix_from_paths(lock_path: &Path, socket_path: &Path) -> Option<String> {
    let lock_name = lock_path.file_name()?.to_str()?;
    let socket_name = socket_path.file_name()?.to_str()?;
    let lock_suffix = lock_name
        .strip_prefix("leader")?
        .strip_suffix(".lock")?
        .to_string();
    let socket_suffix = socket_name
        .strip_prefix("leader")?
        .strip_suffix(".sock")?
        .to_string();

    if lock_suffix == socket_suffix {
        Some(lock_suffix)
    } else {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Lock held by another process")]
    AlreadyLocked,
    #[error("Timed out waiting to acquire lock after {0:?}")]
    Timeout(Duration),
}

/// Lock manager for the leader process using OS-level file locking (flock).
///
/// The lock file serves two purposes:
/// 1. Exclusive lock indicates who is the leader (or who is spawning)
/// 2. File contents store the leader's PID for diagnostics
///
/// Lock semantics:
/// - Leader holds exclusive lock for its entire lifetime
/// - Clients use try_lock to check if leader exists and coordinate spawning
///
/// Cleanup behavior:
/// - If lock is held when dropped (crash/exit), files are cleaned up
/// - If `release()` is called before drop, files are NOT cleaned up (handoff to leader)
#[derive(Debug)]
pub struct LeaderLock {
    lock_path: PathBuf,
    sock_path: PathBuf,
    lock_file: Option<File>,
    /// Tracks if we should clean up files on drop.
    /// Set to true when lock is acquired, set to false when explicitly released.
    /// This ensures cleanup happens if we crash while holding the lock,
    /// but NOT if we explicitly hand off to another process via release().
    was_leader: bool,
}

impl LeaderLock {
    pub fn new() -> Self {
        Self {
            lock_path: default_lock_path(),
            sock_path: default_socket_path(),
            lock_file: None,
            was_leader: false,
        }
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.sock_path
    }

    pub fn lock_path(&self) -> &PathBuf {
        &self.lock_path
    }

    fn open_lock_file(&self) -> Result<File, LockError> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?)
    }

    fn mark_acquired(&mut self, file: File) {
        self.lock_file = Some(file);
        self.was_leader = true;
    }

    /// Try to acquire exclusive lock without blocking.
    ///
    /// Returns `Ok(true)` if lock acquired, `Ok(false)` if already held by another process.
    /// After acquiring, call `write_pid()` to record the leader's PID.
    pub fn try_acquire(&mut self) -> Result<bool, LockError> {
        let file = self.open_lock_file()?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                self.mark_acquired(file);
                Ok(true)
            }
            Err(e) if is_lock_contended(&e) => Ok(false),
            Err(e) => Err(LockError::Io(e)),
        }
    }

    /// Acquire exclusive lock, blocking until available.
    ///
    /// Used by the leader process on startup. After acquiring, call `write_pid()`
    /// to record the leader's PID.
    pub fn acquire_blocking(&mut self) -> Result<(), LockError> {
        let file = self.open_lock_file()?;

        file.lock_exclusive()?;
        self.mark_acquired(file);
        Ok(())
    }

    /// Try to acquire exclusive lock with a timeout.
    ///
    /// Polls `try_lock_exclusive()` every 200ms until the lock is acquired or the
    /// timeout elapses. Returns `LockError::Timeout` if the deadline is exceeded.
    ///
    /// Used by the leader subprocess in the socket-then-lock startup flow: the
    /// spawning client holds the lock while the leader binds its IPC socket, then
    /// releases it. This method waits for that handoff, but gives up after `timeout`
    /// so a duplicate leader (started while another is already running) exits
    /// cleanly instead of blocking forever.
    pub fn try_acquire_timeout(&mut self, timeout: Duration) -> Result<(), LockError> {
        let file = self.open_lock_file()?;

        let deadline = Instant::now() + timeout;
        let poll_interval = Duration::from_millis(200);

        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    self.mark_acquired(file);
                    return Ok(());
                }
                Err(e) if is_lock_contended(&e) => {
                    if Instant::now() >= deadline {
                        return Err(LockError::Timeout(timeout));
                    }
                    std::thread::sleep(poll_interval);
                }
                Err(e) => return Err(LockError::Io(e)),
            }
        }
    }

    /// Write our PID to the lock file. Call after acquiring lock.
    pub fn write_pid(&mut self) -> Result<(), LockError> {
        if let Some(ref mut file) = self.lock_file {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            file.sync_all()?;
        }
        Ok(())
    }

    pub fn read_pid(&self) -> Option<u32> {
        Self::read_pid_from_path(&self.lock_path)
    }

    pub fn read_pid_from_path(path: &Path) -> Option<u32> {
        let mut content = String::new();
        File::open(path)
            .and_then(|mut f| f.read_to_string(&mut content))
            .ok()?;
        content.trim().parse().ok()
    }

    /// Delete the socket file. Call while holding the lock.
    pub fn cleanup_socket(&self) -> io::Result<()> {
        match fs::remove_file(&self.sock_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Release the lock explicitly.
    ///
    /// This is used by the spawner to release the lock after the leader has bound
    /// its socket. After calling this, the `Drop` impl will NOT clean up files,
    /// since we're intentionally handing off to the leader process.
    pub fn release(&mut self) -> io::Result<()> {
        if let Some(file) = self.lock_file.take() {
            file.unlock()?;
        }
        self.was_leader = false;
        Ok(())
    }

    pub fn is_held(&self) -> bool {
        self.lock_file.is_some()
    }
}

impl Default for LeaderLock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl LeaderLock {
    /// Bind a lock to explicit paths for tests running outside the default home.
    pub(crate) fn from_paths(lock_path: PathBuf, sock_path: PathBuf) -> Self {
        Self {
            lock_path,
            sock_path,
            lock_file: None,
            was_leader: false,
        }
    }
}

impl Drop for LeaderLock {
    fn drop(&mut self) {
        // Lock is automatically released when file is closed.
        if self.was_leader {
            let _ = fs::remove_file(&self.lock_path);
            let _ = fs::remove_file(&self.sock_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_lock(temp: &TempDir) -> LeaderLock {
        LeaderLock::from_paths(
            temp.path().join("leader.lock"),
            temp.path().join("leader.sock"),
        )
    }

    #[test]
    fn override_socket_path_wins_over_default_derivation() {
        let root = Path::new("/home/u/.kigi");
        let override_sock = PathBuf::from("/home/u/.kigi/leader-branch.sock");

        assert_eq!(
            resolve_socket_path(Some(override_sock.clone()), root),
            override_sock
        );
        // The lock is the sibling `.lock`, NOT the default name.
        assert_eq!(
            resolve_lock_path(Some(override_sock), root),
            PathBuf::from("/home/u/.kigi/leader-branch.lock")
        );
    }

    #[test]
    fn no_override_falls_back_to_default_paths() {
        let root = Path::new("/home/u/.kigi");
        assert_eq!(resolve_socket_path(None, root), root.join("leader.sock"));
        assert_eq!(resolve_lock_path(None, root), root.join("leader.lock"));
    }

    #[test]
    fn lock_path_for_socket_swaps_extension() {
        assert_eq!(
            lock_path_for_socket(Path::new("/x/leader-foo.sock")),
            PathBuf::from("/x/leader-foo.lock")
        );
        // A socket path without an extension still gets a `.lock` sibling.
        assert_eq!(
            lock_path_for_socket(Path::new("/x/myleader")),
            PathBuf::from("/x/myleader.lock")
        );
    }

    #[test]
    fn try_acquire_succeeds_when_unlocked() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        assert!(lock.try_acquire().unwrap());
        assert!(lock.is_held());
    }

    #[test]
    fn try_acquire_fails_when_held() {
        let temp = TempDir::new().unwrap();
        let mut lock1 = test_lock(&temp);
        let mut lock2 = test_lock(&temp);

        assert!(lock1.try_acquire().unwrap());
        // Should return false, not error
        assert!(!lock2.try_acquire().unwrap());
    }

    #[test]
    fn write_and_read_pid() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        lock.try_acquire().unwrap();
        lock.write_pid().unwrap();

        let pid = lock.read_pid().unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(
            LeaderLock::read_pid_from_path(lock.lock_path()),
            Some(std::process::id())
        );
    }

    #[test]
    fn default_lock_and_socket_paths_have_empty_suffix() {
        let root = Path::new("/home/u/.kigi");
        let lock_path = default_lock_path_in(root);
        let socket_path = default_socket_path_in(root);

        assert_eq!(
            socket_suffix_from_paths(&lock_path, &socket_path),
            Some(String::new())
        );
    }

    #[test]
    fn override_paths_yield_matching_non_default_suffix() {
        let override_sock = PathBuf::from("/x/leader-branch.sock");
        let lock = lock_path_for_socket(&override_sock);
        assert_eq!(
            socket_suffix_from_paths(&lock, &override_sock),
            Some("-branch".to_string())
        );
    }

    #[test]
    fn mismatched_suffixes_yield_none() {
        assert!(
            socket_suffix_from_paths(Path::new("/x/leader-a.lock"), Path::new("/x/leader-b.sock"))
                .is_none()
        );
    }

    #[test]
    fn cleanup_socket_removes_file() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        fs::write(&lock.sock_path, "").unwrap();
        assert!(lock.sock_path.exists());

        lock.try_acquire().unwrap();
        lock.cleanup_socket().unwrap();

        assert!(!lock.sock_path.exists());
    }

    #[test]
    fn cleanup_socket_ok_if_missing() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        lock.try_acquire().unwrap();
        lock.cleanup_socket().unwrap();
    }

    #[test]
    fn release_allows_reacquisition() {
        let temp = TempDir::new().unwrap();
        let mut lock1 = test_lock(&temp);
        let mut lock2 = test_lock(&temp);

        assert!(lock1.try_acquire().unwrap());
        lock1.release().unwrap();

        assert!(lock2.try_acquire().unwrap());
    }

    #[test]
    fn drop_releases_lock() {
        let temp = TempDir::new().unwrap();
        let mut lock2 = test_lock(&temp);

        {
            let mut lock1 = test_lock(&temp);
            assert!(lock1.try_acquire().unwrap());
            // lock1 dropped here
        }

        // lock2 should be able to acquire now
        assert!(lock2.try_acquire().unwrap());
    }

    #[test]
    fn release_prevents_file_cleanup_on_drop() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        // Create socket file (simulating leader binding)
        fs::write(&lock.sock_path, "").unwrap();
        assert!(lock.sock_path.exists());

        // Acquire and then release (simulating spawner handoff)
        assert!(lock.try_acquire().unwrap());
        lock.release().unwrap();

        drop(lock);

        assert!(
            temp.path().join("leader.sock").exists(),
            "Socket file should NOT be deleted after release()"
        );
    }

    #[test]
    fn drop_without_release_cleans_up_files() {
        let temp = TempDir::new().unwrap();

        {
            let mut lock = test_lock(&temp);

            fs::write(&lock.sock_path, "").unwrap();
            assert!(lock.sock_path.exists());

            // Acquire but do NOT release (simulating crash/normal exit)
            assert!(lock.try_acquire().unwrap());
            // lock dropped here without release()
        }

        assert!(
            !temp.path().join("leader.sock").exists(),
            "Socket file SHOULD be deleted when dropped without release()"
        );
    }

    #[test]
    fn read_pid_returns_none_for_missing_lock_file() {
        let temp = TempDir::new().unwrap();
        let lock = test_lock(&temp);
        // Lock file doesn't exist yet
        assert!(lock.read_pid().is_none());
        assert!(LeaderLock::read_pid_from_path(lock.lock_path()).is_none());
    }

    #[test]
    fn read_pid_returns_none_for_empty_lock_file() {
        let temp = TempDir::new().unwrap();
        let lock = test_lock(&temp);
        fs::write(lock.lock_path(), "").unwrap();
        assert!(lock.read_pid().is_none());
    }

    #[test]
    fn read_pid_returns_none_for_non_numeric_content() {
        let temp = TempDir::new().unwrap();
        let lock = test_lock(&temp);
        fs::write(lock.lock_path(), "not-a-pid").unwrap();
        assert!(lock.read_pid().is_none());
    }

    #[test]
    fn try_acquire_timeout_succeeds_when_unlocked() {
        let temp = TempDir::new().unwrap();
        let mut lock = test_lock(&temp);

        lock.try_acquire_timeout(Duration::from_secs(1)).unwrap();
        assert!(lock.is_held());
    }

    #[test]
    fn try_acquire_timeout_returns_timeout_when_held() {
        let temp = TempDir::new().unwrap();
        let mut lock1 = test_lock(&temp);
        let mut lock2 = test_lock(&temp);

        assert!(lock1.try_acquire().unwrap());

        let result = lock2.try_acquire_timeout(Duration::from_millis(500));
        assert!(
            matches!(result, Err(LockError::Timeout(_))),
            "Expected Timeout error, got {:?}",
            result
        );
        assert!(!lock2.is_held());
    }

    #[test]
    fn try_acquire_timeout_succeeds_after_release() {
        let temp = TempDir::new().unwrap();
        let mut lock1 = test_lock(&temp);
        let mut lock2 = test_lock(&temp);

        assert!(lock1.try_acquire().unwrap());

        let lock_path = lock1.lock_path.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            lock1.release().unwrap();
            // keep the path for verification, lock1 is consumed
            lock_path
        });

        // lock2 should acquire within the timeout because lock1 is released after 200ms
        lock2.try_acquire_timeout(Duration::from_secs(5)).unwrap();
        assert!(lock2.is_held());

        handle.join().unwrap();
    }
}
