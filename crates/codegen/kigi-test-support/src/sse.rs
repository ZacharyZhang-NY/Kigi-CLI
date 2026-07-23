//! SSE stream generators for mock inference endpoints, producing the exact
//! wire format the real kigi sampling client parses.

use axum::response::sse::Event;
use serde_json::json;

use crate::scripted::SseEvent;

/// Anthropic Messages: one text block streamed as a single delta.
pub fn messages_api_events(text: &str, model: &str, stop_reason: &str) -> Vec<Event> {
    vec![
        Event::default().data(
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_test", "type": "message", "role": "assistant",
                    "content": [], "model": model, "stop_reason": null,
                    "usage": {
                        "input_tokens": 10, "output_tokens": 0,
                        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                    }
                }
            })
            .to_string(),
        ),
        Event::default().data(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
                .to_string(),
        ),
        Event::default().data(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})
                .to_string(),
        ),
        Event::default().data(json!({"type":"content_block_stop","index":0}).to_string()),
        Event::default().data(
            json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":5,"input_tokens":10}})
                .to_string(),
        ),
        Event::default().data(json!({"type":"message_stop"}).to_string()),
    ]
}

/// Streams `text` word-by-word, collapsing whitespace runs; use
/// [`chat_completion_events_exact`] when the receiver must reconstruct `text`
/// byte-for-byte.
pub fn chat_completion_events(text: &str, model: &str) -> Vec<Event> {
    chat_completion_events_from_deltas(&space_prefixed_deltas(text.split_whitespace()), model)
}

/// Like [`chat_completion_events`] but byte-exact: concatenating the deltas
/// reproduces `text` byte-for-byte. Fenced code blocks (mermaid etc.) need
/// their newlines to parse as a block, which `split_whitespace` destroys.
pub fn chat_completion_events_exact(text: &str, model: &str) -> Vec<Event> {
    chat_completion_events_from_deltas(&chat_completion_deltas(text), model)
}

/// Splits on single spaces only, so newlines and tabs stay inside the words
/// and concatenating the deltas reconstructs `text` byte-for-byte.
fn chat_completion_deltas(text: &str) -> Vec<String> {
    space_prefixed_deltas(text.split(' '))
}

/// The caller's iterator decides collapsing (echo) vs byte-exact (fixed).
fn space_prefixed_deltas<'a>(words: impl Iterator<Item = &'a str>) -> Vec<String> {
    words
        .enumerate()
        .map(|(i, word)| {
            if i == 0 {
                word.to_owned()
            } else {
                format!(" {word}")
            }
        })
        .collect()
}

fn chat_completion_events_from_deltas(deltas: &[String], model: &str) -> Vec<Event> {
    let n = deltas.len();
    let mut events = Vec::new();

    for (i, content) in deltas.iter().enumerate() {
        let finish_reason = if i + 1 == n {
            json!("stop")
        } else {
            json!(null)
        };

        let chunk = if i == 0 {
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant", "content": content },
                    "finish_reason": finish_reason
                }]
            })
        } else {
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "content": content },
                    "finish_reason": finish_reason
                }]
            })
        };
        events.push(Event::default().data(chunk.to_string()));
    }

    events.push(
        Event::default().data(
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": n,
                    "total_tokens": 10 + n
                }
            })
            .to_string(),
        ),
    );
    events.push(Event::default().data("[DONE]"));
    events
}

/// Streams `text` word-by-word, collapsing whitespace runs; use
/// [`responses_api_events_exact`] when the receiver must reconstruct `text`
/// byte-for-byte.
pub fn responses_api_events(text: &str, model: &str) -> Vec<Event> {
    let deltas: Vec<String> = text
        .split_whitespace()
        .map(|word| format!("{word} "))
        .collect();
    responses_api_events_from_deltas(&deltas, text, model)
}

/// Like [`responses_api_events`] but byte-exact: concatenating the deltas
/// reproduces `text` byte-for-byte.
pub fn responses_api_events_exact(text: &str, model: &str) -> Vec<Event> {
    responses_api_events_from_deltas(&responses_api_deltas(text), text, model)
}

