//! OAuth configuration types for MCP servers.
//!
//! Constructed by the host's TOML parsing (`McpServerConfig::oauth_config`)
//! and consumed by [`crate::oauth`].

use std::collections::HashMap;

/// Travels alongside `acp::McpServer` rather than inside it, because that is an
/// external crate type and can't carry extra fields.
#[derive(Debug, Clone, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub callback_port: Option<u16>,
}

impl McpOAuthConfig {
    pub fn is_configured(&self) -> bool {
        self.client_id.is_some()
    }
}

/// Keyed by MCP server name.
pub type McpOAuthConfigMap = HashMap<String, McpOAuthConfig>;
