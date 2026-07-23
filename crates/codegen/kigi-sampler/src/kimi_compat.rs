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

/// Dialect-dispatched body adaptation. Kimi keeps the full historical
/// pipeline (thinking + message hygiene + schema normalization — all built
/// for the Kimi wire's strictness); DeepSeek differs ONLY in how thinking
/// rides the body; Passthrough providers take OpenAI-style bodies verbatim
/// (their `reasoning_effort` scalar is already the wire form).
pub(crate) fn adapt_chat_completions_body_for(
    compat: kigi_sampling_types::ChatCompat,
    body: &mut Value,
) {
    match compat {
        kigi_sampling_types::ChatCompat::Kimi => adapt_chat_completions_body(body),
        kigi_sampling_types::ChatCompat::DeepSeek => {
            adapt_thinking_deepseek(body);
            strip_kigi_private_message_fields(body);
        }
        kigi_sampling_types::ChatCompat::Passthrough => {
            strip_kigi_private_message_fields(body);
        }
        kigi_sampling_types::ChatCompat::StrictOpenAi => {
            strip_kigi_private_message_fields(body);
            strip_stream_options(body);
        }
        kigi_sampling_types::ChatCompat::Mistral => {
            strip_kigi_private_message_fields(body);
            strip_stream_options(body);
            normalize_mistral_tool_call_ids(body);
        }
    }
}

/// Mistral's validator requires tool-call ids of EXACTLY nine
/// `[a-zA-Z0-9]` characters. Foreign backends mint arbitrary ids
/// (OpenAI `call_…`, UUIDs, Anthropic `toolu_…`), so non-conforming ids
/// are remapped deterministically — ported from Pi's
/// `mistral-conversations.ts` normalizer: strip non-alphanumerics, keep
/// the id when the result is already exactly nine chars, otherwise hash
/// (FNV-1a → base36) down to nine, retrying with an attempt suffix on
/// collision. ONE map serves `tool_calls[].id` and `tool_call_id` alike,
/// so call/result pairing survives.
fn normalize_mistral_tool_call_ids(body: &mut Value) {
    const LEN: usize = 9;

    fn derive(id: &str, attempt: u32) -> String {
        let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
        if attempt == 0 && normalized.len() == LEN {
            return normalized;
        }
        let seed_base = if normalized.is_empty() {
            id
        } else {
            &normalized
        };
        let seed = if attempt == 0 {
            seed_base.to_string()
        } else {
            format!("{seed_base}:{attempt}")
        };
        // FNV-1a (stable across builds, unlike std's DefaultHasher) → base36.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in seed.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let mut out = String::with_capacity(LEN);
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut h = hash;
        while out.len() < LEN {
            out.push(digits[(h % 36) as usize] as char);
            // +1 keeps the stream from collapsing to zeros
            h = h / 36 + 1;
        }
        out
    }

    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    let mut forward: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut normalize = |id: &str| -> String {
        if let Some(mapped) = forward.get(id) {
            return mapped.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive(id, attempt);
            if taken.insert(candidate.clone()) {
                forward.insert(id.to_string(), candidate.clone());
                return candidate;
            }
            attempt += 1;
        }
    };
    for message in messages.iter_mut() {
        if let Some(tool_calls) = message.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
            for tc in tool_calls {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()).map(str::to_owned) {
                    tc["id"] = Value::String(normalize(&id));
                }
            }
        }
        if let Some(id) = message
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        {
            message["tool_call_id"] = Value::String(normalize(&id));
        }
    }
}

/// Mistral's strict Pydantic validator 422-rejects `stream_options`
/// (`extra_forbidden` on `stream_options.include_usage`; its request model
/// has no such field). kigi injects `stream_options.include_usage` on every
/// streaming request for the other providers, so strip the whole object for
/// Mistral. Streaming usage falls back to token estimation (as for any
/// provider that omits streaming usage).
fn strip_stream_options(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("stream_options");
    }
}

/// Remove kigi-internal history artifacts from input messages before they
/// reach a non-Kimi wire. `reasoning_content` is Kimi's replayed-thinking
/// field (Kimi consumes it; DeepSeek documents it as prefix-mode-only and
/// historically 400s on it; other providers don't know it) and `model_id`
/// is kigi's private per-message provenance. Kimi's own pipeline handles
/// these in `adapt_messages`.
fn strip_kigi_private_message_fields(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for message in messages {
        if let Some(obj) = message.as_object_mut() {
            obj.remove("reasoning_content");
            obj.remove("model_id");
        }
    }
}

/// DeepSeek spells the thinking control `thinking:{type, reasoning_effort}`
/// (api-docs.deepseek.com create-chat-completion; the server maps
/// low/medium→high and xhigh→max itself, so the canonical level passes
/// through verbatim). `none` disables thinking; absent leaves the server
/// default (enabled).
fn adapt_thinking_deepseek(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(effort) = obj.remove("reasoning_effort") else {
        return;
    };
    let Some(level) = effort.as_str().map(str::to_owned) else {
        return;
    };
    if level == "none" {
        obj.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": "disabled" }),
        );
    } else {
        obj.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": "enabled", "reasoning_effort": level }),
        );
    }
}

