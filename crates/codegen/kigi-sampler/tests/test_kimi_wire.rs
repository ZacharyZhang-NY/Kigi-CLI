//! Kimi chat/completions wire tests (PRD F3 acceptance).
//!
//! Exercises the sampler end-to-end against a mock HTTP server:
//! * streaming happy path with `reasoning_content` deltas, tool-call deltas,
//!   and a Kimi-shaped usage chunk (usage riding inside the choice, cache
//!   hits as top-level `cached_tokens`),
//! * the request the wire actually carries: plain `Authorization: Bearer`,
//!   `User-Agent: kigi/{version}`, no xAI proxy marker headers, and the
//!   `crate::kimi_compat` body adaptations,
//! * 429 honoring the standard `Retry-After` header,
//! * mid-stream network drop recovering through the retry loop.
//!
//! 401-no-retry and the rate-limit retry threshold are covered by
//! `test_actor.rs`; this file does not duplicate them.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use futures_util::stream::{self};
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use kigi_sampler::{
    ApiBackend, RequestId, RetryPolicy, SamplerActor, SamplerConfig, SamplingChannel, SamplingEvent,
};
use kigi_sampling_types::{
    AssistantItem, ContentPart, ConversationItem, ConversationRequest, ReasoningEffort, ToolCall,
    ToolResultItem, ToolSpec, UserItem, synthesized_reasoning_item,
};

// Mock server harness (same shape as test_actor.rs)

struct MockServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl MockServer {
    async fn spawn(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn test_config(base_url: String) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("test-kimi-key".into()),
        base_url,
        model: "kimi-for-coding".into(),
        max_completion_tokens: Some(1024),
        api_backend: ApiBackend::ChatCompletions,
        extra_headers: IndexMap::new(),
        context_window: 128_000,
        max_retries: Some(3),
        idle_timeout_secs: Some(30),
        ..Default::default()
    }
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })],
        ..Default::default()
    }
}

fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> Event {
    let mut choice = json!({ "index": 0, "delta": delta });
    choice["finish_reason"] = finish.map(Value::from).unwrap_or(Value::Null);
    if let Some(u) = usage {
        // Kimi deviation under test: usage rides INSIDE the choice
        // (kimi-cli kimi.py:522-533 `extract_usage_from_chunk`).
        choice["usage"] = u;
    }
    let body = json!({
        "id": "chatcmpl-kimi",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "kimi-for-coding",
        "choices": [choice]
    });
    Event::default().data(body.to_string())
}

async fn drain_until_terminal(
    rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
    timeout: Duration,
) -> Vec<SamplingEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for terminal event")
            .expect("event channel closed before terminal event");
        let terminal = matches!(
            ev,
            SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. }
        );
        out.push(ev);
        if terminal {
            return out;
        }
    }
}

// Streaming happy path: reasoning + tool calls + Kimi usage shapes

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kimi_stream_reasoning_tool_calls_and_choice_usage() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let events = vec![
                chunk(
                    json!({ "role": "assistant", "reasoning_content": "let me think" }),
                    None,
                    None,
                ),
                chunk(json!({ "reasoning_content": " harder" }), None, None),
                chunk(json!({ "content": "Running the tool." }), None, None),
                // Tool call split across chunks: id+name first, args continue.
                chunk(
                    json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                             "function": { "name": "read_file", "arguments": "{\"path\":" } }] }),
                    None,
                    None,
                ),
                chunk(
                    json!({ "tool_calls": [{ "index": 0,
                             "function": { "arguments": "\"a.rs\"}" } }] }),
                    None,
                    None,
                ),
                // Terminal chunk: finish_reason + usage inside the choice with
                // Moonshot's top-level `cached_tokens` (kimi.py:427-431).
                chunk(
                    json!({}),
                    Some("tool_calls"),
                    Some(json!({
                        "prompt_tokens": 100,
                        "completion_tokens": 20,
                        "total_tokens": 120,
                        "cached_tokens": 60
                    })),
                ),
            ];
            Sse::new(stream::iter(
                events.into_iter().map(Ok::<_, std::convert::Infallible>),
            ))
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        test_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-kimi"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(30)).await;
    server.shutdown();

    // Reasoning tokens stream on the Reasoning channel, text on Text.
    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Reasoning,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "let me think harder");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Running the tool.");

    // Tool-call deltas surfaced incrementally.
    assert!(events.iter().any(|e| matches!(
        e,
        SamplingEvent::ToolCallDelta { id: Some(id), .. } if id == "call_1"
    )));

    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let calls = response.tool_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id.as_ref(), "call_1");
            assert_eq!(calls[0].name, "read_file");
            assert_eq!(calls[0].arguments.as_ref(), "{\"path\":\"a.rs\"}");
            let reasoning_item = response
                .reasoning_items()
                .next()
                .expect("reasoning sibling preserved");
            let kigi_sampling_types::rs::SummaryPart::SummaryText(t) = &reasoning_item.summary[0];
            assert_eq!(t.text, "let me think harder");
            // Choice-level usage + top-level cached_tokens both absorbed.
            let usage = response.usage.as_ref().expect("usage from choice");
            assert_eq!(usage.prompt_tokens, 100);
            assert_eq!(usage.completion_tokens, 20);
            assert_eq!(usage.cached_prompt_tokens, 60);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

