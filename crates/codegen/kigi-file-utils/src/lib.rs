//! Local file utilities: per-turn event tracking, content hashing, and
//! project-directory classification.
pub mod events;
pub mod s3;
pub mod trace_context;
pub mod workspace_classifier;

/// Directory names that are always skipped when scanning a workspace for
/// project content (dependency caches, build output, editor state, …).
pub const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".env",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".output",
    ".cache",
    ".parcel-cache",
    ".turbo",
    "vendor",
    "bower_components",
    ".tox",
    ".nox",
    ".eggs",
    ".idea",
    ".vscode",
    ".gradle",
    ".dart_tool",
    "coverage",
    ".nyc_output",
    "htmlcov",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

/// [`SKIP_DIR_NAMES`] as a set for O(1) membership checks.
pub fn skip_dir_set() -> &'static std::collections::HashSet<&'static str> {
    use std::collections::HashSet;
    use std::sync::LazyLock;
    static SET: LazyLock<HashSet<&str>> =
        LazyLock::new(|| SKIP_DIR_NAMES.iter().copied().collect());
    &SET
}

/// Compute SHA256 hash of content as a hex string.
pub fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}
/// Compute SHA256 hash of a file by streaming, without loading entire file into memory.
/// If `max_bytes` is set (> 0), only hash up to that many bytes.
pub fn sha256_hex_from_file(
    path: &std::path::Path,
    max_bytes: Option<u64>,
) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut reader: Box<dyn Read> = if let Some(limit) = max_bytes {
        Box::new(file.take(limit))
    } else {
        Box::new(file)
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
