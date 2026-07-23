//! Markdown-based memory storage that persists knowledge across sessions.
//!
//! ## Data Layout
//!
//! ```text
//! ~/.kigi/memory/
//!   ├── MEMORY.md                         # Global curated knowledge
//!   └── {workspace_hash}/                 # Per-workspace (blake3(cwd)[..16])
//!       ├── MEMORY.md                     # Project-level curated knowledge
//!       └── sessions/
//!           └── YYYY-MM-DD-{slug}-{sid8}.md  # Session logs
//! ```
//!
//! ## Feature Flag
//!
//! Memory is gated behind the `--experimental-memory` CLI flag or
//! `KIGI_MEMORY=1`; when disabled the host never initializes this crate.

pub mod archive;
pub mod backend;
pub mod chunker;
pub mod dream;
pub mod dream_lock;
pub mod embedding;
pub mod index;
pub mod mmr;
pub mod query_expansion;
pub mod schema;
pub mod search;
pub mod storage;
pub mod text_utils;
pub mod watcher;

pub use backend::{MemoryBackendImpl, MemoryBackendParams};
pub use index::{MemoryIndex, init_sqlite_vec};
pub use storage::{MemoryScope, MemoryStorage};

/// Embeds every chunk that has no embedding yet, returning how many succeeded.
///
/// The async glue between the sync `MemoryIndex` and the async
/// `EmbeddingProvider`. Call after reindex, flush writes, or session-end
/// writes. Failures are logged and skipped rather than propagated, so a dead
/// embedding endpoint degrades search instead of breaking the session.
pub async fn embed_missing_chunks(
    index: &MemoryIndex,
    provider: &dyn embedding::EmbeddingProvider,
) -> usize {
    let chunks = match index.chunks_without_embeddings() {
        Ok(c) if c.is_empty() => return 0,
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: kigi_log::memory_log::TARGET,
                error = %e,
                "failed to query chunks without embeddings"
            );
            return 0;
        }
    };

    let total = chunks.len();
    let mut embedded = 0;

    // 32 matches the typical provider max batch size.
    for batch in chunks.chunks(32) {
        let texts: Vec<&str> = batch.iter().map(|(_, text)| text.as_str()).collect();
        match provider.embed_batch(&texts).await {
            Ok(embeddings) => {
                for ((chunk_id, _), embedding) in batch.iter().zip(embeddings.iter()) {
                    if let Err(e) = index.upsert_embedding(chunk_id, embedding) {
                        tracing::warn!(
                            target: kigi_log::memory_log::TARGET,
                            chunk_id,
                            error = %e,
                            "failed to upsert embedding"
                        );
                    } else {
                        embedded += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: kigi_log::memory_log::TARGET,
                    error = %e,
                    batch_size = texts.len(),
                    "embedding batch failed, skipping"
                );
            }
        }
    }

    if embedded > 0 {
        tracing::info!(
            target: kigi_log::memory_log::TARGET,
            embedded,
            total,
            "embedded missing chunks"
        );
    }
    embedded
}