/// Map the OpenAI-style `reasoning_effort` knob onto Kimi's `thinking`
/// request field and drop `reasoning_effort` from the wire.
///
/// kimi-cli 1.49.0 controls thinking through the request body's
/// `thinking: {"type": "enabled" | "disabled"}` field
/// (packages/kosong/src/kosong/chat_provider/kimi.py:214-223 `with_thinking`:
/// `"enabled" if effort != "off" else "disabled"`; wired by
/// src/kimi_cli/llm.py:475-481). When no effort is configured, nothing is
/// sent and the server default applies (llm.py:482 "leave as-is").
///
/// Models with selectable levels (the `/models` `think_efforts` block, e.g.
/// K3's low/high/max) additionally take the level as `thinking.effort` —
/// verified against the live api.kimi.com: `{"type": "enabled", "effort":
/// "low"}` is accepted, values outside `valid_efforts` are a 400. The
/// catalog gates efforts to that per-model list, so this layer only renames
/// the one canonical-vs-wire divergence (`xhigh` → `max`) and passes the
/// level through verbatim — inventing or clamping a level here would hide a
/// real contract violation.
fn adapt_thinking(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(effort) = obj.remove("reasoning_effort") else {
        return;
    };
    let effort = effort.as_str().map(str::to_owned);
    let enabled = effort.as_deref() != Some("none");
    let mut thinking = serde_json::Map::new();
    thinking.insert(
        "type".to_owned(),
        Value::String(if enabled { "enabled" } else { "disabled" }.to_owned()),
    );
    if enabled && let Some(level) = effort {
        let wire_level = if level == "xhigh" {
            "max".to_owned()
        } else {
            level
        };
        thinking.insert("effort".to_owned(), Value::String(wire_level));
    }
    obj.insert("thinking".to_owned(), Value::Object(thinking));
}

