//! Codex `apply_patch`.
//!
//! Ports the codex patch parser, fuzzy matcher, and replacement logic. The
//! [`parser`], [`seek_sequence`], and [`apply`] layers are pure functions over
//! `&str`; [`tool`] is the only layer that touches the filesystem.

pub mod apply;
pub mod errors;
pub mod parser;
pub mod seek_sequence;
pub mod tool;

pub use apply::derive_new_contents;
pub use errors::{ApplyPatchError, ParseError};
pub use parser::{Hunk, ParsedPatch, UpdateFileChunk, parse_patch};
pub use tool::{ApplyPatchInput, ApplyPatchTool};
