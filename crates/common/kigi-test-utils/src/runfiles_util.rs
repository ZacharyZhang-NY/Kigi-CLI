//! Bazel runfiles helpers for test data.
//!
//! Under `bazel test`, data lives in the runfiles tree; under `cargo test`,
//! `CARGO_MANIFEST_DIR` is the crate root. [`crate_root!`] covers both.

use std::path::PathBuf;

/// Resolve a runfiles path to an absolute directory when the `bazel` feature
/// is on and the entry exists; otherwise `None`.
pub fn try_resolve_runfiles(_path: &str) -> Option<PathBuf> {
    #[cfg(feature = "bazel")]
    {
        let r = runfiles::Runfiles::create().ok()?;
        runfiles::rlocation!(r, _path)
    }
    #[cfg(not(feature = "bazel"))]
    {
        None
    }
}

/// Crate root under both `bazel test` (runfiles) and `cargo test`
/// (`CARGO_MANIFEST_DIR`).
///
/// # Example
///
/// ```ignore
/// use kigi_test_utils::crate_root;
///
/// fn test_data_dir() -> std::path::PathBuf {
///     crate_root!("_main/crates/common/kigi-test-utils").join("testdata")
/// }
/// ```
#[macro_export]
macro_rules! crate_root {
    ($runfiles_path:expr) => {
        $crate::runfiles_util::try_resolve_runfiles($runfiles_path)
            .unwrap_or_else(|| ::std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    };
}
