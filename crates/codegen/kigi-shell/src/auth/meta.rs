use serde::{Deserialize, Serialize};

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
