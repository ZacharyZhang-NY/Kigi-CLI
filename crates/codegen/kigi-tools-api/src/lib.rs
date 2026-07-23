//! Shared API definitions for Kigi tools: protobuf types, config validation,
//! and canonical slash-command wording.
//!
//! Used by both the tools library and the gRPC server, and by host services
//! that must not depend on the tools implementation crate.

#![allow(clippy::derive_partial_eq_without_eq)]

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/kigi.tools.v1.rs"));
}

pub mod config_validation;
pub mod slash_commands;

pub use pb::{
    AgentCompletionRequirement, AgentToolExecConfig, AgentToolRetryConfig,
    ClearToolOverrideRequest, ClearToolOverrideResponse, DisableToolRequest, DisableToolResponse,
    EnableToolRequest, EnableToolResponse, ErrorCode, ExecuteToolRequest, ExecuteToolResponse,
    ExecutionMetadata, ExecutionOptions, FinalizeAgentRequest, FinalizeAgentResponse,
    FinalizeConfigValidationDetails, FinalizeConfigViolation, FinalizeToolServerConfigRequest,
    FinalizeToolServerConfigResponse, GetAgentInfoRequest, GetAgentInfoResponse,
    GetCompletionStateRequest, GetCompletionStateResponse, GetSystemPromptRequest,
    GetSystemPromptResponse, GetSystemRemindersRequest, GetSystemRemindersResponse,
    GetToolInfoRequest, GetToolOptionsRequest, GetToolOptionsResponse, GetToolStateRequest,
    GetToolStateResponse, GetTruncationConfigRequest, GetTruncationConfigResponse,
    ListToolsRequest, ListToolsResponse, OutputFieldSpec, OutputFormat, OutputFormatSpec,
    ResetCompletionStateRequest, ResetCompletionStateResponse, ResetToolOptionsRequest,
    ResetToolOptionsResponse, SetSystemRemindersRequest, SetSystemRemindersResponse,
    SetToolOptionsRequest, SetToolOptionsResponse, SetToolOverrideRequest, SetToolOverrideResponse,
    SetTruncationConfigRequest, SetTruncationConfigResponse, StreamDataChunk, StreamDataKind,
    StreamFinalResult, ToolCapabilities, ToolCategory, ToolConfigEntry, ToolError, ToolInfo,
    ToolSource, ToolStreamChunk, ToolSuccess, TruncationConfig, VersionWarning,
};

/// Default client-facing tool name derived from a namespaced tool id.
///
/// Tool ids are colon-separated `Namespace:tool` (e.g. `Kigi:grep`); the
/// default name is the segment after the FIRST colon, so an id with embedded
/// colons (`ns:a:b`) resolves to `a`. Ids without a colon are returned as-is.
///
/// This is the single source of truth shared by the tools server (which
/// advertises tools under this name unless `name_override` is set) and any
/// client that needs to predict the advertised name from a config entry
/// (e.g. prompt tool selection in a downstream service). Keeping both sides on
/// this helper prevents a silent desync that would drop tools from prompts.
pub fn default_client_name(id: &str) -> &str {
    id.split(':').nth(1).unwrap_or(id)
}

impl ToolCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::File => "file",
            Self::Search => "search",
            Self::Shell => "shell",
            Self::Workflow => "workflow",
            Self::External => "external",
            Self::Custom => "custom",
        }
    }
}

#[cfg(test)]
mod default_client_name_tests {
    use super::default_client_name;

    #[test]
    fn pins_first_colon_derivation() {
        assert_eq!(default_client_name("Kigi:grep"), "grep");
        assert_eq!(default_client_name("ns:a:b"), "a");
        assert_eq!(default_client_name("bare"), "bare");
        assert_eq!(default_client_name(""), "");
    }
}
