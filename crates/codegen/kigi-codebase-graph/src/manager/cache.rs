//! Index caching for fast loading.
//!
//! The on-disk format is a custom binary layout tagged with the magic bytes
//! "SGIX". Caches written by the earlier bincode format are detected and
//! rejected rather than parsed, so the caller rebuilds from source.

use std::path::Path;

use crate::scope_graph::ScopeGraphIndex;

pub const CACHE_FILE_NAME: &str = ".goto_index.bin";

#[derive(Debug)]
pub enum CacheError {
    IoError(std::io::Error),
    SerializeError(String),
    DeserializeError(String),
    /// A bincode-era cache was found; the caller is expected to rebuild.
    LegacyFormat,
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::IoError(e) => write!(f, "IO error: {}", e),
            CacheError::SerializeError(msg) => write!(f, "Serialization error: {}", msg),
            CacheError::DeserializeError(msg) => write!(f, "Deserialization error: {}", msg),
            CacheError::LegacyFormat => write!(f, "Legacy cache format detected"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<std::io::Error> for CacheError {
    fn from(e: std::io::Error) -> Self {
        CacheError::IoError(e)
    }
}

pub type Result<T> = std::result::Result<T, CacheError>;

pub fn get_cache_path(root_path: &Path) -> std::path::PathBuf {
    root_path.join(CACHE_FILE_NAME)
}

/// Returns `CacheError::LegacyFormat` for a bincode-format cache, signaling to
/// the caller that a rebuild is needed.
pub fn load_index(cache_path: &Path) -> Result<ScopeGraphIndex> {
    if !cache_path.exists() {
        return Err(CacheError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Cache file not found",
        )));
    }

    match ScopeGraphIndex::load(cache_path) {
        Ok(Some(index)) => Ok(index),
        // `Ok(None)` is how the loader reports a legacy-format file.
        Ok(None) => {
            tracing::info!(
                cache_path = %cache_path.display(),
                "Legacy cache format detected, will rebuild"
            );
            Err(CacheError::LegacyFormat)
        }
        Err(e) => Err(CacheError::IoError(e)),
    }
}

pub fn save_index(cache_path: &Path, index: &ScopeGraphIndex) -> Result<()> {
    index.save(cache_path).map_err(CacheError::IoError)
}

/// Saves on a detached thread: the caller gets no join handle and no result,
/// so a failed write is only visible in the logs.
pub fn save_index_async(cache_path: std::path::PathBuf, index: ScopeGraphIndex) {
    std::thread::spawn(move || {
        if let Err(e) = save_index(&cache_path, &index) {
            tracing::warn!("Failed to save index cache: {}", e);
        }
    });
}

pub fn cache_exists(cache_path: &Path) -> bool {
    cache_path.exists()
}

/// Size of the cache file in bytes, or `None` if it cannot be stat'd.
pub fn cache_size(cache_path: &Path) -> Option<u64> {
    std::fs::metadata(cache_path).ok().map(|m| m.len())
}
