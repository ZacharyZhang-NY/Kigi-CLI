//! kigi compaction configuration.
//!
//! Tunables and defaults only; trigger *wiring* (pre-sampling checks,
//! preflight overflow, model-switch, suppression) stays per-host. Mirrors
//! [`IntraCompactionConfig`](crate::intra_compaction::IntraCompactionConfig) /
//! [`InterCompactionConfig`](crate::inter_compaction::InterCompactionConfig),
//! which also live in their module's `config.rs`.

/// Applies only when no other source (env var, user config, remote
/// per-model/global flags) sets it. Shared by kigi and Kigi chat.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT: u8 = 85;

/// A cleaned summary shorter than this is treated as degenerate and retried
/// like a transient failure. The smallest healthy prod summary observed was
/// ~3,242 chars, so 500 is a wide margin below anything legitimate.
pub const MIN_SUMMARY_SEED_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct FullReplaceConfig {
    /// First try + retries, counted together.
    pub max_attempts: u32,
    pub retry_delay_secs: u64,
    /// Applies per attempt, not to the whole retry loop.
    pub sampling_timeout_secs: u64,
}

impl Default for FullReplaceConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            retry_delay_secs: 3,
            sampling_timeout_secs: 120,
        }
    }
}
