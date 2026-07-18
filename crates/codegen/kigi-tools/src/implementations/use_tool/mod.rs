//! `use_tool` — dispatch to a discovered MCP tool.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::util::mcp_truncate::{McpTruncateContext, truncate_tool_output};

/// Input for the `use_tool` meta-dispatch tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct UseToolInput {
    /// The qualified name of the integration tool to call (e.g., "linear__save_issue").
    /// Must be a tool previously discovered via `search_tool`.
    pub tool_name: String,
    /// The arguments to pass to the tool, as a JSON object.
    /// Use the parameter schema returned by `search_tool` to construct this.
    #[schemars(schema_with = "object_value_schema")]
    pub tool_input: serde_json::Value,
}

fn object_value_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": true,
    })
}

/// Configuration for [`UseTool`].
///
/// Controls whether the native-tool corrective error is active.
/// When `native_tool_correction` is `true` (default), `use_tool` detects
/// native tool names via [`EnabledNativeToolNames`] and returns a targeted
/// corrective error ("call it directly"). When `false`, the old generic
/// "not a valid MCP tool name" warning fires for all unqualified names,
/// regardless of whether the name is a native tool.
///
/// Use `false` if you want the pre-fix behavior (e.g., offline evaluation
/// where the corrective error would alter the model's trajectory).
///
/// [`EnabledNativeToolNames`]: crate::types::resources::EnabledNativeToolNames
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UseToolParams {
    /// Enable the native-tool corrective error. Default: `true`.
    #[serde(default = "default_true")]
    pub native_tool_correction: bool,
}

fn default_true() -> bool {
    true
}

impl Default for UseToolParams {
    fn default() -> Self {
        Self {
            native_tool_correction: true,
        }
    }
}

crate::register_resource!("kigi", "UseTool", UseToolParams);

/// Meta tool that dispatches calls to MCP tools discovered via `search_tool`.
///
/// `run()` reads [`InnerDispatch`] from `ToolCallContext::extensions` — set
/// by `FinalizedToolset::call()` on every call — and dispatches to the target
/// tool via the runtime `ToolDispatch` trait → `FinalizedToolset::call_raw()`.
/// This bypasses the outer `ToolBridge` mutex and avoids deadlock.
/// `call_raw()` skips reminders/persistence so post-processing
/// runs exactly once (via the outer `call("use_tool")`).
///
/// If `InnerDispatch` is absent, dispatch fails with a clear error (should
/// never happen in production — `FinalizedToolset::call()` always sets it).
///
/// The tool exists so its definition appears in the model's tool list —
/// keeping the tool set stable across turns (no KV cache breaks when new
/// MCP tools are discovered).
///
/// [`InnerDispatch`]: crate::types::resources::InnerDispatch
#[derive(Debug, Default)]
pub struct UseTool;

async fn dispatch_local_mcp(
    dispatch: std::sync::Arc<crate::types::resources::InnerDispatch>,
    tool_name: &str,
    tool_input: serde_json::Value,
    ctx: kigi_tool_runtime::ToolCallContext,
) -> Result<ToolOutput, kigi_tool_runtime::ToolError> {
    let tool_id = kigi_tool_protocol::ToolId::new(tool_name).map_err(|_| {
        kigi_tool_runtime::ToolError::invalid_arguments(format!("invalid tool name: '{tool_name}'"))
    })?;
    let typed = dispatch.0.call_terminal(tool_id, tool_input, ctx).await?;
    serde_json::from_value(typed.value)
        .map_err(|e| kigi_tool_runtime::ToolError::custom("output_decoding", e.to_string()))
}

fn normalize_mcp_arguments(input: serde_json::Value) -> serde_json::Value {
    match input {
        serde_json::Value::String(s) => match serde_json::from_str(&s) {
            Ok(v @ serde_json::Value::Object(_)) => v,
            _ => serde_json::Value::String(s),
        },
        serde_json::Value::Null => serde_json::json!({}),
        other => other,
    }
}

pub async fn dispatch_mcp_tool(
    ctx: &kigi_tool_runtime::ToolCallContext,
    tool_name: &str,
    tool_input: serde_json::Value,
    caller: &str,
) -> Result<ToolOutput, kigi_tool_runtime::ToolError> {
    let tool_input = normalize_mcp_arguments(tool_input);
    let Some(dispatch) = ctx
        .extensions
        .get::<crate::types::resources::InnerDispatch>()
    else {
        return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
            "{caller} called outside of tool execution context. inner_dispatch not set -- this is a bug."
        )));
    };

    dispatch_local_mcp(dispatch, tool_name, tool_input, ctx.clone()).await
}