// Request surface: bearer auth, kigi UA, kimi_compat body adaptations

type Captured = Arc<Mutex<Option<(HeaderMap, Value)>>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_carries_bearer_kigi_ua_and_kimi_dialect_body() {
    let captured: Captured = Arc::new(Mutex::new(None));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, body: Bytes| {
            let captured = Arc::clone(&captured_handler);
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                *captured.lock().unwrap() = Some((headers, body));
                let events = vec![chunk(
                    json!({ "role": "assistant", "content": "ok" }),
                    Some("stop"),
                    None,
                )];
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        test_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    // Multi-turn conversation exercising every request-side adaptation:
    // reasoning folded onto the assistant, an empty-content tool-call turn,
    // and an enum-only tool schema property.
    let request = ConversationRequest {
        items: vec![
            ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: Arc::<str>::from("read a.rs"),
                }],
                synthetic_reason: None,
                ..Default::default()
            }),
            ConversationItem::Reasoning(synthesized_reasoning_item("planning the read")),
            ConversationItem::Assistant(AssistantItem {
                content: Arc::<str>::from(""),
                tool_calls: vec![ToolCall {
                    id: Arc::<str>::from("call_9"),
                    name: "read_file".into(),
                    arguments: Arc::<str>::from("{\"path\":\"a.rs\"}"),
                }],
                model_id: Some("kimi-for-coding".into()),
                model_fingerprint: None,
                reasoning_effort: None,
            }),
            ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: "call_9".into(),
                content: Arc::<str>::from("fn main() {}"),
                images: vec![],
            }),
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            parameters: json!({
                "type": "object",
                "properties": {
                    // Enum-only property: Moonshot 400s without a `type`.
                    "mode": { "enum": ["full", "head"] },
                    "path": { "type": "string" }
                }
            }),
        }],
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    };
    handle.submit(RequestId::from("req-wire"), request);
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(30)).await;
    server.shutdown();
    assert!(matches!(
        events.last().unwrap(),
        SamplingEvent::Completed { .. }
    ));

    let (headers, body) = captured.lock().unwrap().take().expect("request captured");

    // -- Auth: plain bearer, nothing else (PRD F3).
    assert_eq!(
        headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer test-kimi-key"
    );
    for gone in [
        "x-xai-token-auth",
        "x-authenticateresponse",
        "x-kigi-conv-id",
        "x-kigi-req-id",
        "x-kigi-model-override",
        "x-kigi-session-id",
        "x-kigi-agent-id",
        "x-kigi-client-identifier",
        "x-kigi-client-version",
        "x-kigi-deployment-id",
        "x-kigi-user-id",
        "x-kigi-client-mode",
    ] {
        assert!(
            headers.get(gone).is_none(),
            "xAI proxy marker header must not be sent: {gone}"
        );
    }

    // -- User-Agent: kigi/{version} (os; arch).
    let ua = headers.get("user-agent").unwrap().to_str().unwrap();
    let expected_prefix = format!("kigi/{}", kigi_version::VERSION);
    assert!(
        ua.starts_with(&expected_prefix),
        "UA must start with {expected_prefix}, got {ua}"
    );

    // -- Streaming fields exactly as kimi-cli sends them (kimi.py:174-181).
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));

    // -- Thinking mapping (kimi.py:214-223 + live think_efforts wire):
    //    effort → thinking {type, effort}, no reasoning_effort on the wire.
    assert_eq!(
        body["thinking"],
        json!({ "type": "enabled", "effort": "high" })
    );
    assert_eq!(body.get("reasoning_effort"), None);

    // -- Message adaptations.
    let messages = body["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn present");
    assert_eq!(
        assistant.get("content"),
        None,
        "empty tool-call content dropped (kimi.py:339-350)"
    );
    assert_eq!(assistant.get("model_id"), None, "kigi extension dropped");
    assert_eq!(
        assistant["reasoning_content"],
        json!("planning the read"),
        "reasoning folded onto the assistant turn (kimi.py:351-352)"
    );
    assert_eq!(assistant["tool_calls"][0]["id"], json!("call_9"));

    // -- Tool schema normalization (kosong jsonschema.py:88-142).
    let props = &body["tools"][0]["function"]["parameters"]["properties"];
    assert_eq!(props["mode"]["type"], json!("string"));
    assert_eq!(props["path"]["type"], json!("string"));
}

