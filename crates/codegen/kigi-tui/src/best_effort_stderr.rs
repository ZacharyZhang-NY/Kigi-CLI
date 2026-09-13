//! Stderr lines for paths where fd 2 may be dead: a closed pane fails every write, `eprintln!` panics on it, and under `panic = "abort"` that is a SIGABRT plus a crash report.

use std::io::Write;

/// Write `line` and a newline to `w`; `false` when the write failed.
pub fn write_line(w: &mut impl Write, line: &str) -> bool {
    writeln!(w, "{line}").is_ok()
}

/// [`write_line`] to process stderr, outcome discarded.
pub fn eprint_line(line: &str) {
    write_line(&mut std::io::stderr(), line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_line_appends_a_newline_and_reports_success() {
        let mut buf = Vec::new();
        assert!(write_line(&mut buf, "Finishing session…"));
        assert_eq!(buf, "Finishing session…\n".as_bytes());
    }

    /// A closed read end fails every write with EPIPE, like the dead tty a closed pane leaves.
    #[test]
    fn write_line_reports_failure_on_a_dead_pipe() {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        drop(reader);
        assert!(!write_line(&mut writer, "unreachable"));
    }
}
