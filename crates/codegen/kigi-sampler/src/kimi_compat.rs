//! Kimi (Moonshot) chat/completions request adaptations.
//!
//! The Kimi endpoints are OpenAI-compatible but deviate in a handful of
//! places (PRD F3 Q1). Every request-side deviation is absorbed HERE, in a
//! single adaptation point applied to the serialized chat/completions body
//! just before it is sent — never as scattered special-cases at call sites.
//! Each adaptation cites the kimi-cli source it was derived from
//! (kimi-cli == the authoritative official client; paths are relative to
//! that repository).
//!
//! Response-side deviations live with the wire types themselves
//! (`kigi_sampling_types::Usage::cached_tokens`,
//! `ChatChunkChoice::usage`) and the L2 stream transform
//! (`stream::chat_completions` synthesizes missing tool-call ids).
//!
//! The `ApiBackend::ChatCompletions` backend is the Kimi dialect: both
//! product channels (subscription OAuth and Moonshot API keys) ride it.
//! Custom providers that need vanilla OpenAI semantics for reasoning use
//! the `Responses` backend, which stays available in model configuration.

use serde_json::Value;

/// Adapt a fully-serialized chat/completions request body to the Kimi
/// dialect, in place. Applied by [`crate::SamplingClient`] to both the
/// streaming and non-streaming chat/completions paths.
pub(crate) fn adapt_chat_completions_body(body: &mut Value) {
    adapt_thinking(body);
    adapt_messages(body);
    adapt_tool_schemas(body);
}

/// Map the OpenAI-style `reasoning_effort` knob onto Kimi's `thinking`
/// request field and drop `reasoning_effort` from the wire.
///
/// kimi-cli controls thinking exclusively through the request body's
/// `thinking: {"type": "enabled" | "disabled"}` field
/// (packages/kosong/src/kosong/chat_provider/kimi.py:214-223 `with_thinking`:
/// `"enabled" if effort != "off" else "disabled"`; wired by
/// src/kimi_cli/llm.py:475-481). When no effort is configured, nothing is
/// sent and the server default applies (llm.py:482 "leave as-is").
fn adapt_thinking(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(effort) = obj.remove("reasoning_effort") else {
        return;
    };
    let enabled = effort.as_str() != Some("none");
    obj.insert(
        "thinking".to_owned(),
        serde_json::json!({ "type": if enabled { "enabled" } else { "disabled" } }),
    );
}

/// Message-level adaptations:
///
/// * Drop `model_id` — a grok-build extension recorded on assistant turns;
///   kimi-cli's message serializer sends no such field
///   (packages/kosong/src/kosong/chat_provider/kimi.py:326-353).
/// * Drop `content` from assistant tool-call messages whose visible content
///   is effectively empty. The Kimi-for-Coding compat layer rejects an
///   empty text content part with 400 "text content is empty"; omitting
///   `content` entirely is always accepted
///   (packages/kosong/src/kosong/chat_provider/kimi.py:339-350, with the
///   "effectively empty" predicate at kimi.py:356-362).
fn adapt_messages(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(obj) = message.as_object_mut() else {
            continue;
        };
        obj.remove("model_id");
        let is_assistant = obj.get("role").and_then(Value::as_str) == Some("assistant");
        let has_tool_calls = obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if is_assistant
            && has_tool_calls
            && obj.get("content").is_some_and(is_effectively_empty_content)
        {
            obj.remove("content");
        }
    }
}

/// Port of kimi-cli `_is_effectively_empty_content_parts`
/// (packages/kosong/src/kosong/chat_provider/kimi.py:356-362): a bare
/// whitespace-only string, or a block list whose entries are all
/// whitespace-only text blocks. Any non-text block (e.g. an image) makes
/// the content non-empty.
fn is_effectively_empty_content(content: &Value) -> bool {
    match content {
        Value::String(s) => s.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().all(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.trim().is_empty())
        }),
        Value::Null => true,
        _ => false,
    }
}

/// Moonshot's schema validator rejects tool parameter schemas whose
/// property schemas omit `type` (e.g. enum-only properties exposed by some
/// MCP servers): HTTP 400 "At path 'properties.X': type is not defined".
/// Fill in an inferred `type` locally so such tools keep working. Port of
/// kimi-cli `ensure_property_types`
/// (packages/kosong/src/kosong/utils/jsonschema.py:88-142, applied per tool
/// at packages/kosong/src/kosong/chat_provider/kimi.py:378-388).
fn adapt_tool_schemas(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if let Some(parameters) = tool.pointer_mut("/function/parameters") {
            recurse_schema(parameters);
        }
    }
}

