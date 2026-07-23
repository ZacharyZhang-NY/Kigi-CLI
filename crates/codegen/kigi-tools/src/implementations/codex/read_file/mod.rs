//! Codex `read_file` — text file reader in codex `L{n}: {content}` format.
//!
//! Port of the codex read_file tool, exposed as its own tool under
//! `ToolNamespace::Codex`. It supports two modes:
//!
//! - **Slice mode** — reads a contiguous range of lines (default).
//! - **Indentation mode** — reads a block based on indentation structure.

pub mod indentation;
pub mod slice;
pub(crate) mod text_utils;
pub mod tool;

pub use tool::{CodexReadFileInput, CodexReadFileTool};
