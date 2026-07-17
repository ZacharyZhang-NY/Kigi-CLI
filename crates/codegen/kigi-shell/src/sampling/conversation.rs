//! API-agnostic conversation representation.
//!
//! The canonical types now live in `kigi_sampling_types::conversation`.
//! This module re-exports them.

// Re-export everything from the standalone crate.
pub use kigi_sampling_types::conversation::*;

// Tests for conversation types now live in kigi-sampling-types crate.
