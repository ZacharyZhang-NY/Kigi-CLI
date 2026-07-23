//! Kimi Code auth configuration.
//!
//! The wire endpoints come from [`kigi_env`] (`oauth_host()`, overridable via
//! `KIGI_OAUTH_HOST`) and the client id is fixed
//! ([`crate::auth::kimi_oauth::KIMI_CODE_CLIENT_ID`]), so this config carries
//! no per-deployment OAuth knobs. The struct is kept (deserialized from the
//! agent config TOML) as the extension point for future auth options.

use serde::{Deserialize, Serialize};

/// Persisted-credential scope key for the Kimi Code OAuth session — both the
/// auth.json map key and the system-keyring entry name (service `kigi`).
pub const KIMI_CODE_OAUTH_SCOPE: &str = "oauth/kimi-code";

/// Auth configuration block (`[kimi_code_config]` in the agent config).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KimiCodeConfig {}

impl KimiCodeConfig {
    pub fn auth_scope(&self) -> String {
        KIMI_CODE_OAUTH_SCOPE.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_scope_is_the_kimi_code_key() {
        assert_eq!(KimiCodeConfig::default().auth_scope(), "oauth/kimi-code");
    }

    #[test]
    fn deserializes_from_empty_toml() {
        let cfg: KimiCodeConfig = toml::from_str("").expect("empty config parses");
        assert_eq!(cfg.auth_scope(), KIMI_CODE_OAUTH_SCOPE);
    }
}
