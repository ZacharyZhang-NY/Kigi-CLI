//! Compact jj status for the system prompt.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::file_system::FsError;

const JJ_LOG_TEMPLATE: &str = r#"separate("\n",
  "Change: " ++ change_id.shortest(8),
  "Commit: " ++ commit_id.shortest(8),
  if(description,
    "Description: " ++ description.first_line(),
    "Description: (no description set)"),
  if(bookmarks,
    "Bookmarks: " ++ bookmarks.join(", "),
    "")
)"#;

/// Truncated to roughly 1k chars so the prompt budget stays bounded.
pub async fn jj_status(working_directory: impl Into<PathBuf>) -> Result<String, FsError> {
    let working_directory = working_directory.into();
    tokio::task::spawn_blocking(move || jj_status_impl(&working_directory))
        .await
        .map_err(|e| FsError::Other(format!("jj status task failed: {e}")))?
}

fn jj_status_impl(cwd: &Path) -> Result<String, FsError> {
    let max_chars = 1000;
    let mut out = String::with_capacity(max_chars);

    let log = run_jj(
        cwd,
        &["log", "--no-graph", "-r", "@", "-T", JJ_LOG_TEMPLATE],
    )
    .ok_or_else(|| FsError::Other("not a jujutsu repository".into()))?;

    for line in log.lines().filter(|l| !l.is_empty()) {
        let _ = writeln!(out, "{line}");
    }

    match run_jj(cwd, &["st"]) {
        Some(st) if st.contains("The working copy is clean") || st.is_empty() => {
            let _ = writeln!(out, "\nWorking copy is clean");
        }
        Some(st) => {
            let _ = writeln!(out);
            let budget = max_chars - 50;
            for (i, line) in st.lines().enumerate() {
                if out.len() + line.len() + 1 > budget {
                    let remaining = st.lines().count() - i;
                    if remaining > 0 {
                        let _ = writeln!(out, "... and {remaining} more lines");
                    }
                    break;
                }
                let _ = writeln!(out, "{line}");
            }
        }
        None => {}
    }

    Ok(out)
}

/// `--ignore-working-copy` keeps this read-only: without it jj snapshots the
/// working copy into a new commit as a side effect of the query.
fn run_jj(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("jj");
    cmd.arg("--ignore-working-copy")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    kigi_tools::util::detach_std_command(&mut cmd);
    let output = cmd.output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}
