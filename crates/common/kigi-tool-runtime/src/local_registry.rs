//! In-process tool registry for local dispatch.
//!
//! [`LocalRegistry`] maps [`ToolId`]s to type-erased
//! [`ToolDyn`](crate::tool::ToolDyn) handles.
//! Toolset finalization registers every config-enabled tool here and
//! dispatch resolves handles via [`LocalRegistry::find`], so a call
//! executes in-process without any wire round-trip.

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::RwLock;

use crate::context::ListToolsContext;
use crate::tool::{ArcTool, Tool};
use kigi_tool_protocol::ToolId;
use kigi_tool_types::ToolDescription;

/// In-process registry of tool handles.
///
/// Mutations are concurrency-safe (`RwLock` on the entry map), so
/// callers MAY hot-add or hot-remove tools while dispatch is in use.
///
/// Entries use `RwLock<IndexMap>` to preserve insertion order so that
/// [`list_tools`](Self::list_tools) returns descriptions in the same
/// order tools were registered (matching the config-defined order).
#[derive(Clone, Default)]
pub struct LocalRegistry {
    entries: Arc<RwLock<IndexMap<ToolId, ArcTool>>>,
}

impl std::fmt::Debug for LocalRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRegistry")
            .field("entries", &self.entries.read().len())
            .finish()
    }
}

impl LocalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed [`Tool`] by value. A later registration of the
    /// same id replaces the previous handle and returns it.
    pub fn register<T>(&self, tool: T) -> Option<ArcTool>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    /// Register a typed [`Tool`] already wrapped in `Arc`.
    pub fn register_arc<T>(&self, tool: Arc<T>) -> Option<ArcTool>
    where
        T: Tool + 'static,
    {
        let id = tool.id();
        self.entries.write().insert(id, tool as ArcTool)
    }

    /// Register a type-erased [`ToolDyn`](crate::tool::ToolDyn) directly.
    ///
    /// Use for inherently dynamic tools (e.g. MCP tools as
    /// `Arc<dyn ToolDyn>`) where the concrete type is unavailable. For
    /// native tools with a concrete type, prefer [`register`](Self::register).
    pub fn register_dyn(&self, tool: ArcTool) -> Option<ArcTool> {
        let id = tool.id();
        self.entries.write().insert(id, tool)
    }

    /// Resolve `tool_id` to its in-process handle, if registered. Returns
    /// a clone so the caller can dispatch without holding the lock across
    /// an await point.
    pub fn find(&self, tool_id: &ToolId) -> Option<ArcTool> {
        self.entries.read().get(tool_id).cloned()
    }

    /// Drop the handle bound to `tool_id`. Returns `true` iff a matching
    /// entry was found and removed.
    pub fn unregister(&self, tool_id: &ToolId) -> bool {
        self.entries.write().shift_remove(tool_id).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    pub fn contains(&self, tool_id: &ToolId) -> bool {
        self.entries.read().contains_key(tool_id)
    }

    /// Descriptions of registered tools filtered by `should_list`, in
    /// **insertion order**, so the caller sees the same ordering as the
    /// config-defined tool list.
    pub fn list_tools(&self, ctx: &ListToolsContext) -> Vec<ToolDescription> {
        self.entries
            .read()
            .values()
            .filter(|handle| handle.should_list(ctx))
            .map(|handle| handle.description(ctx))
            .collect()
    }
}