/// JSON Schema keywords that describe a property's shape without a `type`
/// keyword; nodes carrying one are left alone
/// (kosong/utils/jsonschema.py:15-24 `_COMBINATOR_KEYS`).
const COMBINATOR_KEYS: [&str; 8] = [
    "anyOf", "oneOf", "allOf", "not", "if", "then", "else", "$ref",
];

/// Walk property-schema positions under `node` (`properties`, `items`,
/// `additionalProperties`, `anyOf`/`oneOf`/`allOf`); `node` itself is a
/// container and is not normalized (kosong/utils/jsonschema.py:114-142).
fn recurse_schema(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
        for value in props.values_mut() {
            normalize_property(value);
        }
    }
    match obj.get_mut("items") {
        Some(items @ Value::Object(_)) => normalize_property(items),
        Some(Value::Array(items)) => {
            for value in items {
                normalize_property(value);
            }
        }
        _ => {}
    }
    if let Some(additional @ Value::Object(_)) = obj.get_mut("additionalProperties") {
        normalize_property(additional);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for value in branches {
                normalize_property(value);
            }
        }
    }
}

/// Ensure a property schema declares `type`, then recurse into it
/// (kosong/utils/jsonschema.py:145-162 `_normalize_property`).
fn normalize_property(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    if !obj.contains_key("type") && !COMBINATOR_KEYS.iter().any(|k| obj.contains_key(*k)) {
        let inferred = if let Some(Value::Array(values)) = obj.get("enum") {
            if values.is_empty() {
                infer_type_from_structure(obj)
            } else {
                infer_type_from_values(values)
            }
        } else if let Some(constant) = obj.get("const") {
            infer_type_from_values(std::slice::from_ref(constant))
        } else {
            infer_type_from_structure(obj)
        };
        obj.insert("type".to_owned(), Value::String(inferred.to_owned()));
    }
    recurse_schema(node);
}

/// Infer `type` from structural keywords when no enum/const is present;
/// defaults to `"string"` only with no structural hints at all
/// (kosong/utils/jsonschema.py:165-215 `_infer_type_from_structure`).
fn infer_type_from_structure(obj: &serde_json::Map<String, Value>) -> &'static str {
    const OBJECT_KEYWORDS: [&str; 7] = [
        "properties",
        "additionalProperties",
        "patternProperties",
        "propertyNames",
        "required",
        "minProperties",
        "maxProperties",
    ];
    const ARRAY_KEYWORDS: [&str; 6] = [
        "items",
        "prefixItems",
        "minItems",
        "maxItems",
        "uniqueItems",
        "contains",
    ];
    const STRING_KEYWORDS: [&str; 4] = ["minLength", "maxLength", "pattern", "format"];
    const NUMERIC_KEYWORDS: [&str; 5] = [
        "minimum",
        "maximum",
        "multipleOf",
        "exclusiveMinimum",
        "exclusiveMaximum",
    ];
    if OBJECT_KEYWORDS.iter().any(|k| obj.contains_key(*k)) {
        "object"
    } else if ARRAY_KEYWORDS.iter().any(|k| obj.contains_key(*k)) {
        "array"
    } else if STRING_KEYWORDS.iter().any(|k| obj.contains_key(*k)) {
        "string"
    } else if NUMERIC_KEYWORDS.iter().any(|k| obj.contains_key(*k)) {
        "number"
    } else {
        "string"
    }
}

