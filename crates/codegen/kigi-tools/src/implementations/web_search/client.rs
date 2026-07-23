//! HTTP client for the Kimi search service (PRD F5).
//!
//! Wire contract ported from kimi-cli `tools/web/search.py` (`SearchWeb`)
//! and verified against the live `api.kimi.com/coding/v1` service:
//!
//! - `POST {search_url}` with JSON `{"text_query", "limit",
//!   "enable_page_crawling", "timeout_seconds": 30}`
//! - headers: `Authorization: Bearer <token>` and
//!   `X-Msh-Tool-Call-Id: <tool call id>` (search.py:82-88)
//! - 200 → `{"search_results": [{site_name, title, url, snippet,
//!   content?, date?, icon?, mime?}]}`
//!
//! The server-side timeout is 30s but page crawling can run longer, so the
//! client allows a generous total timeout (search.py:74 uses 180s).

use super::types::WebSearchConfig;
use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::SharedApiKeyProvider;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};

/// Total request timeout. Mirrors kimi-cli search.py:74 (`total=180`):
/// the service crawls pages when `include_content` is set.
const SEARCH_TIMEOUT_SECS: u64 = 180;
/// `timeout_seconds` request field — the server-side search budget
/// (search.py:93).
const SERVER_TIMEOUT_SECS: u64 = 30;

fn tool_error(msg: impl Into<String>) -> kigi_tool_runtime::ToolError {
    kigi_tool_runtime::ToolError::execution(
        kigi_tool_protocol::ToolId::new("web_search").expect("valid"),
        msg.into(),
    )
}

/// One search hit (kimi-cli search.py `SearchResult`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub site_name: String,
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub date: String,
}

/// Response envelope (kimi-cli search.py `Response`).
#[derive(Debug, serde::Deserialize)]
struct SearchResponse {
    search_results: Vec<SearchResult>,
}

#[derive(Clone)]
pub struct WebSearchClient {
    http: reqwest::Client,
    search_url: String,
    api_key: String,
    api_key_provider: Option<SharedApiKeyProvider>,
    /// Optional 401-attribution hook. Callers can wire this so a 401 from
    /// the search service emits an `auth_401_attribution` event with
    /// `consumer == "WebSearch"`.
    attribution_callback: Option<SharedAttributionCallback>,
}

impl WebSearchClient {
    /// Create a new web search client from `WebSearchConfig::Enabled`.
    ///
    /// Returns `Err` if the config is `Disabled` or if header values are invalid.
    pub fn new(
        config: &WebSearchConfig,
        api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, kigi_tool_runtime::ToolError> {
        let WebSearchConfig::Enabled {
            search_url,
            api_key,
            extra_headers,
        } = config
        else {
            return Err(tool_error(
                "Cannot create WebSearchClient from disabled config",
            ));
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (key, value) in extra_headers {
            let header_name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| tool_error(format!("Invalid header name '{key}': {e}")))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| tool_error(format!("Invalid header value for '{key}': {e}")))?;
            headers.insert(header_name, header_value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(SEARCH_TIMEOUT_SECS))
            .build()
            .map_err(|e| tool_error(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            search_url: search_url.clone(),
            api_key: api_key.clone(),
            api_key_provider,
            attribution_callback: None,
        })
    }

    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }

    /// Live token from the provider (OAuth refresh) when available, else the
    /// config-time key.
    async fn current_bearer(&self) -> String {
        crate::types::api_key_provider::resolve_bearer(self.api_key_provider.as_ref())
            .await
            .unwrap_or_else(|| self.api_key.clone())
    }

    fn record_401_attribution(&self, sent_bearer: &str) {
        crate::attribution::emit_401(
            self.attribution_callback.as_ref(),
            ToolConsumer::WebSearch,
            Some(sent_bearer),
        );
    }

