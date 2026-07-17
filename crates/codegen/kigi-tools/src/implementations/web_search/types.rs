use indexmap::IndexMap;

/// Configuration for the `web_search` tool (PRD F5).
///
/// The Kimi search service exists only on the Kimi Code subscription channel
/// (`POST {coding_base}/search`, kimi-cli `auth/platforms.py`:
/// `search_url=f"{_kimi_code_base_url()}/search"`), so the shell enables this
/// only for OAuth sessions — API-key-only sessions get `Disabled` and the
/// tool is absent.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebSearchConfig {
    #[default]
    Disabled,
    Enabled {
        /// Full POST endpoint, e.g. `https://api.kimi.com/coding/v1/search`.
        search_url: String,
        /// Initial bearer token; a live token from the api-key provider
        /// (OAuth refresh) takes precedence per request.
        api_key: String,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        extra_headers: IndexMap<String, String>,
    },
}

impl WebSearchConfig {
    /// Returns `true` when the config is the `Enabled` variant.
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Return a copy safe for returning to clients: the `api_key` is
    /// replaced with `"***REDACTED***"`.
    pub fn redacted(&self) -> Self {
        match self {
            Self::Disabled => Self::Disabled,
            Self::Enabled {
                search_url,
                extra_headers,
                ..
            } => Self::Enabled {
                search_url: search_url.clone(),
                api_key: "***REDACTED***".to_string(),
                extra_headers: extra_headers.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_is_disabled() {
        let config = WebSearchConfig::default();
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_config_redacted() {
        let mut headers = IndexMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());
        let config = WebSearchConfig::Enabled {
            search_url: "https://api.kimi.com/coding/v1/search".to_string(),
            api_key: "secret-key-12345".to_string(),
            extra_headers: headers,
        };
        match config.redacted() {
            WebSearchConfig::Enabled {
                search_url,
                api_key,
                extra_headers,
            } => {
                assert_eq!(api_key, "***REDACTED***");
                assert_eq!(search_url, "https://api.kimi.com/coding/v1/search");
                assert_eq!(extra_headers.get("X-Custom").unwrap(), "value");
            }
            WebSearchConfig::Disabled => panic!("expected Enabled variant"),
        }
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = WebSearchConfig::Enabled {
            search_url: "https://api.kimi.com/coding/v1/search".to_string(),
            api_key: "key".to_string(),
            extra_headers: IndexMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: WebSearchConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_enabled());
    }
}
