//! Compaction policy — threshold, model, and memory flush configuration.

/// Controls when and how the session's conversation is compacted to free up
/// context window space, and whether a memory flush runs before each compaction.
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Percentage of context window that triggers auto-compaction.
    pub auto_compact_threshold_percent: u32,

    /// `None` uses the session's current model.
    pub compact_model: Option<String>,

    /// Run a memory flush turn before each compaction: the session actor asks
    /// the model to summarize important information from the conversation
    /// before it is discarded. Requires the memory system to be enabled.
    pub memory_flush_enabled: bool,

    /// Per-compaction wall-clock budget (seconds); a generation exceeding it is
    /// cut and retried — the backstop for reasoning runaways token limits miss.
    pub wall_clock_budget_secs: u64,

    /// Prefire two-pass compaction: when usage approaches the threshold,
    /// speculatively summarize the history prefix in the background (pass 1);
    /// at compaction, summarize NOTE₁ + the recent tail (pass 2). `false`
    /// selects the single-pass path. Real sessions resolve this from the
    /// `two_pass_compaction` config flag at session build.
    pub two_pass_enabled: bool,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            auto_compact_threshold_percent: 85,
            compact_model: None,
            memory_flush_enabled: false,
            wall_clock_budget_secs: 300,
            two_pass_enabled: false,
        }
    }
}