// 429 with Retry-After

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_honors_retry_after_then_succeeds() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // Moonshot-shaped 429 body + standard Retry-After.
                    let mut headers = HeaderMap::new();
                    headers.insert("retry-after", "1".parse().unwrap());
                    Err::<Sse<_>, (StatusCode, HeaderMap, String)>((
                        StatusCode::TOO_MANY_REQUESTS,
                        headers,
                        json!({ "error": {
                            "message": "Your account is rate limited",
                            "type": "rate_limit_reached_error"
                        }})
                        .to_string(),
                    ))
                } else {
                    let events = vec![chunk(
                        json!({ "role": "assistant", "content": "after limit" }),
                        Some("stop"),
                        None,
                    )];
                    Ok(Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    )))
                }
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        test_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    let started = std::time::Instant::now();
    handle.submit(RequestId::from("req-ra"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(30)).await;
    let elapsed = started.elapsed();
    server.shutdown();

    // A Retrying event carried the classified rate-limit.
    assert!(events.iter().any(|e| matches!(
        e,
        SamplingEvent::Retrying { kind, .. }
            if *kind == kigi_sampler::SamplingErrorKind::RateLimited
    )));
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(
                response.assistant().unwrap().content.as_ref(),
                "after limit"
            );
        }
        other => panic!("expected Completed after Retry-After wait, got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2, "exactly one retry");
    // Retry-After: 1 replaces the ~2s jittered exponential backoff. The wait
    // must be at least the advertised second (and clearly less than the
    // exhaust-path 30s timeout).
    assert!(
        elapsed >= Duration::from_secs(1),
        "waited less than Retry-After: {elapsed:?}"
    );
}

// Mid-stream network drop → retry → recovery

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_stream_drop_recovers_via_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // dies mid-body (simulated network drop).
                    let events: Vec<Result<Event, std::io::Error>> = vec![
                        Ok(chunk(
                            json!({ "role": "assistant", "content": "partial" }),
                            None,
                            None,
                        )),
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "connection reset by peer",
                        )),
                    ];
                    Sse::new(stream::iter(events))
                } else {
                    let events: Vec<Result<Event, std::io::Error>> = vec![Ok(chunk(
                        json!({ "role": "assistant", "content": "recovered" }),
                        Some("stop"),
                        None,
                    ))];
                    Sse::new(stream::iter(events))
                }
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        test_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-drop"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(60)).await;
    server.shutdown();

    assert!(
        events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. })),
        "mid-stream drop must go through the retry loop"
    );
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            // The poisoned partial attempt is discarded; only the fresh
            // attempt's content survives.
            assert_eq!(response.assistant().unwrap().content.as_ref(), "recovered");
        }
        other => panic!("expected Completed after recovery, got {other:?}"),
    }
    assert!(
        counter.load(Ordering::SeqCst) >= 2,
        "server hit at least twice"
    );
}
