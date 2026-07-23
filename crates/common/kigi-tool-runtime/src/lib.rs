//! Unified tool runtime contract.
//!
//! Adapters for individual tool sources re-export from here so every tool
//! author sees the same surface.

#![forbid(unsafe_code)]

pub mod context;
pub mod dispatch;
pub mod error;
pub mod local_registry;
pub mod notification;
pub mod render;
pub mod search;
pub mod streaming;
pub mod tool;

pub use context::{
    BehaviorVersion, Cancellation, Cwd, ListToolsContext, SessionContext, ToolCallContext,
    TraceContext, TypedExtensions, WorkspaceViewerContext,
};
pub use dispatch::ToolDispatch;
pub use error::{ToolError, ToolErrorKind};
pub use local_registry::LocalRegistry;
pub use notification::{
    BashExecutionBackgrounded, BashExecutionComplete, BashExecutionFailed, BashExecutionTimeout,
    BashNotificationBase, BashOutputChunk, FileRead, FileWritten, LspServerCrashed,
    LspServerFailed, LspServerReady, LspServerRetrying, LspServerStarting, MonitorEvent,
    PlanModeEntered, PlanModeExited, ScheduledTaskCreated, ScheduledTaskFired,
    ScheduledTaskRemoved, TaskKind, TaskSnapshot, ToolNotification, ToolNotificationHandle,
    UserQuestionAsked,
};
pub use render::{
    ModelOutputExtractor, ToolChatCompletion, ToolChatCompletionResponse, ToolCodeExecutionResult,
    ToolOutput, ToolStreamError, extract_content_blocks, extractor_for,
};
pub use search::{SearchSnapshot, ServerSummary, ToolIndex, ToolSearchIndex, ToolSearchResult};
pub use streaming::{PartialResultPayload, stream_chunk};
pub use tool::{
    ArcTool, ArcToolFamily, ContentBlock, Tool, ToolDyn, ToolFamily, ToolProgress, ToolStream,
    ToolStreamItem, ToolVariant, TypedToolOutput, terminal_only, with_progress,
};

pub use kigi_tool_protocol::{StreamingSpec, ToolCallId, ToolCapabilities, ToolId, ToolScope};
