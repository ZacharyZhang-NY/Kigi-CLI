//! Configuration for inter-compaction.
//!
//! This is a plain data struct — harness-specific service-config integration
//! stays in the compaction subscriber, which resolves config values and
//! constructs this struct.

use serde::{Deserialize, Serialize};

use crate::history::types::CompactionStrategy;

/// Runtime configuration for a single inter-compaction invocation.
///
/// Mirrors the fields used by the between-turn compaction service config,
/// without a harness-specific config-macro dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterCompactionConfig {
    /// The agent/scheduler name to use for the compaction model.
    ///
    /// Model routing is host policy — kept here because service configs
    /// deserialize this struct as-is.
    pub compaction_model_name: String,
    /// End-to-end timeout for the compaction sampling in seconds.
    pub sampling_timeout_secs: u64,
    pub compaction_strategy: CompactionStrategy,
    /// [DivideAndConquer] Max tokens per chunk before sending to the LLM.
    /// (Basic strategy ignores this and emits a single chunk.)
    pub dnc_chunk_token_limit: u32,
    /// User messages with character count > this threshold are truncated
    /// (middle-cut) when assembling the `<kigi_user_queries>` preamble.
    /// Applies to both Basic and DivideAndConquer.
    pub user_message_compact_threshold: u32,
}
