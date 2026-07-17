//! Permission-policy config value types, extracted from kigi-shell
//! (config dependency inversion).

use serde::{Deserialize, Serialize};

/// Permission policy configuration loaded from `[permission]` section in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionConfig {
    pub rules: Vec<PermissionRule>,
}

/// A single permission rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: RuleAction,
    #[serde(default)]
    pub tool: ToolFilter,
    pub pattern: Option<String>,
    #[serde(default)]
    pub pattern_mode: PatternMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PatternMode {
    #[default]
    Glob,
    /// Match against URL host rather than full string (from `WebFetch(domain:...)`).
    Domain,
}

/// Action to take when rule matches.
///
/// CWE-1188: Default changed from Allow to Deny so that omitting the
/// `action` field in a TOML permission rule does not silently create a
/// catch-all allow rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    #[default]
    Deny,
    Ask,
}

/// Tool filter for permission rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolFilter {
    #[default]
    Any,
    Bash,
    Edit,
    Read,
    Grep,
    Mcp,
    WebFetch,
}

/// How the agent handles tool execution permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Prompt the user for each tool call (default).
    #[default]
    Ask,
    /// Approve everything without prompting.
    AlwaysApprove,
    /// LLM transcript classifier reviews non-fast-path tool calls.
    Auto,
}

impl PermissionMode {
    pub fn is_always_approve(self) -> bool {
        matches!(self, Self::AlwaysApprove)
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn from_yolo(yolo: bool) -> Self {
        if yolo { Self::AlwaysApprove } else { Self::Ask }
    }
}
