//! `web_search` tool.
//!
//! Calls the Kimi search service (PRD F5; kimi-cli `tools/web/search.py`
//! parity). Reads the pre-constructed `WebSearchClient` from Resources
//! (inserted by `with_backend()` when the config is `Enabled`, i.e. only
//! on Kimi Code OAuth sessions).

use crate::implementations::web_search::client::WebSearchClient;
use crate::types::output::WebSearchOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct WebSearchInput {
    #[schemars(description = "The query text to search for.")]
    pub query: String,
    #[schemars(
        description = "The number of results to return (1-20). Typically you do \
                              not need to set this value. When the results do not contain \
                              what you need, you probably want to give a more concrete \
                              query."
    )]
    pub limit: Option<u8>,
    #[schemars(
        description = "Whether to include the content of the web pages in the \
                              results. It can consume a large amount of tokens when set. \
                              Avoid enabling this together with a large limit."
    )]
    pub include_content: Option<bool>,
}

/// kimi-cli search.py `Params.limit` default / bounds (default=5, ge=1, le=20).
const DEFAULT_LIMIT: u8 = 5;
const MAX_LIMIT: u8 = 20;

#[derive(Debug, Default)]
pub struct WebSearchTool;

impl crate::types::tool_metadata::ToolMetadata for WebSearchTool {
    fn kind(&self) -> ToolKind {
        ToolKind::WebSearch
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Kigi
    }

    fn description_template(&self) -> &str {
        "Search the web for up-to-date information, tailored for coding and software development tasks."
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl kigi_tool_runtime::Tool for WebSearchTool {
    type Args = WebSearchInput;
    type Output = WebSearchOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new("web_search").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(
            "web_search",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.web_search", skip_all)]
    async fn run(
        &self,
        ctx: kigi_tool_runtime::ToolCallContext,
        input: WebSearchInput,
    ) -> Result<WebSearchOutput, kigi_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let client;
        {
            let res = resources.lock().await;
            client = res.require::<WebSearchClient>()?.clone();
        }

        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let (content, citations) = client
            .search(
                &input.query,
                limit,
                input.include_content.unwrap_or(false),
                ctx.call_id.as_str(),
            )
            .await?;

        Ok(WebSearchOutput {
            query: input.query.clone(),
            content,
            citations,
            allowed_domains: None,
            pre_formatted: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    #[test]
    fn tool_name_and_description() {
        let tool = WebSearchTool;
        assert_eq!(kigi_tool_runtime::Tool::id(&tool).as_str(), "web_search");
        assert!(
            crate::types::tool_metadata::ToolMetadata::description_template(&tool)
                .contains("Search the web")
        );
        assert!(
            crate::types::tool_metadata::ToolMetadata::description_template(&tool)
                .contains("coding")
        );
    }

    #[tokio::test]
    async fn errors_when_client_not_in_resources() {
        let resources = Resources::new();
        let tool = WebSearchTool;
        let result = kigi_tool_runtime::Tool::run(
            &tool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            WebSearchInput {
                query: "test".into(),
                limit: None,
                include_content: None,
            },
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("missing required resource"),
            "Expected 'missing required resource' error, got: {err_msg}"
        );
    }
}
