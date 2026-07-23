//! Legacy (0.4.10) policy for `read_file`: a generic error message for every
//! filesystem failure (no structured variants) and no gitignore enforcement.
//! The execution path stays in `mod.rs`; only version-specific policy lives here.

use std::path::Path;

/// Historical 0.4.10 collapsed all filesystem read failures (missing file,
/// directory path, permission denied) into this generic message without
/// appending OS error detail.
pub(crate) fn render_read_error(path: &Path) -> String {
    format!("Failed to read file: {}", path.display())
}

pub(crate) fn allows_gitignored_reads() -> bool {
    true
}
