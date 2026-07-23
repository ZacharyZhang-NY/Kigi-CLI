//! Context types and the typed-extension store they share.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use kigi_tool_protocol::ToolCallId;

/// Open typed-extension store keyed by `TypeId`.
#[derive(Clone, Default)]
pub struct TypedExtensions {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl TypedExtensions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> &mut Self {
        self.map.insert(TypeId::of::<T>(), Arc::new(value));
        self
    }

    pub fn insert_arc<T: Send + Sync + 'static>(&mut self, value: Arc<T>) -> &mut Self {
        self.map.insert(TypeId::of::<T>(), value);
        self
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|arc| Arc::downcast::<T>(arc).ok())
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<Arc<T>> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|arc| Arc::downcast::<T>(arc).ok())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Copy entries from `defaults` that are not already present in `self`.
    pub fn merge_defaults(&mut self, defaults: &TypedExtensions) {
        for (key, value) in &defaults.map {
            self.map.entry(*key).or_insert_with(|| value.clone());
        }
    }
}

/// Per-call context.
#[derive(Clone)]
pub struct ToolCallContext {
    pub call_id: ToolCallId,
    pub extensions: TypedExtensions,
}

impl Default for ToolCallContext {
    fn default() -> Self {
        Self {
            call_id: ToolCallId::new_v7(),
            extensions: TypedExtensions::new(),
        }
    }
}

impl ToolCallContext {
    pub fn new(call_id: ToolCallId) -> Self {
        Self {
            call_id,
            extensions: TypedExtensions::new(),
        }
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> &mut Self {
        self.extensions.insert(value);
        self
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.extensions.get::<T>()
    }
}

/// Per-turn context consumed by [`crate::Tool::should_list`].
#[derive(Clone, Default)]
pub struct ListToolsContext {
    pub extensions: TypedExtensions,
}

impl ListToolsContext {
    pub fn new() -> Self {
        Self::default()
    }
}

// Runtime-blessed per-concept extensions. One type per concept so
// dispatchers install exactly what they have and tools depend on
// exactly what they need.

/// Working directory for relative path resolution.
#[derive(Clone, Debug)]
pub struct Cwd(pub PathBuf);

/// Opaque behaviour version. Tools that branch on this MUST treat
/// unknown values as a hard error.
#[derive(Clone, Debug)]
pub struct BehaviorVersion(pub String);

/// Distributed-trace correlation context (e.g. W3C `traceparent`).
///
/// Receive-side carrier only: stamped from the inbound wire value for
/// tool impls to read, never serialized back out.
#[derive(Clone, Debug)]
pub struct TraceContext(pub String);

/// Session ID — which hub session this call belongs to.
/// Multi-session tool servers dispatch to the matching per-session state.
#[derive(Clone, Debug)]
pub struct SessionContext(pub String);

/// Cooperative-cancellation handle for the current tool call. Tools MAY
/// poll/await this for graceful shutdown; the dispatcher also hard-cancels
/// by dropping the call future when it fires.
#[derive(Clone, Debug)]
pub struct Cancellation(pub tokio_util::sync::CancellationToken);

/// Per-user feature-flag bag on [`ToolCallContext`]. Dispatcher resolves;
/// tools read. Default is off for every field so an absent extension never
/// opts a feature in. New fields need `#[serde(default)]` so older
/// `session.bind` payloads stay deserializable.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceViewerContext {
    /// When `true`, `BashTool` emits `bash_output_chunk` Progress frames.
    #[serde(default)]
    pub stream_tool_progress: bool,
}