/// Message-level adaptations:
///
/// * Drop `model_id` — a kigi extension recorded on assistant turns;
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
    fn deepseek_dialect_spells_thinking_reasoning_effort() {
        use kigi_sampling_types::ChatCompat;
        // Official docs: thinking:{type, reasoning_effort}; server maps
        // low/medium→high, xhigh→max itself — levels pass through verbatim.
        let mut body = json!({ "model": "deepseek-v4-pro", "reasoning_effort": "high" });
        adapt_chat_completions_body_for(ChatCompat::DeepSeek, &mut body);
        assert_eq!(body.get("reasoning_effort"), None);
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "reasoning_effort": "high" })
        );

        let mut body = json!({ "model": "deepseek-v4-flash", "reasoning_effort": "max" });
        adapt_chat_completions_body_for(ChatCompat::DeepSeek, &mut body);
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "reasoning_effort": "max" })
        );

        // none disables; absent leaves the server default (no thinking key).
        let mut body = json!({ "reasoning_effort": "none" });
        adapt_chat_completions_body_for(ChatCompat::DeepSeek, &mut body);
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        let mut body = json!({ "model": "deepseek-chat" });
        adapt_chat_completions_body_for(ChatCompat::DeepSeek, &mut body);
        assert_eq!(body.get("thinking"), None);

        // DeepSeek does NOT get kimi's message/tool-schema rewrites (empty
        // assistant tool-call content survives — DeepSeek's documented
        // function-calling round-trip uses that shape), but kigi-private
        // fields are stripped: replayed reasoning_content is prefix-mode-only
        // on the DeepSeek wire (historically a 400 in input messages).
        let mut body = json!({
            "reasoning_effort": "high",
            "messages": [
                { "role": "assistant", "content": "", "tool_calls": [{}],
                  "reasoning_content": "replayed thinking", "model_id": "kigi/x" }
            ]
        });
        adapt_chat_completions_body_for(ChatCompat::DeepSeek, &mut body);
        assert_eq!(body["messages"][0]["content"], json!(""));
        assert_eq!(body["messages"][0].get("reasoning_content"), None);
        assert_eq!(body["messages"][0].get("model_id"), None);
    }

    #[test]
    fn strict_openai_dialect_strips_stream_options_and_private_fields() {
        use kigi_sampling_types::ChatCompat;
        // Mistral 422s on stream_options (extra_forbidden) and doesn't know
        // kigi's private message fields; OpenAI-style reasoning_effort stays.
        let mut body = json!({
            "model": "mistral-medium-latest",
            "reasoning_effort": "high",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [
                { "role": "assistant", "content": "hi",
                  "reasoning_content": "internal", "model_id": "kigi/x" }
            ]
        });
        adapt_chat_completions_body_for(ChatCompat::StrictOpenAi, &mut body);
        assert_eq!(
            body.get("stream_options"),
            None,
            "stream_options must be stripped"
        );
        assert_eq!(body["stream"], json!(true), "stream flag stays");
        assert_eq!(
            body["reasoning_effort"],
            json!("high"),
            "OpenAI-style effort passes through (Mistral accepts it natively)"
        );
        assert_eq!(body["messages"][0].get("reasoning_content"), None);
        assert_eq!(body["messages"][0].get("model_id"), None);
        assert_eq!(body["messages"][0]["content"], json!("hi"));
    }

    #[test]
    fn passthrough_dialect_leaves_openai_body_verbatim() {
        use kigi_sampling_types::ChatCompat;
        // Verbatim EXCEPT kigi-private history artifacts, which no non-Kimi
        // wire understands.
        let mut body = json!({
            "model": "gpt-oss",
            "reasoning_effort": "high",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "yo",
                  "reasoning_content": "internal", "model_id": "kigi/x" }
            ]
        });
        adapt_chat_completions_body_for(ChatCompat::Passthrough, &mut body);
        assert_eq!(
            body,
            json!({
                "model": "gpt-oss",
                "reasoning_effort": "high",
                "messages": [
                    { "role": "user", "content": "hi" },
                    { "role": "assistant", "content": "yo" }
                ]
            }),
            "reasoning_effort stays OpenAI-style; private fields are stripped"
        );
    }

    #[test]
    fn kimi_dialect_dispatch_matches_legacy_pipeline() {
        use kigi_sampling_types::ChatCompat;
        let mut via_dispatch = json!({ "model": "k3", "reasoning_effort": "max" });
        adapt_chat_completions_body_for(ChatCompat::Kimi, &mut via_dispatch);
        let mut via_legacy = json!({ "model": "k3", "reasoning_effort": "max" });
        adapt_chat_completions_body(&mut via_legacy);
        assert_eq!(via_dispatch, via_legacy, "Kimi dispatch = legacy pipeline");
    }

    #[test]
    fn reasoning_effort_maps_to_kimi_thinking_field() {
        // Level rides along as thinking.effort (live wire: 200 with
        // {"type": "enabled", "effort": "low"}).
        let mut body = json!({ "model": "kimi-for-coding", "reasoning_effort": "high" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(body.get("reasoning_effort"), None);
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "effort": "high" })
        );

        // Legacy canonical `xhigh` (pre-Max configs/sessions) is spelled
        // `max` on the Kimi wire (the K3 valid_efforts vocabulary is
        // low/high/max — there is no `xhigh` there).
        let mut body = json!({ "model": "k3", "reasoning_effort": "xhigh" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "effort": "max" })
        );

        // Canonical `max` (what the K3 menu token parses to since the
        // ReasoningEffort::Max split) passes through `unchanged`.
        let mut body = json!({ "model": "k3", "reasoning_effort": "max" });
        adapt_chat_completions_body(&mut body);
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "effort": "max" })
        );

        // kimi.py:218: "off" (our ReasoningEffort::None) → disabled, and no
        // effort key (a disabled+effort combination would be contradictory).
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
        assert_eq!(assistant.get("model_id"), None, "kigi extension dropped");
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

    /// Mistral dialect: exactly-nine `[a-zA-Z0-9]` tool-call ids. A
    /// conforming id survives; foreign ids (OpenAI `call_…`, UUIDs) remap
    /// deterministically; the SAME map serves `tool_calls[].id` and
    /// `tool_call_id`, so pairing survives; distinct inputs never collide.
    #[test]
    fn mistral_dialect_normalizes_tool_call_ids_symmetrically() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "abc123XYZ", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                    {"id": "call_0123456789abcdef", "type": "function", "function": {"name": "b", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "abc123XYZ", "content": "r1"},
                {"role": "tool", "tool_call_id": "call_0123456789abcdef", "content": "r2"},
            ],
            "stream_options": {"include_usage": true}
        });
        adapt_chat_completions_body_for(kigi_sampling_types::ChatCompat::Mistral, &mut body);

        let msgs = body["messages"].as_array().unwrap();
        let ids: Vec<String> = msgs[0]["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tc| tc["id"].as_str().unwrap().to_string())
            .collect();
        // Conforming id kept verbatim.
        assert_eq!(ids[0], "abc123XYZ");
        // Foreign id remapped to exactly nine alphanumerics.
        assert_eq!(ids[1].len(), 9, "{ids:?}");
        assert!(ids[1].chars().all(|ch| ch.is_ascii_alphanumeric()));
        assert_ne!(ids[0], ids[1], "distinct inputs must not collide");
        // Results carry the SAME mapped ids.
        assert_eq!(msgs[1]["tool_call_id"].as_str().unwrap(), ids[0]);
        assert_eq!(msgs[2]["tool_call_id"].as_str().unwrap(), ids[1]);
        // StrictOpenAi base behavior rides along.
        assert!(body.get("stream_options").is_none());

        // Determinism: the same foreign id maps identically in a fresh body.
        let mut body2 = serde_json::json!({
            "messages": [
                {"role": "tool", "tool_call_id": "call_0123456789abcdef", "content": "r"}
            ]
        });
        adapt_chat_completions_body_for(kigi_sampling_types::ChatCompat::Mistral, &mut body2);
        assert_eq!(
            body2["messages"][0]["tool_call_id"].as_str().unwrap(),
            ids[1],
            "remap must be deterministic across requests (prefix-cache stability)"
        );
    }
}