/// `split_inclusive(' ')` keeps each chunk's trailing space, so concatenating
/// the chunks reconstructs `text` byte-for-byte (newlines included).
fn responses_api_deltas(text: &str) -> Vec<String> {
    text.split_inclusive(' ').map(str::to_owned).collect()
}

// `deltas` and `text` deliberately disagree in echo mode: collapsed deltas, uncollapsed
// `response.completed` text — inherited load-bearing shell behavior, do not unify.
fn responses_api_events_from_deltas(deltas: &[String], text: &str, model: &str) -> Vec<Event> {
    let mut events = Vec::new();
    let mut seq = 0;

    events.push(
        Event::default().data(
            json!({
                "type": "response.created",
                "sequence_number": seq,
                "response": {
                    "id": "resp_test",
                    "object": "response",
                    "created_at": 1234567890,
                    "model": model,
                    "status": "in_progress",
                    "output": []
                }
            })
            .to_string(),
        ),
    );
    seq += 1;

    for chunk in deltas {
        events.push(
            Event::default().data(
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": seq,
                    "item_id": "item_test",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": chunk
                })
                .to_string(),
            ),
        );
        seq += 1;
    }

    events.push(
        Event::default().data(
            json!({
                "type": "response.completed",
                "sequence_number": seq,
                "response": {
                    "id": "resp_test",
                    "object": "response",
                    "created_at": 1234567890,
                    "model": model,
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_test",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                            "annotations": []
                        }]
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "total_tokens": 15,
                        "input_tokens_details": { "cached_tokens": 0 },
                        "output_tokens_details": { "reasoning_tokens": 0 }
                    }
                }
            })
            .to_string(),
        ),
    );
    events.push(Event::default().data("[DONE]"));
    events
}

/// Reasoning-only completion: reasoning summary deltas and a `reasoning`
/// output item, with no message, output text or tool call.
/// `response_to_conversation_items` appends an empty assistant, yielding
/// `[Reasoning, Assistant("")]`, so the turn classifies as
/// `EmptyReason::ReasoningOnly` and the sampler resamples (the model doomloop).
///
/// Returns [`SseEvent`]s rather than axum `Event`s because this is a scripted
/// scenario ([`crate::ScriptedResponse::sse`] / `enqueue_response`), not an
/// echo/fixed response mode wired into the `mock_server` handlers.
pub fn responses_api_reasoning_only_events(reasoning: &str, model: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut seq = 0;

    events.push(SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
    ));
    seq += 1;

    for word in reasoning.split_whitespace() {
        events.push(SseEvent::data(
            json!({
                "type": "response.reasoning_summary_text.delta",
                "sequence_number": seq,
                "item_id": "reasoning_item_1",
                "output_index": 0,
                "summary_index": 0,
                "delta": format!("{word} ")
            })
            .to_string(),
        ));
        seq += 1;
    }

    events.push(SseEvent::data(
        json!({
            "type": "response.completed",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "completed",
                "output": [{
                    "type": "reasoning",
                    "id": "reasoning_item_1",
                    "summary": [{
                        "type": "summary_text",
                        "text": reasoning
                    }],
                    "status": "completed"
                }],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "total_tokens": 15,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 5 }
                }
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]"));
    events
}

/// Reasoning summary deltas first, then a normal text answer: the shape a
/// reasoning-capable model produces on an ordinary turn. `response.completed`
/// carries both output items, so the collector yields
/// `[Reasoning, Assistant(text)]` — a full, non-empty turn.
pub fn responses_api_reasoning_and_text_events(
    reasoning: &str,
    text: &str,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut seq = 0;

    events.push(SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
    ));
    seq += 1;

    for word in reasoning.split_whitespace() {
        events.push(SseEvent::data(
            json!({
                "type": "response.reasoning_summary_text.delta",
                "sequence_number": seq,
                "item_id": "reasoning_item_1",
                "output_index": 0,
                "summary_index": 0,
                "delta": format!("{word} ")
            })
            .to_string(),
        ));
        seq += 1;
    }

    for word in text.split_whitespace() {
        events.push(SseEvent::data(
            json!({
                "type": "response.output_text.delta",
                "sequence_number": seq,
                "item_id": "item_test",
                "output_index": 1,
                "content_index": 0,
                "delta": format!("{word} ")
            })
            .to_string(),
        ));
        seq += 1;
    }

    events.push(SseEvent::data(
        json!({
            "type": "response.completed",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "completed",
                "output": [
                    {
                        "type": "reasoning",
                        "id": "reasoning_item_1",
                        "summary": [{
                            "type": "summary_text",
                            "text": reasoning
                        }],
                        "status": "completed"
                    },
                    {
                        "type": "message",
                        "id": "msg_test",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                            "annotations": []
                        }]
                    }
                ],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 10,
                    "total_tokens": 20,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 5 }
                }
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]"));
    events
}

