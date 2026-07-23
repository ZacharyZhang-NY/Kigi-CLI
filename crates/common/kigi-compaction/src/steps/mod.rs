//! Prompt content for compacting accumulated step turns (tool calls +
//! assistant responses) within a single agent turn.
//!
//! Parallel to [`crate::history`], the history-compaction content. The
//! orchestration that uses it lives in [`crate::intra_compaction`], and the
//! turn selection it shares with the History target is the crate-root
//! [`select`](crate::select) primitive.

pub mod prompt;

pub use prompt::format_compaction_prompt;
