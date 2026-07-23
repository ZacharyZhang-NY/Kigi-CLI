//! Hermetic git helpers for tests.
//!
//! Under `bazel test`, `GIT_BIN_PATH` points at a Bazel-provided static git.
//! Helpers prepend that binary's directory to `PATH` so `Command::new("git")`
//! resolves to it instead of a system install.

use std::path::{Path, PathBuf};
use std::sync::Once;

static HERMETIC_GIT_INIT: Once = Once::new();

/// Prepend the hermetic git binary directory to `PATH`.
/// Idempotent — only the first call mutates `PATH`.
pub fn ensure_hermetic_git_on_path() {
    HERMETIC_GIT_INIT.call_once(|| {
        if let Ok(git_bin) = std::env::var("GIT_BIN_PATH") {
            let git_path = PathBuf::from(&git_bin);
            let git_path = if git_path.is_relative() {
                std::env::current_dir().unwrap().join(&git_path)
            } else {
                git_path
            };
            if let Some(bin_dir) = git_path.parent() {
                let current_path = std::env::var("PATH").unwrap_or_default();
                // SAFETY: once via `Once`, before any child processes spawn.
                unsafe {
                    std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), current_path));
                }
            }
        }
    });
}

/// Put hermetic git on `PATH` at the top of tests that spawn `git`.
///
/// ```ignore
/// #[test]
/// fn my_git_test() {
///     kigi_test_utils::require_git!();
///     // ... git commands work here ...
/// }
/// ```
#[macro_export]
macro_rules! require_git {
    () => {
        $crate::git::ensure_hermetic_git_on_path();
    };
}

/// Init a fresh repo at `path` with dummy user config (hermetic git).
pub fn init_git_repo(path: &Path) {
    ensure_hermetic_git_on_path();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["init"])
        .output()
        .unwrap();

    std::process::Command::new("git")
        .current_dir(path)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .unwrap();

    std::process::Command::new("git")
        .current_dir(path)
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();
}

/// Stage all files and create a commit (hermetic git).
pub fn git_commit_all(path: &Path, message: &str) {
    ensure_hermetic_git_on_path();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["add", "."])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["commit", "-m", message])
        .output()
        .unwrap();
}

/// Run git in `dir` with a fixed author/committer; assert success; return
/// trimmed stdout (hermetic git).
pub fn run_git(dir: &Path, args: &[&str]) -> String {
    run_git_with_env(dir, args, &[])
}

/// Like [`run_git`], with extra env vars (e.g. `GIT_SEQUENCE_EDITOR`).
/// Masks global/system git config and disables credential prompts so local
/// `commit.gpgsign` / `core.hooksPath` / `rebase.autoSquash` cannot skew
/// tests. `envs` is applied last and may override any of this.
pub fn run_git_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
    ensure_hermetic_git_on_path();
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Write a grouped fan-out of ~`files` files under `dir` (`files_per_dir`
/// per directory, directories bucketed 100 per group). No git ops.
pub fn write_fanout_tree(dir: &Path, files: usize, files_per_dir: usize) {
    for d in 0..files.div_ceil(files_per_dir) {
        let sub = dir.join(format!("g{}", d / 100)).join(format!("d{d}"));
        std::fs::create_dir_all(&sub).expect("create populated dir");
        for f in 0..files_per_dir {
            std::fs::write(
                sub.join(format!("file_{f}.txt")),
                format!("content {d} {f}\n"),
            )
            .expect("write populated file");
        }
    }
}

/// Create `feature` with `picks` one-file commits off HEAD, advance the base
/// by one commit (so rebase has work), leave `feature` checked out.
/// Returns the base branch name.
pub fn make_feature_branch(dir: &Path, picks: usize) -> String {
    let base = run_git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    run_git(dir, &["checkout", "-b", "feature"]);
    for k in 0..picks {
        let name = format!("pick_{k}.txt");
        std::fs::write(dir.join(&name), format!("pick {k}\n")).expect("write pick file");
        run_git(dir, &["add", &name]);
        run_git(dir, &["commit", "-m", &format!("pick {k}")]);
    }
    run_git(dir, &["checkout", &base]);
    std::fs::write(dir.join("base_advance.txt"), "advance\n").expect("write base advance file");
    run_git(dir, &["add", "base_advance.txt"]);
    run_git(dir, &["commit", "-m", "advance base"]);
    run_git(dir, &["checkout", "feature"]);
    base
}