/// SSE `event:` name and payload `type` of the non-standard doom-loop check
/// event, duplicating `kigi_sampling_types::DOOM_LOOP_CHECK_EVENT_TYPE` like
/// every other wire string here; the shell integration tests pin the two
/// spellings against each other by absorbing built frames through the real
/// client.
const DOOM_LOOP_CHECK_EVENT: &str = "response.doom_loop_check";

fn doom_loop_check_frame(triggers: &[&str], seq: u64) -> SseEvent {
    SseEvent::with_event(
        DOOM_LOOP_CHECK_EVENT,
        json!({
            "sequence_number": seq,
            "type": DOOM_LOOP_CHECK_EVENT,
            "doom_loop_check": { "triggers": triggers }
        })
        .to_string(),
    )
}

/// Inject `doom_loop_check.triggers` into a turn's terminal
/// `response.completed` object — the dual of the mid-stream
/// [`doom_loop_check_frame`]. Composes over any turn builder; re-serialization
/// may reorder JSON keys, which is safe because clients and shape tests parse
/// these frames rather than byte-compare them. Every builder emits a completed
/// frame, so a miss is a script bug.
fn with_terminal_doom_loop_field(mut events: Vec<SseEvent>, triggers: &[&str]) -> Vec<SseEvent> {
    let patched = events.iter_mut().any(|e| {
        if e.data == "[DONE]" {
            return false;
        }
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&e.data) else {
            return false;
        };
        if value["type"] != "response.completed" {
            return false;
        }
        value["response"]["doom_loop_check"] = json!({ "triggers": triggers });
        e.data = value.to_string();
        true
    });
    assert!(
        patched,
        "turn builders always emit a response.completed frame"
    );
    events
}

/// Server-detected doom loop: a reasoning-only stream (the doomed signature —
/// the model loops in its thinking and never answers) followed by named
/// `response.doom_loop_check` frames carrying the growing **cumulative**
/// trigger set (one frame per prefix of `triggers`, mirroring how the server
/// re-emits as new triggers appear), and a terminal `response.completed` whose
/// response object carries the full set under `doom_loop_check.triggers`.
pub fn responses_api_doom_loop_check_events(
    triggers: &[&str],
    reasoning: &str,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = responses_api_reasoning_only_events(reasoning, model);
    // Cumulative frames land between the deltas and the terminal event; the
    // frame seq roughly continues the stream (clients never validate it).
    for prefix_len in 1..=triggers.len() {
        let at = events.len() - 2;
        events.insert(
            at,
            doom_loop_check_frame(&triggers[..prefix_len], at as u64),
        );
    }
    with_terminal_doom_loop_field(events, triggers)
}

/// An ordinary reasoning + text turn whose terminal `response.completed`
/// object carries `doom_loop_check.triggers` with NO mid-stream check frame —
/// the terminal-only copy of the signal.
pub fn responses_api_doom_loop_terminal_only_events(
    triggers: &[&str],
    reasoning: &str,
    text: &str,
    model: &str,
) -> Vec<SseEvent> {
    with_terminal_doom_loop_field(
        responses_api_reasoning_and_text_events(reasoning, text, model),
        triggers,
    )
}

/// Splice ONE named `response.doom_loop_check` frame with an arbitrary
/// `data:` payload — a byte-exact wire fixture or a malformed variant — into
/// an otherwise-normal reasoning + text turn, right after `response.created`.
/// The payload's own `sequence_number` (if any) is its business: clients never
/// validate sequence continuity.
pub fn responses_api_with_doom_loop_frame(
    check_frame_data: &str,
    reasoning: &str,
    text: &str,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = responses_api_reasoning_and_text_events(reasoning, text, model);
    events.insert(
        1,
        SseEvent::with_event(DOOM_LOOP_CHECK_EVENT, check_frame_data),
    );
    events
}