    /// Search the Kimi service. Returns the rendered result text plus the
    /// unique result URLs as citations.
    ///
    /// `tool_call_id` rides along as `X-Msh-Tool-Call-Id` (search.py:85) so
    /// the service can correlate the request with the agent turn.
    pub async fn search(
        &self,
        query: &str,
        limit: u8,
        include_content: bool,
        tool_call_id: &str,
    ) -> Result<(String, Vec<String>), kigi_tool_runtime::ToolError> {
        let bearer = self.current_bearer().await;
        let response = self
            .http
            .post(&self.search_url)
            .header(AUTHORIZATION, format!("Bearer {bearer}"))
            .header("X-Msh-Tool-Call-Id", tool_call_id)
            .json(&serde_json::json!({
                "text_query": query,
                "limit": limit,
                "enable_page_crawling": include_content,
                "timeout_seconds": SERVER_TIMEOUT_SECS,
            }))
            .send()
            .await
            .map_err(|e| {
                tool_error(format!(
                    "Search request failed: {e}. The search service may be unavailable."
                ))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.record_401_attribution(&bearer);
            return Err(kigi_tool_runtime::ToolError::unauthorized(
                "Search service returned 401 Unauthorized".to_string(),
            ));
        }
        if !status.is_success() {
            return Err(tool_error(format!(
                "Failed to search. Status: {status}. This may indicate that the \
                 search service is currently unavailable."
            )));
        }
        let results = response
            .json::<SearchResponse>()
            .await
            .map_err(|e| tool_error(format!("Failed to parse search results: {e}")))?
            .search_results;
        Ok(render_results(&results))
    }
}

/// Render hits in kimi-cli's result schema (search.py:141-149):
/// `Title/Date/URL/Summary` per hit, page content when crawled, hits
/// separated by `---`. Citations are the unique result URLs in order.
fn render_results(results: &[SearchResult]) -> (String, Vec<String>) {
    let mut content = String::new();
    let mut citations: Vec<String> = Vec::new();
    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            content.push_str("---\n\n");
        }
        content.push_str(&format!(
            "Title: {}\nDate: {}\nURL: {}\nSummary: {}\n\n",
            result.title, result.date, result.url, result.snippet
        ));
        if !result.content.is_empty() {
            content.push_str(&format!("{}\n\n", result.content));
        }
        if !citations.contains(&result.url) {
            citations.push(result.url.clone());
        }
    }
    (content, citations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn enabled_config(url: &str) -> WebSearchConfig {
        WebSearchConfig::Enabled {
            search_url: url.to_string(),
            api_key: "test-key".to_string(),
            extra_headers: IndexMap::new(),
        }
    }

    #[test]
    fn new_rejects_disabled_config() {
        assert!(WebSearchClient::new(&WebSearchConfig::Disabled, None).is_err());
    }

    #[test]
    fn new_rejects_invalid_extra_header() {
        let mut headers = IndexMap::new();
        headers.insert("bad header name".to_string(), "v".to_string());
        let config = WebSearchConfig::Enabled {
            search_url: "https://api.kimi.com/coding/v1/search".to_string(),
            api_key: "k".to_string(),
            extra_headers: headers,
        };
        assert!(WebSearchClient::new(&config, None).is_err());
    }

    #[test]
    fn render_results_follows_kimi_cli_schema() {
        let results = vec![
            SearchResult {
                site_name: "Rust Blog".into(),
                title: "Announcing Rust".into(),
                url: "https://blog.rust-lang.org/a".into(),
                snippet: "The release".into(),
                content: String::new(),
                date: "2026-01-01".into(),
            },
            SearchResult {
                site_name: "Docs".into(),
                title: "The Book".into(),
                url: "https://doc.rust-lang.org/book".into(),
                snippet: "Learn Rust".into(),
                content: "Full crawled page text".into(),
                date: String::new(),
            },
        ];
        let (content, citations) = render_results(&results);
        assert_eq!(
            content,
            "Title: Announcing Rust\nDate: 2026-01-01\nURL: https://blog.rust-lang.org/a\n\
             Summary: The release\n\n---\n\nTitle: The Book\nDate: \n\
             URL: https://doc.rust-lang.org/book\nSummary: Learn Rust\n\n\
             Full crawled page text\n\n"
        );
        assert_eq!(
            citations,
            [
                "https://blog.rust-lang.org/a",
                "https://doc.rust-lang.org/book"
            ]
        );
    }

    #[test]
    fn render_results_deduplicates_citations() {
        let hit = SearchResult {
            site_name: String::new(),
            title: "T".into(),
            url: "https://same.example".into(),
            snippet: "S".into(),
            content: String::new(),
            date: String::new(),
        };
        let (_, citations) = render_results(&[hit.clone(), hit]);
        assert_eq!(citations, ["https://same.example"]);
    }

    #[tokio::test]
    async fn search_sends_kimi_wire_contract_and_parses_results() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("x-msh-tool-call-id", "call-42"))
            .and(body_json(serde_json::json!({
                "text_query": "rust ownership",
                "limit": 5,
                "enable_page_crawling": false,
                "timeout_seconds": 30,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "search_results": [{
                    "site_name": "Docs",
                    "title": "Ownership",
                    "url": "https://doc.rust-lang.org/ownership",
                    "snippet": "What is ownership?",
                    "content": "",
                    "date": "2026-05-01",
                    "icon": "",
                    "mime": ""
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            WebSearchClient::new(&enabled_config(&format!("{}/search", server.uri())), None)
                .unwrap();
        let (content, citations) = client
            .search("rust ownership", 5, false, "call-42")
            .await
            .unwrap();
        assert!(content.contains("Title: Ownership"));
        assert!(content.contains("URL: https://doc.rust-lang.org/ownership"));
        assert_eq!(citations, ["https://doc.rust-lang.org/ownership"]);
    }

    #[tokio::test]
    async fn search_maps_401_to_unauthorized() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client =
            WebSearchClient::new(&enabled_config(&format!("{}/search", server.uri())), None)
                .unwrap();
        let err = client.search("q", 5, false, "c").await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[tokio::test]
    async fn search_surfaces_server_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client =
            WebSearchClient::new(&enabled_config(&format!("{}/search", server.uri())), None)
                .unwrap();
        let err = client.search("q", 5, false, "c").await.unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
    }
}
