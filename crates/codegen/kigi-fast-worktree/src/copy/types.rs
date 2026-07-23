//! Shared types for copy operations.

use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::{DashMap, DashSet};

/// Counts of dirty files in the *source* worktree.
#[derive(Clone, Debug, Default)]
pub struct DirtyFilesReport {
    pub modified_files: u64,
    pub untracked_files: u64,
    pub deleted_files: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CopyStats {
    pub files_copied: u64,
    pub dirs_created: u64,
    pub symlinks_copied: u64,
    pub files_skipped: u64,
    /// Non-fatal issues encountered while copying.
    pub issues: Vec<String>,
}

impl CopyStats {
    pub fn merge(&mut self, other: CopyStats) {
        self.files_copied += other.files_copied;
        self.dirs_created += other.dirs_created;
        self.symlinks_copied += other.symlinks_copied;
        self.files_skipped += other.files_skipped;
        self.issues.extend(other.issues);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopyEntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug)]
pub(crate) struct CopyEntry {
    pub(crate) rel_path: PathBuf,
    pub(crate) kind: CopyEntryKind,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ParallelCopyConfig {
    /// 0 means "one per CPU".
    pub num_workers: usize,
    /// Channel buffer size, per shard.
    pub channel_buffer: usize,
    /// Paths relative to the source root.
    pub skip_files: Option<Arc<DashSet<PathBuf>>>,
    pub respect_gitignore: bool,
    /// Globs, applied on top of `skip_files`.
    pub skip_patterns: Vec<String>,
}

/// Result of a parallel copy operation, including stats and the set of copied paths.
#[derive(Clone, Debug, Default)]
pub(crate) struct ParallelCopyResult {
    pub stats: CopyStats,
    /// All relative paths that were successfully copied (for deduplication in subsequent copies).
    pub copied_paths: DashSet<PathBuf>,
    /// Metadata for files that were copied (for index updates). Only regular files, not symlinks/dirs.
    pub file_metadata: DashMap<PathBuf, Metadata>,
}