impl crate::types::tool_metadata::ToolMetadata for UseTool {
    fn kind(&self) -> ToolKind {
        ToolKind::UseTool
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Kigi
    }

    fn description_template(&self) -> &str {
        "Call an MCP integration tool.\n\n\
         The `tool_name` must be the qualified `server__tool` name (e.g., `linear__save_issue`). \
         The `tool_input` must conform exactly to the input schema returned by `${{ tools.by_kind.search_tool }}`."
    }
}

impl kigi_tool_runtime::Tool for UseTool {
    type Args = UseToolInput;
    type Output = ToolOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new("use_tool").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(
            "use_tool",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: kigi_tool_runtime::ToolCallContext,
        input: UseToolInput,
    ) -> Result<ToolOutput, kigi_tool_runtime::ToolError> {
        use crate::types::resources::{EnabledNativeToolNames, Params};

        let resources = crate::types::tool_metadata::shared_resources(&ctx).ok();
        let (is_native, search_tool_name) = if let Some(resources) = resources.as_ref() {
            let guard = resources.lock().await;
            let correction_enabled = guard
                .get::<Params<UseToolParams>>()
                .is_none_or(|p| p.0.native_tool_correction);
            let native = correction_enabled
                && guard
                    .get::<EnabledNativeToolNames>()
                    .is_some_and(|set| set.contains(&input.tool_name));
            let st = guard
                .get::<crate::types::template_renderer::TemplateRenderer>()
                .and_then(|r| r.tool_for_kind(ToolKind::SearchTool))
                .map(str::to_string)
                .unwrap_or_else(|| "search_tool".to_string());
            (native, st)
        } else {
            (false, "search_tool".to_string())
        };

        if !input.tool_name.contains("__") {
            return Err(if is_native {
                // Native tool wrongly routed through use_tool. Tell the model
                // to call it directly. Strategy chosen via offline eval over
                // real production failures:
                // 2% doom-loop, 86% native recovery, 0 double-schedules.
                tracing::info!(
                    tool_name = %input.tool_name,
                    "use_tool: native tool detected, returning corrective error"
                );
                kigi_tool_runtime::ToolError::invalid_arguments(format!(
                    "`{tool}` is a native tool, not an MCP integration tool. \
                     Call `{tool}` directly as its own tool call instead of \
                     routing it through `use_tool`.",
                    tool = input.tool_name
                ))
            } else {
                // Unknown name (e.g. a built-in skill like `jira`). Keep the
                // existing search_tool steer (empirically reduces retry loops
                // on unqualified tool names).
                kigi_tool_runtime::ToolError::invalid_arguments(format!(
                    "'{}' is not a valid MCP tool name. \
                     Tool names must be qualified as `server__tool` (e.g., `linear__save_issue`). \
                     Use `{}` to discover available tools.",
                    input.tool_name, search_tool_name
                ))
            });
        }

        let output =
            dispatch_mcp_tool(&ctx, &input.tool_name, input.tool_input, "use_tool").await?;

        let trunc_ctx = McpTruncateContext::from_tool_ctx(&ctx, "use_tool").await;
        Ok(truncate_tool_output(output, &trunc_ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::InnerDispatch;
    use crate::util::mcp_truncate::{LONG_LINE_BYTES, McpDumpKind};
    use crate::util::query_tools::QueryTools;
    use std::sync::Arc;

    /// Mock dispatch returning a fixed output.
    struct MockToolDispatch {
        expected_tool_name: String,
        return_output: ToolOutput,
    }

    #[async_trait::async_trait]
    impl kigi_tool_runtime::ToolDispatch for MockToolDispatch {
        async fn call(
            &self,
            tool_id: kigi_tool_protocol::ToolId,
            _args: serde_json::Value,
            _ctx: kigi_tool_runtime::ToolCallContext,
        ) -> kigi_tool_runtime::ToolStream<kigi_tool_runtime::TypedToolOutput> {
            assert_eq!(tool_id.as_str(), self.expected_tool_name);
            let value = serde_json::to_value(self.return_output.clone()).unwrap();
            kigi_tool_runtime::terminal_only(Ok(kigi_tool_runtime::TypedToolOutput::from_value(
                tool_id, value,
            )))
        }
    }

    type SharedArgs = Arc<std::sync::Mutex<Option<serde_json::Value>>>;

    /// Mock dispatch that captures the dispatched args for assertion.
    struct CapturingDispatch {
        captured_args: SharedArgs,
    }

    #[async_trait::async_trait]
    impl kigi_tool_runtime::ToolDispatch for CapturingDispatch {
        async fn call(
            &self,
            tool_id: kigi_tool_protocol::ToolId,
            args: serde_json::Value,
            _ctx: kigi_tool_runtime::ToolCallContext,
        ) -> kigi_tool_runtime::ToolStream<kigi_tool_runtime::TypedToolOutput> {
            if !matches!(
                tool_id.as_str(),
                "server__tool" | "linear__save_issue" | "linear__list_issues"
            ) {
                return kigi_tool_runtime::terminal_only(Err(
                    kigi_tool_runtime::ToolError::not_found(tool_id, "Tool not found"),
                ));
            }
            *self.captured_args.lock().unwrap() = Some(args);
            let value = serde_json::to_value(ToolOutput::Text("ok".into())).unwrap();
            kigi_tool_runtime::terminal_only(Ok(kigi_tool_runtime::TypedToolOutput::from_value(
                tool_id, value,
            )))
        }
    }

    fn ctx_capturing() -> (kigi_tool_runtime::ToolCallContext, SharedArgs) {
        let args: SharedArgs = Arc::new(std::sync::Mutex::new(None));
        let ctx = ctx_with_dispatch(CapturingDispatch {
            captured_args: Arc::clone(&args),
        });
        (ctx, args)
    }

    /// Mock dispatch that always returns an error.
    struct ErrorToolDispatch {
        error: String,
    }

    #[async_trait::async_trait]
    impl kigi_tool_runtime::ToolDispatch for ErrorToolDispatch {
        async fn call(
            &self,
            _tool_id: kigi_tool_protocol::ToolId,
            _args: serde_json::Value,
            _ctx: kigi_tool_runtime::ToolCallContext,
        ) -> kigi_tool_runtime::ToolStream<kigi_tool_runtime::TypedToolOutput> {
            let tid = kigi_tool_protocol::ToolId::new(&self.error)
                .unwrap_or_else(|_| kigi_tool_protocol::ToolId::new("unknown").expect("valid"));
            kigi_tool_runtime::terminal_only(Err(kigi_tool_runtime::ToolError::not_found(
                tid,
                format!("Tool not found: {}", self.error),
            )))
        }
    }

    fn new_ctx() -> kigi_tool_runtime::ToolCallContext {
        let call_id = kigi_tool_protocol::ToolCallId::new_v7();
        kigi_tool_runtime::ToolCallContext::new(call_id)
    }

    fn ctx_with_dispatch(
        dispatch: impl kigi_tool_runtime::ToolDispatch + 'static,
    ) -> kigi_tool_runtime::ToolCallContext {
        let mut ctx = new_ctx();
        ctx.extensions.insert(InnerDispatch(Arc::new(dispatch)));
        ctx
    }

    #[tokio::test]
    async fn rejects_builtin_tool_names() {
        let tool = UseTool;
        let ctx = new_ctx();

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "read_file".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.kind, kigi_tool_runtime::ToolErrorKind::InvalidArguments);
        assert!(err.detail.contains("not a valid MCP tool name"));
        assert!(err.detail.contains("read_file"));
    }

    #[tokio::test]
    async fn errors_when_inner_dispatch_not_set() {
        let tool = UseTool;
        let ctx = new_ctx();

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "linear__save_issue".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.kind, kigi_tool_runtime::ToolErrorKind::InvalidArguments);
        assert!(err.detail.contains("inner_dispatch not set"));
    }

    #[tokio::test]
    async fn dispatches_via_ctx_inner_dispatch() {
        let tool = UseTool;
        let ctx = ctx_with_dispatch(MockToolDispatch {
            expected_tool_name: "linear__save_issue".into(),
            return_output: ToolOutput::Text("issue created".into()),
        });

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "linear__save_issue".into(),
                tool_input: serde_json::json!({"title": "test issue"}),
            },
        )
        .await;