/// Infer a `type` from concrete enum/const values: single JSON type wins,
/// `{integer, number}` collapses to `"number"`, any other mix falls back to
/// `"string"` (kosong/utils/jsonschema.py:218-247 `_infer_type_from_values`).
fn infer_type_from_values(values: &[Value]) -> &'static str {
    let mut inferred = std::collections::BTreeSet::new();
    for value in values {
        let ty = match value {
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Null => "null",
            Value::Object(_) => "object",
            Value::Array(_) => "array",
        };
        inferred.insert(ty);
    }
    if inferred.len() == 1 {
        return inferred.pop_first().expect("non-empty set");
    }
    if inferred == std::collections::BTreeSet::from(["integer", "number"]) {
        return "number";
    }
    "string"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_effort_maps_to_kimi_thinking_field() {
        let mut body = json!({ "model": "kimi-for-coding", "reasoning_effort": "high" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body.get("reasoning_effort"), None);
        assert_eq!(body["thinking"], json!({ "type": "enabled" }));

        // kimi.py:218: "off" (our ReasoningEffort::None) → disabled.
        let mut body = json!({ "reasoning_effort": "none" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));

        // llm.py:482: unset → leave as-is (no `thinking` at all).
        let mut body = json!({ "model": "kimi-for-coding" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body.get("thinking"), None);
    }

    #[test]
    fn assistant_tool_call_with_empty_content_drops_content() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "hi" },
                {
                    "role": "assistant",
                    "content": "",
                    "model_id": "kimi-for-coding",
                    "tool_calls": [{ "id": "c1", "type": "function",
                                     "function": { "name": "f", "arguments": "{}" } }]
                },
            ]
        });
        adapt_chat_completions_body(&mut body);
        let assistant = &body["messages"][1];
        assert_eq!(assistant.get("content"), None, "empty content dropped");
        assert_eq!(assistant.get("model_id"), None, "grok extension dropped");
        assert!(assistant.get("tool_calls").is_some());
        // The user message keeps its content.
        assert_eq!(body["messages"][0]["content"], json!("hi"));
    }

    #[test]
    fn assistant_tool_call_with_real_content_keeps_content() {
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": "let me check",
                "tool_calls": [{ "id": "c1", "type": "function",
                                 "function": { "name": "f", "arguments": "{}" } }]
            }]
        });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body["messages"][0]["content"], json!("let me check"));
    }

    #[test]
    fn assistant_without_tool_calls_keeps_empty_content() {
        // Only tool-call turns drop content (kimi.py:339-350 guards on
        // `message.tool_calls`); a plain empty assistant turn is left alone.
        let mut body = json!({
            "messages": [{ "role": "assistant", "content": "" }]
        });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body["messages"][0]["content"], json!(""));
    }

    #[test]
    fn empty_text_block_list_counts_as_empty_content() {
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": [{ "type": "text", "text": "  " }],
                "tool_calls": [{ "id": "c1", "type": "function",
                                 "function": { "name": "f", "arguments": "{}" } }]
            }]
        });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body["messages"][0].get("content"), None);
    }

    #[test]
    fn image_block_is_not_empty_content() {
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "content": [{ "type": "image_url", "image_url": { "url": "data:x" } }],
                "tool_calls": [{ "id": "c1", "type": "function",
                                 "function": { "name": "f", "arguments": "{}" } }]
            }]
        });
        adapt_chat_completions_body(&mut body);
        assert!(body["messages"][0].get("content").is_some());
    }

    #[test]
    fn enum_only_property_gains_inferred_type() {
        // The Moonshot validator 400s on `{"enum": [...]}` without `type`
        // (kosong/utils/jsonschema.py:91-96).
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "mode": { "enum": ["smart", "full"] },
                            "count": { "enum": [1, 2, 3] },
                            "ratio": { "enum": [1, 2.5] },
                            "nested": {
                                "type": "object",
                                "properties": { "inner": { "enum": ["a"] } }
                            },
                            "combined": { "anyOf": [{ "type": "string" }] }
                        }
                    }
                }
            }]
        });
        adapt_chat_completions_body(&mut body);
        let props = &body["tools"][0]["function"]["parameters"]["properties"];
        assert_eq!(props["mode"]["type"], json!("string"));
        assert_eq!(props["count"]["type"], json!("integer"));
        assert_eq!(props["ratio"]["type"], json!("number"));
        assert_eq!(
            props["nested"]["properties"]["inner"]["type"],
            json!("string")
        );
        assert_eq!(
            props["combined"].get("type"),
            None,
            "combinator nodes are left alone"
        );
    }

    #[test]
    fn structural_keywords_infer_shape_not_string() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "t",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "obj": { "properties": { "x": { "type": "string" } } },
                            "arr": { "items": { "type": "string" } },
                            "num": { "minimum": 0 },
                            "free": {}
                        }
                    }
                }
            }]
        });
        adapt_chat_completions_body(&mut body);
        let props = &body["tools"][0]["function"]["parameters"]["properties"];
        assert_eq!(props["obj"]["type"], json!("object"));
        assert_eq!(props["arr"]["type"], json!("array"));
        assert_eq!(props["num"]["type"], json!("number"));
        assert_eq!(props["free"]["type"], json!("string"));
    }
}
