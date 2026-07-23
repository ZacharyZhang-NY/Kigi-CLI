//! Edge types for the ScopeGraph.

use serde::{Deserialize, Serialize};

/// Edge weight in the ScopeGraph. Every variant is directed source-to-target,
/// in the order its name reads.
#[derive(Serialize, Deserialize, PartialEq, Eq, Copy, Clone, Debug)]
pub enum EdgeKind {
    /// Nested scope to its parent scope.
    ScopeToScope,

    /// Definition to the scope that owns it, which for a hoisted def is the
    /// parent of the scope it was written in.
    DefToScope,

    /// Import to its defining scope.
    ImportToScope,

    RefToDef,

    RefToImport,
}