        assert!(result.is_ok());
        if let Ok(ToolOutput::Text(msg)) = result {
            assert_eq!(msg.text, "issue created");
        } else {
            panic!("Expected ToolOutput::Text");
        }
    }

    #[tokio::test]
    async fn propagates_inner_dispatch_error() {
        let tool = UseTool;
        let ctx = ctx_with_dispatch(ErrorToolDispatch {
            error: "bad__tool".into(),
        });

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "bad__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert!(err.detail.contains("bad__tool"));
    }

    #[tokio::test]
    async fn normalizes_string_encoded_tool_input() {
        let tool = UseTool;
        let (ctx, captured_args) = ctx_capturing();

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "linear__list_issues".into(),
                tool_input: serde_json::Value::String(r#"{"assignee": "me", "limit": 10}"#.into()),
            },
        )
        .await;

        assert!(result.is_ok());
        let captured = captured_args.lock().unwrap().clone().unwrap();
        assert!(
            captured.is_object(),
            "string-encoded input should be parsed to object"
        );
        assert_eq!(captured["assignee"], "me");
        assert_eq!(captured["limit"], 10);
    }

    #[tokio::test]
    async fn passes_object_tool_input_unchanged() {
        let tool = UseTool;
        let (ctx, captured_args) = ctx_capturing();

        let expected = serde_json::json!({"title": "test", "team": "ENG"});
        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "linear__save_issue".into(),
                tool_input: expected.clone(),
            },
        )
        .await;

        assert!(result.is_ok());
        let captured = captured_args.lock().unwrap().clone().unwrap();
        assert_eq!(captured, expected);
    }

    #[tokio::test]
    async fn non_json_string_passes_through() {
        let tool = UseTool;
        let (ctx, captured_args) = ctx_capturing();

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::Value::String("not json".into()),
            },
        )
        .await;

        assert!(result.is_ok());
        let captured = captured_args.lock().unwrap().clone().unwrap();
        assert_eq!(captured, serde_json::Value::String("not json".into()));
    }

    #[tokio::test]
    async fn local_server_tool_still_uses_local_dispatch_path() {
        let tool = UseTool;
        let (ctx, captured_args) = ctx_capturing();

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({"local": true}),
            },
        )
        .await;

        assert!(result.is_ok());
        let captured = captured_args.lock().unwrap().clone().unwrap();
        assert_eq!(captured, serde_json::json!({"local": true}));
    }

    fn ctx_with_dispatch_and_resources(
        dispatch: impl kigi_tool_runtime::ToolDispatch + 'static,
        resources: crate::types::resources::SharedResources,
    ) -> kigi_tool_runtime::ToolCallContext {
        let mut ctx = new_ctx();
        ctx.extensions.insert(InnerDispatch(Arc::new(dispatch)));
        ctx.extensions.insert(resources);
        ctx
    }

    #[tokio::test]
    async fn truncates_large_mcp_output() {
        use crate::types::context::TruncationConfig;
        use crate::types::output::{MCPOutput, MCPOutputDetails};
        use crate::types::resources::{Resources, TruncationCfg};
        use crate::util::truncate::format_bytes;

        // Explicit TruncationCfg — avoid process-global mcp_max_output_bytes().
        let limit = 20_000;
        let big = "x".repeat(limit + 1000);
        let mut resources = Resources::new();
        let mut cfg = TruncationConfig::default();
        cfg.per_tool_max_output_bytes
            .insert("use_tool".to_string(), limit);
        resources.insert(TruncationCfg(cfg));

        let tool = UseTool;
        let ctx = ctx_with_dispatch_and_resources(
            MockToolDispatch {
                expected_tool_name: "server__tool".into(),
                return_output: ToolOutput::MCP(MCPOutput::okay_output(
                    "server__tool".into(),
                    "server".into(),
                    big,
                )),
            },
            resources.into_shared(),
        );

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        if let ToolOutput::MCP(mcp) = &result {
            if let MCPOutputDetails::OkayOutput(text) = mcp.output() {
                assert!(
                    text.contains("[MCP output truncated:"),
                    "truncated output must contain truncation annotation, got: {}",
                    &text[text.len().saturating_sub(200)..],
                );
                let expected = format!("showing first {}", format_bytes(limit));
                assert!(
                    text.contains(&expected),
                    "annotation must show the truncation limit ({expected})"
                );
            } else {
                panic!("expected OkayOutput");
            }
        } else {
            panic!("expected ToolOutput::MCP");
        }
    }

    #[tokio::test]
    async fn truncation_cfg_override() {
        use crate::types::context::TruncationConfig;
        use crate::types::output::{MCPOutput, MCPOutputDetails};
        use crate::types::resources::{Resources, TruncationCfg};

        let custom_limit = 5_000;
        let big = "z".repeat(custom_limit + 500);

        let mut resources = Resources::new();
        let mut cfg = TruncationConfig::default();
        cfg.per_tool_max_output_bytes
            .insert("use_tool".to_string(), custom_limit);
        resources.insert(TruncationCfg(cfg));

        let tool = UseTool;
        let ctx = ctx_with_dispatch_and_resources(
            MockToolDispatch {
                expected_tool_name: "server__tool".into(),
                return_output: ToolOutput::MCP(MCPOutput::okay_output(
                    "server__tool".into(),
                    "server".into(),
                    big,
                )),
            },
            resources.into_shared(),
        );

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        if let ToolOutput::MCP(mcp) = &result {
            if let MCPOutputDetails::OkayOutput(text) = mcp.output() {
                assert!(
                    text.contains("[MCP output truncated:"),
                    "truncated output must contain truncation annotation"
                );
                assert!(
                    text.contains("showing first 5.0KB"),
                    "annotation must reflect the custom limit"
                );
            } else {
                panic!("expected OkayOutput");
            }
        } else {
            panic!("expected ToolOutput::MCP");
        }
    }

    #[tokio::test]
    async fn truncates_large_mcp_error_output() {
        use crate::types::context::TruncationConfig;
        use crate::types::output::{MCPOutput, MCPOutputDetails};
        use crate::types::resources::{Resources, TruncationCfg};

        let limit = 20_000;
        let big_error = "e".repeat(limit + 500);
        let mut resources = Resources::new();
        let mut cfg = TruncationConfig::default();
        cfg.per_tool_max_output_bytes
            .insert("use_tool".to_string(), limit);
        resources.insert(TruncationCfg(cfg));

        let tool = UseTool;
        let ctx = ctx_with_dispatch_and_resources(
            MockToolDispatch {
                expected_tool_name: "server__tool".into(),
                return_output: ToolOutput::MCP(MCPOutput::errored(
                    "server__tool".into(),
                    "server".into(),
                    big_error,
                )),
            },
            resources.into_shared(),
        );

        let result = kigi_tool_runtime::Tool::run(
            &tool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        if let ToolOutput::MCP(mcp) = &result {
            if let MCPOutputDetails::Error(text) = mcp.output() {
                assert!(
                    text.contains("[MCP output truncated:"),
                    "large MCP error output must be truncated"
                );
            } else {
                panic!("expected MCPOutputDetails::Error");
            }
        } else {
            panic!("expected ToolOutput::MCP");
        }
    }

    #[test]
    fn schema_allows_arbitrary_properties_for_tool_input() {
        let schema = schemars::schema_for!(UseToolInput);
        let schema_json = serde_json::to_value(&schema).unwrap();
        let tool_input_schema = &schema_json["properties"]["tool_input"];
        assert_eq!(
            tool_input_schema["type"], "object",
            "tool_input schema should have type: object, got: {tool_input_schema}"
        );
        assert_eq!(
            tool_input_schema["additionalProperties"], true,
            "tool_input schema must allow arbitrary keys for MCP inputs, got: {tool_input_schema}"
        );
    }

    // ── MCP dump classification (.json extension + jq/python steer) ──

    #[test]
    fn classify_long_line_json_warns_against_grep() {
        let payload = format!(r#"{{"data":"{}"}}"#, "x".repeat(3_000));
        let kind = McpDumpKind::classify(&payload);
        assert_eq!(kind, McpDumpKind::LongLineJson);
        assert_eq!(kind.extension(), "json");
        let steer = kind.steer("run_terminal_command", all_tools());
        assert!(
            steer.contains("run_terminal_command"),
            "steer must use the harness shell tool: {steer}"
        );
        assert!(steer.contains("jq"), "names jq as an option: {steer}");
        assert!(
            steer.contains("grep"),
            "long-line JSON steer must warn against grep: {steer}"
        );
        assert!(
            !steer.contains("if available"),
            "presence is detected, so no 'if available' hedge: {steer}"
        );
    }

    #[test]
    fn classify_long_line_json_array() {
        let payload = format!("[{}]", vec!["\"x\""; 1_000].join(","));
        assert!(payload.len() > LONG_LINE_BYTES);
        assert_eq!(McpDumpKind::classify(&payload), McpDumpKind::LongLineJson);
    }

    #[test]
    fn classify_pretty_json_suggests_jq_without_grep_warning() {
        let payload = "{\n  \"name\": \"node\",\n  \"panels\": [\n    {\"title\": \"CPU\"}\n  ]\n}";
        let kind = McpDumpKind::classify(payload);
        assert_eq!(kind, McpDumpKind::Json);
        assert_eq!(kind.extension(), "json");
        let steer = kind.steer("bash", all_tools());
        assert!(steer.contains("jq"), "should suggest jq: {steer}");
        assert!(
            !steer.contains("grep"),
            "pretty JSON steer should not warn against grep: {steer}"
        );
    }

    #[test]
    fn classify_ignores_surrounding_whitespace() {
        assert_eq!(
            McpDumpKind::classify("   \n  {\"a\": 1}  \n"),
            McpDumpKind::Json
        );
    }

    #[test]
    fn classify_non_json_is_plain_text_with_no_steer() {
        let kind = McpDumpKind::classify("just some log output\nline two\nline three");
        assert_eq!(kind, McpDumpKind::Other);
        assert_eq!(kind.extension(), "txt");
        assert_eq!(kind.steer("bash", all_tools()), "");
    }

    #[test]
    fn classify_invalid_json_is_other() {
        // Starts with `{`/`[` but does not parse.
        assert_eq!(McpDumpKind::classify("{not valid json"), McpDumpKind::Other);
        assert_eq!(
            McpDumpKind::classify("[1, 2, 3"),
            McpDumpKind::Other,
            "unterminated array is not JSON"
        );
    }

    #[test]
    fn classify_bare_scalars_are_other() {
        // Valid JSON scalars, but the `{`/`[` gate excludes them → Other.
        for s in ["12345", "true", "null", "\"a string\"", ""] {
            assert_eq!(
                McpDumpKind::classify(s),
                McpDumpKind::Other,
                "{s:?} should classify as Other"
            );
        }
    }

    #[test]
    fn classify_python_repr_single_line_is_long_line_text() {
        // mcp-server-sqlite returns a Python repr of rows (single quotes) on one
        // long line — JSON-ish but invalid JSON; the long-line case still catches it.
        let row = "{'id': 0, 'name': 'user0', 'email': 'u0@example.com', 'age': 20}";
        let payload = format!("[{}]", vec![row; 60].join(", "));
        assert!(payload.len() > LONG_LINE_BYTES && !payload.contains('\n'));
        let kind = McpDumpKind::classify(&payload);
        assert_eq!(kind, McpDumpKind::LongLineText);
        assert_eq!(kind.extension(), "txt");
        let steer = kind.steer("bash", all_tools());
        assert!(steer.contains("python"), "should steer to python: {steer}");
        assert!(
            steer.contains("grep"),
            "single-long-line steer must warn against grep: {steer}"
        );
        assert!(
            !steer.contains("valid JSON"),
            "must not claim invalid JSON is JSON: {steer}"
        );
    }

    #[test]
    fn classify_minified_blob_single_line_is_long_line_text() {
        let payload = "QUJD".repeat(800); // base64-like blob, one long non-JSON line
        assert_eq!(McpDumpKind::classify(&payload), McpDumpKind::LongLineText);
    }

    #[test]
    fn classify_csv_stays_plain_text() {
        // CSV is line-addressable (grep/awk work) → Other, no steer.
        let mut csv = String::from("id,name,email,age\n");
        for i in 0..50 {
            csv.push_str(&format!("{i},user{i},u{i}@example.com,{}\n", 20 + i));
        }
        let kind = McpDumpKind::classify(&csv);
        assert_eq!(kind, McpDumpKind::Other);
        assert_eq!(kind.extension(), "txt");
        assert_eq!(kind.steer("bash", all_tools()), "");
    }

    // ── presence-aware steer (names only installed query tools) ──

    /// Every query tool present — for classification tests that only care
    /// about the dump kind, not which tools the host happens to have.
    fn all_tools() -> QueryTools {
        QueryTools {
            jq: Some("jq"),
            python: Some("python3"),
            sed: Some("sed"),
            cut: Some("cut"),
        }
    }

    #[test]
    fn steer_names_only_installed_tools() {
        // jq absent, python present → the JSON steer names python, not jq.
        let tools = QueryTools {
            jq: None,
            python: Some("python3"),
            sed: None,
            cut: None,
        };
        let steer = McpDumpKind::LongLineJson.steer("bash", tools);
        assert!(
            steer.contains("python3"),
            "names the present python: {steer}"
        );
        assert!(!steer.contains("jq"), "must not name absent jq: {steer}");
        assert!(
            !steer.contains("if available"),
            "no hedge once presence is known: {steer}"
        );
    }

    #[test]
    fn steer_omits_examples_when_no_query_tools_present() {
        // Neither jq nor python → no "(e.g. …)" clause, but still steer to the
        // shell tool (and keep the grep warning for the long line).
        let none = QueryTools::default();
        let steer = McpDumpKind::LongLineJson.steer("bash", none);
        assert!(
            steer.contains("`bash`"),
            "still names the shell tool: {steer}"
        );
        assert!(
            !steer.contains("(e.g."),
            "no examples when none present: {steer}"
        );
        assert!(
            !steer.contains("jq") && !steer.contains("python"),
            "{steer}"
        );
        assert!(
            steer.contains("grep"),
            "keeps the long-line warning: {steer}"
        );
    }

    // Tool-set membership/ordering invariants live with the mechanism in
    // `util::query_tools::tests`; here we only test the steer's own behavior
    // (which kind steers, presence-gating, no-hedge).

    // ── run() wiring: extension, file, and the gated steer ──

    #[tokio::test]
    async fn json_dump_written_as_json_with_query_steer() {
        use crate::types::context::TruncationConfig;
        use crate::types::output::{MCPOutput, MCPOutputDetails};
        use crate::types::resources::{Resources, SessionFolder, TruncationCfg};

        let tmp = tempfile::tempdir().unwrap();
        let limit = 20_000;
        let big_json = format!(r#"{{"data":"{}"}}"#, "x".repeat(limit + 1000));
        let mut resources = Resources::new();
        resources.insert(SessionFolder(tmp.path().to_path_buf()));
        let mut cfg = TruncationConfig::default();
        cfg.per_tool_max_output_bytes
            .insert("use_tool".to_string(), limit);
        resources.insert(TruncationCfg(cfg));

        let ctx = ctx_with_dispatch_and_resources(
            MockToolDispatch {
                expected_tool_name: "server__tool".into(),
                return_output: ToolOutput::MCP(MCPOutput::okay_output(
                    "server__tool".into(),
                    "server".into(),
                    big_json,
                )),
            },
            resources.into_shared(),
        );

        let result = kigi_tool_runtime::Tool::run(
            &UseTool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        // dump saved as .json
        let files: Vec<_> = std::fs::read_dir(tmp.path().join("mcp"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "exactly one dump file");
        assert_eq!(
            files[0].extension().and_then(|e| e.to_str()),
            Some("json"),
            "JSON payload must be saved as .json, got {:?}",
            files[0]
        );

        // annotation: .json path + steer to query the file via the shell tool.
        // (Which query tools are *named* depends on the host's $PATH, so assert
        // only the deterministic parts here; tool-naming is covered by the
        // presence-aware unit tests above.)
        if let ToolOutput::MCP(mcp) = &result {
            if let MCPOutputDetails::OkayOutput(text) = mcp.output() {
                assert!(text.contains("[MCP output truncated:"));
                assert!(text.contains(".json"), "annotation names the .json file");
                assert!(
                    text.contains(".json. "),
                    "file hint must end with a period before the steer: {text}"
                );
                assert!(
                    text.contains("to query the saved file"),
                    "JSON dump must steer to query the file: {}",
                    &text[text.len().saturating_sub(300)..]
                );
                assert!(
                    text.contains("`bash`"),
                    "steer references the resolved shell tool (fallback bash): {}",
                    &text[text.len().saturating_sub(300)..]
                );
                assert!(
                    !text.contains("if available"),
                    "presence is detected, so no 'if available' hedge: {}",
                    &text[text.len().saturating_sub(300)..]
                );
            } else {
                panic!("expected OkayOutput");
            }
        } else {
            panic!("expected ToolOutput::MCP");
        }
    }

    #[tokio::test]
    async fn no_steer_when_no_dump_file_written() {
        use crate::types::context::TruncationConfig;
        use crate::types::output::{MCPOutput, MCPOutputDetails};
        use crate::types::resources::{Resources, TruncationCfg};

        // LongLineJson, but no SessionFolder → no file → steer suppressed.
        let limit = 20_000;
        let big_json = format!("[{}]", vec!["1"; limit].join(","));
        let mut resources = Resources::new();
        let mut cfg = TruncationConfig::default();
        cfg.per_tool_max_output_bytes
            .insert("use_tool".to_string(), limit);
        resources.insert(TruncationCfg(cfg));

        let ctx = ctx_with_dispatch_and_resources(
            MockToolDispatch {
                expected_tool_name: "server__tool".into(),
                return_output: ToolOutput::MCP(MCPOutput::okay_output(
                    "server__tool".into(),
                    "server".into(),
                    big_json,
                )),
            },
            resources.into_shared(),
        );

        let result = kigi_tool_runtime::Tool::run(
            &UseTool,
            ctx,
            UseToolInput {
                tool_name: "server__tool".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();

        if let ToolOutput::MCP(mcp) = &result {
            if let MCPOutputDetails::OkayOutput(text) = mcp.output() {
                assert!(text.contains("[MCP output truncated:"));
                assert!(
                    !text.contains("ineffective on it") && !text.contains("to query"),
                    "no steer should be attached without a dump file: {text}"
                );
            } else {
                panic!("expected OkayOutput");
            }
        } else {
            panic!("expected ToolOutput::MCP");
        }
    }

    // ── Native-tool routing ─────────────────────────────────

    fn native_resources(native: &[&str]) -> crate::types::resources::SharedResources {
        use crate::types::resources::{EnabledNativeToolNames, Resources};
        let mut resources = Resources::new();
        let set: std::collections::HashSet<String> = native.iter().map(|s| s.to_string()).collect();
        resources.insert(EnabledNativeToolNames(set));
        resources.into_shared()
    }

    /// Resources with native-tool correction explicitly disabled.
    fn native_resources_correction_off(
        native: &[&str],
    ) -> crate::types::resources::SharedResources {
        use crate::types::resources::{EnabledNativeToolNames, Params, Resources};
        let mut resources = Resources::new();
        let set: std::collections::HashSet<String> = native.iter().map(|s| s.to_string()).collect();
        resources.insert(EnabledNativeToolNames(set));
        resources.insert(Params(UseToolParams {
            native_tool_correction: false,
        }));
        resources.into_shared()
    }

    fn ctx_capturing_with_resources(
        resources: crate::types::resources::SharedResources,
    ) -> (kigi_tool_runtime::ToolCallContext, SharedArgs) {
        let args: SharedArgs = Arc::new(std::sync::Mutex::new(None));
        let mut ctx = new_ctx();
        ctx.extensions
            .insert(InnerDispatch(Arc::new(CapturingDispatch {
                captured_args: Arc::clone(&args),
            })));
        ctx.extensions.insert(resources);
        (ctx, args)
    }

    #[tokio::test]
    async fn native_tool_returns_corrective_error_without_dispatching() {
        let (ctx, captured_args) =
            ctx_capturing_with_resources(native_resources(&["scheduler_create"]));

        let result = kigi_tool_runtime::Tool::run(
            &UseTool,
            ctx,
            UseToolInput {
                tool_name: "scheduler_create".into(),
                tool_input: serde_json::json!({"interval": "5m"}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.kind, kigi_tool_runtime::ToolErrorKind::InvalidArguments);
        assert!(err.detail.contains("native tool"), "got: {}", err.detail);
        assert!(err.detail.contains("scheduler_create"));
        assert!(err.detail.contains("directly"));
        assert!(
            captured_args.lock().unwrap().is_none(),
            "corrective error must NOT dispatch the native tool"
        );
    }

    #[tokio::test]
    async fn unknown_non_mcp_name_keeps_search_tool_steer() {
        // `jira` is NOT in the native set — it should hit the existing
        // "not a valid MCP tool name" steer and must not dispatch.
        let (ctx, captured_args) =
            ctx_capturing_with_resources(native_resources(&["scheduler_create"]));

        let result = kigi_tool_runtime::Tool::run(
            &UseTool,
            ctx,
            UseToolInput {
                tool_name: "jira".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            err.detail.contains("not a valid MCP tool name"),
            "got: {}",
            err.detail
        );
        assert!(
            captured_args.lock().unwrap().is_none(),
            "unknown name must NOT dispatch"
        );
    }

    #[tokio::test]
    async fn correction_disabled_falls_back_to_generic_warning() {
        // With native_tool_correction=false, even a known native tool name
        // gets the generic "not a valid MCP tool name" warning instead of
        // the corrective error — preserves the pre-fix generic warning path.
        let (ctx, captured_args) =
            ctx_capturing_with_resources(native_resources_correction_off(&["scheduler_create"]));

        let result = kigi_tool_runtime::Tool::run(
            &UseTool,
            ctx,
            UseToolInput {
                tool_name: "scheduler_create".into(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            err.detail.contains("not a valid MCP tool name"),
            "with correction disabled, native names should get the generic warning, got: {}",
            err.detail
        );
        assert!(
            !err.detail.contains("native tool"),
            "should NOT contain the corrective error text"
        );
        assert!(captured_args.lock().unwrap().is_none());
    }
}
