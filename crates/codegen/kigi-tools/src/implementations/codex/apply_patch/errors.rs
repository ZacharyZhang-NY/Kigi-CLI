//! Error types for the codex apply-patch engine.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, PartialEq, Clone, Error)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}

#[derive(Debug, Error)]
pub enum ApplyPatchError {
    #[error(transparent)]
    Parse(#[from] ParseError),

    /// A context line or old-lines block could not be located in the file.
    #[error("{0}")]
    ComputeReplacements(String),

    /// The underlying `std::io::Error` is flattened into strings so that this
    /// enum can implement `PartialEq` for tests.
    #[error("{context}: {message}")]
    Io {
        context: String,
        message: String,
        path: Option<PathBuf>,
    },
}

impl PartialEq for ApplyPatchError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Parse(a), Self::Parse(b)) => a == b,
            (Self::ComputeReplacements(a), Self::ComputeReplacements(b)) => a == b,
            (
                Self::Io {
                    context: ca,
                    message: ma,
                    path: pa,
                },
                Self::Io {
                    context: cb,
                    message: mb,
                    path: pb,
                },
            ) => ca == cb && ma == mb && pa == pb,
            _ => false,
        }
    }
}