/// Reasoning summary deltas first, then one `function_call` — the shape a
/// reasoning-capable model produces when it thinks before its first tool call.
/// `response.completed` carries both output items and no message, so the
/// collector yields `[Reasoning, ToolCall]`; the tool call keeps the turn
/// non-empty, so there is no `EmptyReason::ReasoningOnly` resample.
pub fn responses_api_reasoning_then_tool_call_events(
    reasoning: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut seq = 0;

    events.push(SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
    ));
    seq += 1;

    for word in reasoning.split_whitespace() {
        events.push(SseEvent::data(
            json!({
                "type": "response.reasoning_summary_text.delta",
                "sequence_number": seq,
                "item_id": "reasoning_item_1",
                "output_index": 0,
                "summary_index": 0,
                "delta": format!("{word} ")
            })
            .to_string(),
        ));
        seq += 1;
    }

    events.push(SseEvent::data(
        json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": seq,
            "item_id": call_id,
            "output_index": 1,
            "delta": arguments
        })
        .to_string(),
    ));
    seq += 1;

    events.push(SseEvent::data(
        json!({
            "type": "response.completed",
            "sequence_number": seq,
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "model": model,
                "status": "completed",
                "output": [
                    {
                        "type": "reasoning",
                        "id": "reasoning_item_1",
                        "summary": [{
                            "type": "summary_text",
                            "text": reasoning
                        }],
                        "status": "completed"
                    },
                    {
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments
                    }
                ],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "total_tokens": 30,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 5 }
                }
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]"));
    events
}

