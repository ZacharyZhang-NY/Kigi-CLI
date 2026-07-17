use serde::{Deserialize, Serialize};

/// Access-gate copy resolved from remote settings (message + optional CTA).
/// Auth no longer produces gates (tier gating was an xAI concept); the pager
/// still renders one when remote settings carry a gate message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateInfo {
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Typed auth metadata passed from the shell to the pager via ACP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthMeta {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    #[serde(default)]
    pub show_resolved_model: Option<bool>,
}
