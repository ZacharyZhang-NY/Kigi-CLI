//! Error types for agent construction.

#[derive(Debug, thiserror::Error)]
pub enum AgentBuildError {
    /// Bad YAML frontmatter, a missing closing `---`, or invalid Markdown
    /// structure in the definition file.
    #[error("failed to parse agent definition: {0}")]
    ParseError(String),

    #[error("missing required field in agent definition: {0}")]
    MissingField(String),

    /// Usually a typo in the definition's `toolNameOverrides`.
    #[error("tool name override references nonexistent tool '{0}'")]
    UnknownToolOverride(String),

    #[error("IO error during agent construction: {0}")]
    IoError(#[from] std::io::Error),

    /// Carries template line numbers and surrounding context.
    #[error("template rendering error: {0}")]
    MiniJinjaError(#[from] minijinja::Error),

    #[error("tool error: {0}")]
    ToolError(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}