/// Chat Completions twin of [`responses_api_reasoning_then_tool_call_events`]:
/// `reasoning_content` deltas, then one `tool_calls` delta, then a
/// `finish_reason: "tool_calls"` chunk with usage.
pub fn chat_completions_reasoning_then_tool_call_events(
    reasoning: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    model: &str,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    for word in reasoning.split_whitespace() {
        events.push(SseEvent::data(
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "reasoning_content": format!("{word} ") },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ));
    }
    events.push(SseEvent::data(
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1234567890,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                },
                "finish_reason": null
            }]
        })
        .to_string(),
    ));
    events.push(SseEvent::data(
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1234567890,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]"));
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing: `split_whitespace` would collapse the fence's newlines
    /// onto one line, so a client would never parse it as a code block and
    /// diagram detection would silently fail.
    #[test]
    fn deltas_reconstruct_multiline_response_byte_for_byte() {
        let text = "Here is a flow:\n\n```mermaid\nflowchart TD\n  A --> B\n  B --> C\n```\n\nDone rendering.\n";

        assert_eq!(chat_completion_deltas(text).concat(), text);
        assert_eq!(responses_api_deltas(text).concat(), text);

        assert!(
            chat_completion_deltas(text)
                .concat()
                .contains("```mermaid\nflowchart TD\n")
        );
    }

    #[test]
    fn deltas_preserve_runs_of_whitespace() {
        let text = "a  b\tc\n";
        assert_eq!(chat_completion_deltas(text).concat(), text);
        assert_eq!(responses_api_deltas(text).concat(), text);
    }

    /// Structural shape guard only: a round-trip through
    /// `rs::ResponseStreamEvent` would pin the async-openai types directly,
    /// but that crate is not a dependency here. The shell integration test
    /// deserializes these events through the real client, covering the wire
    /// contract end-to-end.
    #[test]
    fn reasoning_only_events_carry_reasoning_and_no_output_text() {
        let events = responses_api_reasoning_only_events("alpha beta gamma", "m");
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));

        let parsed: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).expect("each event is valid JSON"))
            .collect();
        let types: Vec<&str> = parsed
            .iter()
            .map(|v| v["type"].as_str().expect("each event has a type tag"))
            .collect();

        let reasoning_delta = parsed
            .iter()
            .find(|v| v["type"] == "response.reasoning_summary_text.delta")
            .expect("must stream a reasoning summary delta");
        assert!(
            !reasoning_delta["delta"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "the reasoning delta must carry text"
        );
        assert!(
            !types.contains(&"response.output_text.delta"),
            "reasoning-only must not stream output text"
        );

        let completed = parsed
            .iter()
            .find(|v| v["type"] == "response.completed")
            .expect("must emit a completed event");
        let output = completed["response"]["output"]
            .as_array()
            .expect("completed carries an output array");
        let reasoning_item = output
            .iter()
            .find(|o| o["type"] == "reasoning")
            .expect("completed output must carry a reasoning item");
        assert!(
            !reasoning_item["summary"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "the reasoning item must carry summary text"
        );
        assert!(
            !output.iter().any(|o| o["type"] == "message"),
            "completed output must have no message item (no visible text)"
        );
    }

    #[test]
    fn reasoning_and_text_events_carry_both_items() {
        let events = responses_api_reasoning_and_text_events("alpha beta", "the answer", "m");
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));

        let parsed: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).expect("each event is valid JSON"))
            .collect();
        let types: Vec<&str> = parsed
            .iter()
            .map(|v| v["type"].as_str().expect("each event has a type tag"))
            .collect();

        let first_reasoning = types
            .iter()
            .position(|t| *t == "response.reasoning_summary_text.delta")
            .expect("must stream reasoning summary deltas");
        let first_text = types
            .iter()
            .position(|t| *t == "response.output_text.delta")
            .expect("must stream output text deltas");
        assert!(
            first_reasoning < first_text,
            "reasoning deltas must precede text deltas"
        );

        let completed = parsed
            .iter()
            .find(|v| v["type"] == "response.completed")
            .expect("must emit a completed event");
        let output = completed["response"]["output"]
            .as_array()
            .expect("completed carries an output array");
        assert_eq!(
            output[0]["summary"][0]["text"].as_str(),
            Some("alpha beta"),
            "completed output must carry the reasoning item first"
        );
        assert_eq!(
            output[1]["content"][0]["text"].as_str(),
            Some("the answer"),
            "completed output must carry the assistant message"
        );
    }

    #[test]
    fn reasoning_then_tool_call_events_carry_reasoning_and_function_call() {
        let events = responses_api_reasoning_then_tool_call_events(
            "alpha beta",
            "call_1",
            "read_file",
            "{\"target_file\":\"a.rs\"}",
            "m",
        );
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));

        let parsed: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).expect("each event is valid JSON"))
            .collect();
        let types: Vec<&str> = parsed
            .iter()
            .map(|v| v["type"].as_str().expect("each event has a type tag"))
            .collect();

        let first_reasoning = types
            .iter()
            .position(|t| *t == "response.reasoning_summary_text.delta")
            .expect("must stream reasoning summary deltas");
        let args_delta = types
            .iter()
            .position(|t| *t == "response.function_call_arguments.delta")
            .expect("must stream a function-call arguments delta");
        assert!(
            first_reasoning < args_delta,
            "reasoning deltas must precede the tool call"
        );
        assert!(
            !types.contains(&"response.output_text.delta"),
            "a think-then-call turn must not stream output text"
        );

        let completed = parsed
            .iter()
            .find(|v| v["type"] == "response.completed")
            .expect("must emit a completed event");
        let output = completed["response"]["output"]
            .as_array()
            .expect("completed carries an output array");
        assert_eq!(
            output[0]["summary"][0]["text"].as_str(),
            Some("alpha beta"),
            "completed output must carry the reasoning item first"
        );
        assert_eq!(output[1]["type"].as_str(), Some("function_call"));
        assert_eq!(output[1]["call_id"].as_str(), Some("call_1"));
        assert_eq!(output[1]["name"].as_str(), Some("read_file"));
        assert!(
            !output.iter().any(|o| o["type"] == "message"),
            "completed output must have no message item (no visible text)"
        );
    }

    #[test]
    fn chat_reasoning_then_tool_call_events_carry_reasoning_then_tool_call() {
        let events = chat_completions_reasoning_then_tool_call_events(
            "alpha beta",
            "call_1",
            "read_file",
            "{\"target_file\":\"a.rs\"}",
            "m",
        );
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));

        let parsed: Vec<serde_json::Value> = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str(&e.data).expect("each event is valid JSON"))
            .collect();
        let delta_at = |v: &serde_json::Value| v["choices"][0]["delta"].clone();

        let first_reasoning = parsed
            .iter()
            .position(|v| !delta_at(v)["reasoning_content"].is_null())
            .expect("must stream reasoning_content deltas");
        let tool_call = parsed
            .iter()
            .position(|v| !delta_at(v)["tool_calls"].is_null())
            .expect("must stream a tool_calls delta");
        assert!(
            first_reasoning < tool_call,
            "reasoning deltas must precede the tool call"
        );
        let call = delta_at(&parsed[tool_call])["tool_calls"][0].clone();
        assert_eq!(call["id"].as_str(), Some("call_1"));
        assert_eq!(call["function"]["name"].as_str(), Some("read_file"));
        assert!(
            parsed.iter().all(|v| delta_at(v)["content"]
                .as_str()
                .unwrap_or_default()
                .is_empty()),
            "a think-then-call turn must not stream visible content"
        );
        assert!(
            parsed
                .iter()
                .any(|v| v["choices"][0]["finish_reason"] == "tool_calls"),
            "the stream must finish with finish_reason tool_calls"
        );
    }

    #[test]
    fn doom_loop_check_events_send_growing_named_frames_and_terminal_field() {
        let events = responses_api_doom_loop_check_events(
            &["tail_repetition:4@response", "tail_repetition:2@response"],
            "looping thought",
            "m",
        );
        assert_eq!(events.last().map(|e| e.data.as_str()), Some("[DONE]"));

        let frames: Vec<&SseEvent> = events
            .iter()
            .filter(|e| e.event.as_deref() == Some(DOOM_LOOP_CHECK_EVENT))
            .collect();
        assert_eq!(frames.len(), 2, "one frame per cumulative prefix");
        let first: serde_json::Value = serde_json::from_str(&frames[0].data).unwrap();
        assert_eq!(first["type"], DOOM_LOOP_CHECK_EVENT);
        assert!(first["sequence_number"].is_u64());
        assert_eq!(
            first["doom_loop_check"]["triggers"],
            json!(["tail_repetition:4@response"])
        );
        let second: serde_json::Value = serde_json::from_str(&frames[1].data).unwrap();
        assert_eq!(
            second["doom_loop_check"]["triggers"],
            json!(["tail_repetition:4@response", "tail_repetition:2@response"])
        );

        let completed = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.data).unwrap())
            .find(|v| v["type"] == "response.completed")
            .expect("must emit a completed event");
        assert_eq!(
            completed["response"]["doom_loop_check"]["triggers"],
            json!(["tail_repetition:4@response", "tail_repetition:2@response"])
        );
        let output = completed["response"]["output"].as_array().unwrap();
        assert!(
            !output.iter().any(|o| o["type"] == "message"),
            "a doomed turn is reasoning-only (no message item)"
        );
    }

    #[test]
    fn doom_loop_terminal_only_events_carry_field_without_mid_stream_frame() {
        let events = responses_api_doom_loop_terminal_only_events(
            &["low_logprob@thinking"],
            "brief thought",
            "the answer",
            "m",
        );
        assert!(
            events.iter().all(|e| e.event.is_none()),
            "terminal-only variant must not emit a named check frame"
        );

        let completed = events
            .iter()
            .filter(|e| e.data != "[DONE]")
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.data).unwrap())
            .find(|v| v["type"] == "response.completed")
            .expect("must emit a completed event");
        assert_eq!(
            completed["response"]["doom_loop_check"]["triggers"],
            json!(["low_logprob@thinking"])
        );
        let output = completed["response"]["output"].as_array().unwrap();
        assert!(output.iter().any(|o| o["type"] == "message"));
        assert!(output.iter().any(|o| o["type"] == "reasoning"));
    }

    #[test]
    fn with_doom_loop_frame_splices_payload_verbatim() {
        let payload = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":42}}"#;
        let events = responses_api_with_doom_loop_frame(payload, "hm", "hi", "m");
        assert_eq!(events[1].event.as_deref(), Some(DOOM_LOOP_CHECK_EVENT));
        assert_eq!(events[1].data, payload);
        let created: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(created["type"], "response.created");
    }
}
