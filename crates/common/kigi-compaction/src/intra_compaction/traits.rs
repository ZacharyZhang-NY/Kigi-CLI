use async_trait::async_trait;

use super::trigger::IntraCompactionError;

/// Which segment of the conversation a single intra-compaction pass acts on.
/// Selects both the prompt template and the read-view items are pulled from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTarget {
    /// The agent loop's accumulated step turns: assistant outputs, tool calls,
    /// tool results.
    Steps,
    /// Prior conversation-history turns, from before the current agent loop.
    /// Coarser prompt, shared with inter-compaction.
    History,
    /// Prior history *and* accumulated steps, replaced wholesale by a single
    /// summary. No tail is kept.
    FullReplace,
}

impl CompactionTarget {
    /// Stable metric label for this target.
    pub fn label(self) -> &'static str {
        match self {
            Self::Steps => "steps",
            Self::History => "history",
            Self::FullReplace => "full_replace",
        }
    }
}

/// Minimal interface the compaction orchestrator needs from the agent's
/// stream processor. Implemented by Kigi chat's `StreamProcessor`
/// (`Item = Arc<KigiTurn>`).
///
/// Implementations that don't support a particular target return
/// [`IntraCompactionError::Unsupported`] from the matching match arm.
#[async_trait]
pub trait CompactionStreamProc: Send + Sync {
    /// The harness's conversation item type.
    type Item;

    /// Items accumulated across all completed steps, oldest first. The
    /// original conversation (system prompt, user messages, prior history) is
    /// excluded.
    async fn get_accumulated_turns_for_compaction(&self) -> Vec<Self::Item>;

    /// Items from prior user/assistant exchanges, before the current agent
    /// loop began, oldest first.
    ///
    /// Default impl returns empty — implementations that do not support
    /// history compaction will have nothing to compact.
    async fn get_history_turns_for_compaction(&self) -> Vec<Self::Item> {
        Vec::new()
    }

    /// The whole conversation — prior history followed by accumulated step
    /// turns, oldest first.
    ///
    /// The default composition is correct for any implementation; override
    /// only if a harness can produce the combined view more cheaply.
    ///
    /// The `Self::Item: Send` bound lets the default hold the history vec across
    /// the second `await` while keeping the boxed future `Send`; every concrete
    /// item type (`Arc<KigiTurn>`) already satisfies it.
    async fn get_all_turns_for_compaction(&self) -> Vec<Self::Item>
    where
        Self::Item: Send,
    {
        let mut all = self.get_history_turns_for_compaction().await;
        all.extend(self.get_accumulated_turns_for_compaction().await);
        all
    }

    /// Replaces the first `n_turns_to_remove` items of the read-view selected
    /// by `target` with the single given `compaction_turn`.
    ///
    /// On invalid input (`n_turns_to_remove > view.len()`), returns
    /// [`IntraCompactionError::InvalidSplit`] and leaves state untouched.
    async fn replace_with_compaction(
        &self,
        target: CompactionTarget,
        n_turns_to_remove: usize,
        compaction_turn: Self::Item,
    ) -> Result<(), IntraCompactionError>;
}
