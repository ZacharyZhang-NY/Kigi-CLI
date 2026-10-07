use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use futures_util::StreamExt;
use prost::Message as _;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use kigi_sampler::{ApiBackend, SamplerConfig, SamplingClient};
use kigi_sampling_types::{ConversationRequest, devin};

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
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn devin_config(base_url: String, model: &str) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("tok-construction".to_string()),
        base_url,
        model: model.to_string(),
        api_backend: ApiBackend::Devin,
        ..Default::default()
    }
}

fn frame(payload: &[u8], flags: u8) -> Vec<u8> {
    let mut out = vec![flags];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Clone, Default)]
struct Captures {
    jwt_meta: Arc<Mutex<Option<devin::Metadata>>>,
    jwt_auth_header: Arc<Mutex<Option<String>>>,
    chat_request: Arc<Mutex<Option<devin::GetChatMessageRequest>>>,
    chat_auth_header: Arc<Mutex<Option<String>>>,
    chat_content_type: Arc<Mutex<Option<String>>>,
}

fn app(caps: Captures, chat_body: Vec<u8>, custom_jwt_response: Option<Vec<u8>>) -> Router {
    let jwt_caps = caps.clone();
    let jwt_body = custom_jwt_response.unwrap_or_else(|| {
        devin::GetUserJwtResponse {
            user_jwt: "jwt-fresh".to_string(),
            custom_api_server_url: String::new(),
        }
        .encode_to_vec()
    });
    let chat_caps = caps.clone();
    Router::new()
        .route(
            devin::GET_USER_JWT_PATH,
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let jwt_body = jwt_body.clone();
                let caps = jwt_caps.clone();
                async move {
                    *caps.jwt_auth_header.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let req = devin::GetUserJwtRequest::decode(&body[..])
                        .expect("decode GetUserJwtRequest");
                    *caps.jwt_meta.lock().unwrap() = req.metadata;
                    (StatusCode::OK, jwt_body)
                }
            }),
        )
        .route(
            devin::GET_CHAT_MESSAGE_PATH,
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let caps = chat_caps.clone();
                let chat_body = chat_body.clone();
                async move {
                    *caps.chat_auth_header.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    *caps.chat_content_type.lock().unwrap() = headers
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    assert_eq!(body[0], 0, "request frame must be flags=0");
                    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
                    let req = devin::GetChatMessageRequest::decode(&body[5..5 + len])
                        .expect("decode GetChatMessageRequest");
                    *caps.chat_request.lock().unwrap() = Some(req);
                    (StatusCode::OK, chat_body)
                }
            }),
        )
}

fn collect_stream() -> Vec<u8> {
    let mut wire = Vec::new();
    let thinking = devin::GetChatMessageResponse {
        delta_thinking: "think ".to_string(),
        delta_signature: "SIG".to_string(),
        delta_signature_type: "sealed".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&thinking, 0));
    let t1 = devin::GetChatMessageResponse {
        delta_text: "Hello, ".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&t1, 0));
    let tc1 = devin::GetChatMessageResponse {
        delta_tool_calls: vec![
            devin::ChatToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments_json: "{\"cmd\":".to_string(),
                ..Default::default()
            },
            devin::ChatToolCall {
                id: "call_2".to_string(),
                name: "read".to_string(),
                arguments_json: "{\"path\":".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&tc1, 0));
    let tc2 = devin::GetChatMessageResponse {
        delta_tool_calls: vec![
            devin::ChatToolCall {
                id: "call_1".to_string(),
                name: String::new(),
                arguments_json: "{\"cmd\":\"ls\"}".to_string(),
                ..Default::default()
            },
            devin::ChatToolCall {
                id: "call_2".to_string(),
                name: String::new(),
                arguments_json: "\"x\"}".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&tc2, 0));
    let text2 = devin::GetChatMessageResponse {
        delta_text: "done".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&text2, 0));
    let stop = devin::GetChatMessageResponse {
        stop_reason: devin::stop_reason::FUNCTION_CALL,
        usage: Some(devin::ModelUsageStats {
            input_tokens: 10,
            output_tokens: 3,
            cache_write_tokens: 2,
            cache_read_tokens: 5,
        }),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&stop, 0));
    wire.extend_from_slice(&frame(b"{}", devin::CONNECT_FLAG_END_STREAM));
    wire
}

fn conversation() -> ConversationRequest {
    ConversationRequest {
        items: vec![kigi_sampling_types::ConversationItem::user("hi")],
        model: Some("MODEL_SWE_17".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn devin_two_step_stream_collects_rich_response() {
    let caps = Captures::default();
    let server = MockServer::spawn(app(caps.clone(), collect_stream(), None)).await;
    let client =
        SamplingClient::new(devin_config(server.base_url(), "MODEL_SWE_17")).expect("client");

    let (raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    let (response, _metrics) = kigi_sampler::collect_response(kigi_sampler::stream_messages(
        raw,
        None,
        kigi_sampler::RequestId::random(),
        std::time::Duration::from_secs(30),
    ))
    .await
    .expect("collect");

    let jwt_meta = caps.jwt_meta.lock().unwrap().clone().expect("jwt metadata");
    assert_eq!(jwt_meta.ide_name, "devin-cli");
    assert_eq!(jwt_meta.ide_type, "chisel");
    assert_eq!(jwt_meta.ide_version, "3000.11.3");
    assert_eq!(jwt_meta.extension_name, "chisel");
    assert_eq!(jwt_meta.extension_version, "3000.11.3");
    assert!(jwt_meta.disable_telemetry);
    assert_eq!(jwt_meta.api_key, "devin-session-token$tok-construction");
    assert!(
        jwt_meta.user_jwt.is_empty(),
        "GetUserJwt carries no jwt yet"
    );
    assert!(caps.jwt_auth_header.lock().unwrap().is_none());
    assert!(caps.chat_auth_header.lock().unwrap().is_none());

    let chat = caps
        .chat_request
        .lock()
        .unwrap()
        .clone()
        .expect("chat request");
    let meta = chat.metadata.expect("chat metadata");
    assert_eq!(meta.api_key, "devin-session-token$tok-construction");
    assert_eq!(meta.user_jwt, "jwt-fresh");
    assert!(meta.disable_telemetry);
    assert_eq!(chat.chat_model_uid, "MODEL_SWE_17");
    assert_eq!(chat.request_type, 5);
    assert_eq!(chat.planner_mode, 1);
    assert_eq!(
        caps.chat_content_type.lock().unwrap().as_deref(),
        Some("application/connect+proto")
    );

    let assistant = response.assistant().expect("assistant item");
    assert_eq!(
        assistant.content.as_ref(),
        "Hello, \ndone",
        "text blocks join with newline"
    );
    assert_eq!(assistant.tool_calls.len(), 2);
    let bash = &assistant.tool_calls[0];
    assert_eq!(bash.name, "bash");
    assert_eq!(bash.arguments.as_ref(), "{\"cmd\":\"ls\"}");
    let read = &assistant.tool_calls[1];
    assert_eq!(read.name, "read");
    assert_eq!(read.arguments.as_ref(), "{\"path\":\"x\"}");

    let reasoning = response
        .items
        .iter()
        .find_map(|i| match i {
            kigi_sampling_types::ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
        .expect("reasoning item");
    let env =
        devin::unpack_devin_signature(reasoning.encrypted_content.as_deref().expect("envelope"))
            .expect("devin envelope");
    assert_eq!(env.model_uid, "MODEL_SWE_17");
    assert_eq!(env.signature, "SIG");
    assert_eq!(env.signature_type, "sealed");

    let usage = response.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 17);
    assert_eq!(usage.completion_tokens, 3);
    assert_eq!(usage.cached_prompt_tokens, 5);
    assert_eq!(usage.total_tokens, 20);

    server.shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn devin_error_trailer_is_not_success() {
    let caps = Captures::default();
    let mut wire = Vec::new();
    let text = devin::GetChatMessageResponse {
        delta_text: "partial".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&frame(&text, 0));
    wire.extend_from_slice(&frame(
        br#"{"error":{"code":"unauthenticated","message":"bad session"}}"#,
        devin::CONNECT_FLAG_END_STREAM,
    ));
    let server = MockServer::spawn(app(caps, wire, None)).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let (mut raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = raw.next().await {
        events.push(ev);
    }
    let err = events
        .iter()
        .find_map(|e| e.as_ref().err())
        .expect("stream yields an error");
    assert!(
        matches!(err, kigi_sampling_types::SamplingError::Auth(_)),
        "unauthenticated trailer must map to auth, got {err:?}"
    );
    assert!(
        !err.to_string().contains("bad session"),
        "raw trailer message must not surface: {err}"
    );
}

#[tokio::test]
async fn devin_missing_trailer_is_not_success() {
    let caps = Captures::default();
    let text = devin::GetChatMessageResponse {
        delta_text: "partial".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    let wire = frame(&text, 0);
    let server = MockServer::spawn(app(caps, wire, None)).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let (mut raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    let mut events = Vec::new();
    while let Some(ev) = raw.next().await {
        events.push(ev);
    }
    assert!(
        events.iter().any(|e| e.is_err()),
        "missing trailer must surface an error"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Ok(kigi_sampling_types::messages::MessageStreamEvent::MessageStop)
        )),
        "message_stop must never be synthesized without a trailer"
    );
}

#[tokio::test]
async fn devin_never_follows_redirects_with_secrets() {
    let app = Router::new().route(
        devin::GET_USER_JWT_PATH,
        post(|| async {
            (
                StatusCode::FOUND,
                [(axum::http::header::LOCATION, "http://off-host.invalid/")],
                Vec::new(),
            )
        }),
    );
    let server = MockServer::spawn(app).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let Err(err) = client.conversation_stream_messages(conversation()).await else {
        panic!("302 must fail, not redirect");
    };
    assert!(
        !err.to_string().contains("tok-construction"),
        "the session token must not surface in the error: {err}"
    );
}

#[tokio::test]
async fn devin_custom_auth_host_rejected() {
    let caps = Captures::default();
    let jwt = devin::GetUserJwtResponse {
        user_jwt: "jwt".to_string(),
        custom_api_server_url: "https://evil.example.com".to_string(),
    }
    .encode_to_vec();
    let server = MockServer::spawn(app(caps.clone(), Vec::new(), Some(jwt))).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let Err(err) = client.conversation_stream_messages(conversation()).await else {
        panic!("custom endpoint must fail closed");
    };
    assert!(err.to_string().contains("KIGI_DEVIN_BASE_URL"));
    assert!(caps.chat_request.lock().unwrap().is_none());
}

#[tokio::test]
async fn devin_live_bearer_beats_construction_key() {
    #[derive(Debug)]
    struct Resolver;
    impl kigi_sampler::BearerResolver for Resolver {
        fn current_bearer(&self) -> Option<String> {
            Some("tok-live".to_string())
        }
    }
    let caps = Captures::default();
    let server = MockServer::spawn(app(caps.clone(), collect_stream(), None)).await;
    let mut cfg = devin_config(server.base_url(), "MODEL_X");
    cfg.bearer_resolver = Some(std::sync::Arc::new(Resolver));
    let client = SamplingClient::new(cfg).expect("client");
    let (raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    let _ = kigi_sampler::collect_response(kigi_sampler::stream_messages(
        raw,
        None,
        kigi_sampler::RequestId::random(),
        std::time::Duration::from_secs(30),
    ))
    .await;
    let jwt_meta = caps.jwt_meta.lock().unwrap().clone().expect("jwt meta");
    assert_eq!(jwt_meta.api_key, "devin-session-token$tok-live");
    let chat = caps.chat_request.lock().unwrap().clone().expect("chat");
    assert_eq!(
        chat.metadata.unwrap().api_key,
        "devin-session-token$tok-live"
    );
}

#[tokio::test]
async fn devin_redirects_are_never_followed() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let hits = Arc::new(AtomicU32::new(0));
    let hits_clone = hits.clone();
    let app_b = Router::new().fallback(move || {
        let hits = hits_clone.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    });
    let server_b = MockServer::spawn(app_b).await;

    let redirect_to_b = format!("http://{}", server_b.addr);
    let loc = redirect_to_b.clone();
    let app = Router::new().route(
        devin::GET_USER_JWT_PATH,
        post(move || {
            let loc = loc.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, loc)],
                    Vec::new(),
                )
            }
        }),
    );
    let server_a = MockServer::spawn(app).await;
    let client = SamplingClient::new(devin_config(server_a.base_url(), "MODEL_X")).expect("client");
    let res = client.conversation_stream_messages(conversation()).await;
    assert!(res.is_err(), "307 must fail, not redirect");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "redirect target hit");

    let chat_hits_a = Arc::new(AtomicU32::new(0));
    let chat_hits_a2 = chat_hits_a.clone();
    let loc = redirect_to_b;
    let jwt_ok = devin::GetUserJwtResponse {
        user_jwt: "jwt".to_string(),
        custom_api_server_url: String::new(),
    }
    .encode_to_vec();
    let app = Router::new()
        .route(
            devin::GET_USER_JWT_PATH,
            post(move || {
                let jwt_ok = jwt_ok.clone();
                async move { (StatusCode::OK, jwt_ok) }
            }),
        )
        .route(
            devin::GET_CHAT_MESSAGE_PATH,
            post(move || {
                let loc = loc.clone();
                let hits = chat_hits_a2.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::PERMANENT_REDIRECT,
                        [(axum::http::header::LOCATION, loc)],
                        Vec::new(),
                    )
                }
            }),
        );
    let server_a2 = MockServer::spawn(app).await;
    let client =
        SamplingClient::new(devin_config(server_a2.base_url(), "MODEL_X")).expect("client");
    let res = client.conversation_stream_messages(conversation()).await;
    assert!(res.is_err(), "308 must fail, not redirect");
    assert_eq!(chat_hits_a.load(Ordering::SeqCst), 1, "chat rpc ran once");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "redirect target hit");
}

#[tokio::test]
async fn devin_empty_user_jwt_aborts_before_chat() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let chat_hits = Arc::new(AtomicU32::new(0));
    let chat_hits_clone = chat_hits.clone();
    let jwt = devin::GetUserJwtResponse {
        user_jwt: String::new(),
        custom_api_server_url: String::new(),
    }
    .encode_to_vec();
    let app = Router::new()
        .route(
            devin::GET_USER_JWT_PATH,
            post(move || {
                let jwt = jwt.clone();
                async move { (StatusCode::OK, jwt) }
            }),
        )
        .route(
            devin::GET_CHAT_MESSAGE_PATH,
            post(move || {
                let hits = chat_hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, Vec::new())
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let Err(err) = client.conversation_stream_messages(conversation()).await else {
        panic!("empty user_jwt must fail");
    };
    assert!(err.to_string().contains("empty user JWT"), "{err}");
    assert_eq!(chat_hits.load(Ordering::SeqCst), 0, "chat rpc must not run");
}

#[tokio::test]
async fn devin_attribution_uses_dispatched_token_snapshot() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct FlipResolver(Arc<AtomicBool>);
    impl kigi_sampler::BearerResolver for FlipResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(if self.0.load(Ordering::SeqCst) {
                "tok-live-NEW".to_string()
            } else {
                "tok-live-OLD".to_string()
            })
        }
    }
    #[derive(Debug, Default)]
    struct Rec(std::sync::Mutex<Vec<(kigi_sampler::SamplingConsumer, Option<String>)>>);
    impl kigi_sampler::Auth401AttributionCallback for Rec {
        fn record_401(&self, consumer: kigi_sampler::SamplingConsumer, prefix: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push((consumer, prefix.map(|s| s.to_string())));
        }
    }

    let flip = Arc::new(AtomicBool::new(false));
    let rec = Arc::new(Rec::default());
    let caps = Captures::default();
    let mut wire = Vec::new();
    wire.extend_from_slice(&frame(
        br#"{"error":{"code":"unauthenticated"}}"#,
        devin::CONNECT_FLAG_END_STREAM,
    ));
    let server = MockServer::spawn(app(caps.clone(), wire, None)).await;
    let mut cfg = devin_config(server.base_url(), "MODEL_X");
    cfg.bearer_resolver = Some(Arc::new(FlipResolver(flip.clone())));
    cfg.attribution_callback = Some(rec.clone());
    let client = SamplingClient::new(cfg).expect("client");

    let (mut raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    flip.store(true, Ordering::SeqCst);
    while raw.next().await.is_some() {}

    let calls = rec.0.lock().unwrap();
    assert!(!calls.is_empty(), "auth trailer must invoke attribution");
    assert!(
        calls
            .iter()
            .all(|(c, _)| *c == kigi_sampler::SamplingConsumer::DevinStream),
        "consumer must be devin_stream: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|(_, p)| p.as_deref() == Some(&"tok-live-OLD"[..12])),
        "attribution must carry the 12-char prefix of the SENT token, got {calls:?}"
    );
}

#[tokio::test]
async fn devin_malformed_trailer_and_post_trailer_bytes_fail() {
    use kigi_sampling_types::messages::MessageStreamEvent;
    let ok_trailer = frame(b"{}", devin::CONNECT_FLAG_END_STREAM);
    let data = devin::GetChatMessageResponse {
        delta_text: "x".to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    let data_frame = frame(&data, 0);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "trailer-null",
            frame(b"null", devin::CONNECT_FLAG_END_STREAM),
        ),
        (
            "trailer-array",
            frame(b"[]", devin::CONNECT_FLAG_END_STREAM),
        ),
        (
            "trailer-string",
            frame(b"\"str\"", devin::CONNECT_FLAG_END_STREAM),
        ),
        (
            "trailer-error-empty",
            frame(br#"{"error":{}}"#, devin::CONNECT_FLAG_END_STREAM),
        ),
        (
            "trailer-error-nonstring-code",
            frame(br#"{"error":{"code":123}}"#, devin::CONNECT_FLAG_END_STREAM),
        ),
        {
            let mut w = ok_trailer.clone();
            w.extend_from_slice(&data_frame);
            ("data-frame-after-trailer", w)
        },
        {
            let mut w = ok_trailer.clone();
            w.extend_from_slice(&ok_trailer);
            ("second-trailer", w)
        },
        {
            let mut w = ok_trailer;
            w.extend_from_slice(b"garbage-after-eof");
            ("trailing-garbage", w)
        },
    ];
    for (name, wire) in cases {
        let server = MockServer::spawn(app(Captures::default(), wire, None)).await;
        let client =
            SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
        let (mut raw, _meta) = client
            .conversation_stream_messages(conversation())
            .await
            .expect("stream");
        let mut saw_err = false;
        let mut saw_stop = false;
        let drain = async {
            while let Some(ev) = raw.next().await {
                match ev {
                    Err(_) => saw_err = true,
                    Ok(MessageStreamEvent::MessageStop) => saw_stop = true,
                    Ok(_) => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), drain)
            .await
            .expect("stream drain must not hang");
        assert!(saw_err, "{name}: stream must surface an error");
        assert!(!saw_stop, "{name}: no MessageStop on a broken stream");
    }

    let server = MockServer::spawn(app(Captures::default(), collect_stream(), None)).await;
    let client = SamplingClient::new(devin_config(server.base_url(), "MODEL_X")).expect("client");
    let response = client
        .conversation_collect(conversation())
        .await
        .expect("clean stream collects");
    assert!(!response.items.is_empty(), "clean stream yields items");
}

#[tokio::test]
async fn devin_401_status_attributes_the_dispatched_token() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct FlipResolver(Arc<AtomicBool>);
    impl kigi_sampler::BearerResolver for FlipResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(if self.0.load(Ordering::SeqCst) {
                "tok-live-NEW".to_string()
            } else {
                "tok-live-OLD".to_string()
            })
        }
    }
    #[derive(Debug, Default)]
    struct Rec(std::sync::Mutex<Vec<(kigi_sampler::SamplingConsumer, Option<String>)>>);
    impl kigi_sampler::Auth401AttributionCallback for Rec {
        fn record_401(&self, consumer: kigi_sampler::SamplingConsumer, prefix: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push((consumer, prefix.map(|s| s.to_string())));
        }
    }

    let flip = Arc::new(AtomicBool::new(false));
    let rec = Arc::new(Rec::default());
    let jwt_meta: Arc<Mutex<Option<devin::Metadata>>> = Arc::new(Mutex::new(None));
    let jwt_meta2 = jwt_meta.clone();
    let flip2 = flip.clone();
    let app = Router::new().route(
        devin::GET_USER_JWT_PATH,
        post(move |body: axum::body::Bytes| {
            let jwt_meta = jwt_meta2.clone();
            let flip = flip2.clone();
            async move {
                let req =
                    devin::GetUserJwtRequest::decode(&body[..]).expect("decode GetUserJwtRequest");
                *jwt_meta.lock().unwrap() = req.metadata;
                flip.store(true, Ordering::SeqCst);
                StatusCode::UNAUTHORIZED
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut cfg = devin_config(server.base_url(), "MODEL_X");
    cfg.bearer_resolver = Some(Arc::new(FlipResolver(flip)));
    cfg.attribution_callback = Some(rec.clone());
    let client = SamplingClient::new(cfg).expect("client");

    let Err(err) = client.conversation_stream_messages(conversation()).await else {
        panic!("401 must error");
    };
    assert!(
        matches!(err, kigi_sampling_types::SamplingError::Auth(_)),
        "401 must map to Auth: {err:?}"
    );

    let meta = jwt_meta.lock().unwrap();
    let meta = meta.as_ref().expect("jwt request captured");
    assert_eq!(meta.api_key, "devin-session-token$tok-live-OLD");
    let calls = rec.0.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|(c, p)| *c == kigi_sampler::SamplingConsumer::DevinStream
                && p.as_deref() == Some(&"tok-live-OLD"[..12])),
        "attribution must carry the sent token prefix: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|(_, p)| !p.as_deref().is_some_and(|s| s.len() > 12)),
        "only a prefix crosses the callback boundary: {calls:?}"
    );
}

#[derive(Clone, Default)]
struct FusionCaptures {
    paths: Arc<Mutex<Vec<String>>>,
    jwt_meta: Arc<Mutex<Option<devin::Metadata>>>,
    assign_request: Arc<Mutex<Option<devin::AssignModelRequest>>>,
    assign_auth_header: Arc<Mutex<Option<String>>>,
    assign_hits: Arc<Mutex<u32>>,
    chat_request: Arc<Mutex<Option<devin::GetChatMessageRequest>>>,
    chat_auth_header: Arc<Mutex<Option<String>>>,
    chat_hits: Arc<Mutex<u32>>,
}

fn fusion_app(caps: FusionCaptures, assign: (StatusCode, Vec<u8>), chat_body: Vec<u8>) -> Router {
    let jwt_caps = caps.clone();
    let assign_caps = caps.clone();
    let chat_caps = caps.clone();
    Router::new()
        .route(
            devin::GET_USER_JWT_PATH,
            post(move |body: axum::body::Bytes| {
                let caps = jwt_caps.clone();
                async move {
                    caps.paths.lock().unwrap().push("GetUserJwt".to_string());
                    let req = devin::GetUserJwtRequest::decode(&body[..])
                        .expect("decode GetUserJwtRequest");
                    *caps.jwt_meta.lock().unwrap() = req.metadata;
                    let resp = devin::GetUserJwtResponse {
                        user_jwt: "jwt-fresh".to_string(),
                        custom_api_server_url: String::new(),
                    };
                    (StatusCode::OK, resp.encode_to_vec())
                }
            }),
        )
        .route(
            devin::ASSIGN_MODEL_PATH,
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let caps = assign_caps.clone();
                let (status, resp_body) = assign.clone();
                async move {
                    caps.paths.lock().unwrap().push("AssignModel".to_string());
                    *caps.assign_hits.lock().unwrap() += 1;
                    *caps.assign_auth_header.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    *caps.assign_request.lock().unwrap() =
                        devin::AssignModelRequest::decode(&body[..]).ok();
                    (status, resp_body)
                }
            }),
        )
        .route(
            devin::GET_CHAT_MESSAGE_PATH,
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let caps = chat_caps.clone();
                let chat_body = chat_body.clone();
                async move {
                    caps.paths
                        .lock()
                        .unwrap()
                        .push("GetChatMessage".to_string());
                    *caps.chat_hits.lock().unwrap() += 1;
                    *caps.chat_auth_header.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
                    *caps.chat_request.lock().unwrap() =
                        devin::GetChatMessageRequest::decode(&body[5..5 + len]).ok();
                    (StatusCode::OK, chat_body)
                }
            }),
        )
}

fn fusion_conversation() -> ConversationRequest {
    ConversationRequest {
        items: vec![
            kigi_sampling_types::ConversationItem::System(kigi_sampling_types::SystemItem {
                content: std::sync::Arc::from("base system"),
            }),
            kigi_sampling_types::ConversationItem::user("fusion smoke"),
        ],
        model: Some("fusion-claude-test-medium-sidekick-swe-test-medium".to_string()),
        ..Default::default()
    }
}

fn fusion_assign_body(jwt: &str, uid: &str) -> Vec<u8> {
    devin::AssignModelResponse {
        assignment: Some(devin::ModelAssignment {
            assignment_jwt: jwt.to_string(),
            model_uid: uid.to_string(),
            harness_uids: vec![],
        }),
    }
    .encode_to_vec()
}

#[tokio::test]
async fn devin_fusion_assigns_then_chats_on_lead_uid() {
    let caps = FusionCaptures::default();
    let chat_body = {
        let text = devin::GetChatMessageResponse {
            delta_text: "FUSION_MAIN_OK".to_string(),
            stop_reason: 2,
            ..Default::default()
        }
        .encode_to_vec();
        let mut wire = Vec::new();
        wire.extend_from_slice(&frame(&text, 0));
        wire.extend_from_slice(&frame(b"{}", devin::CONNECT_FLAG_END_STREAM));
        wire
    };
    let server = MockServer::spawn(fusion_app(
        caps.clone(),
        (
            StatusCode::OK,
            fusion_assign_body("FAKE-ASSIGN-JWT", "claude-test-medium"),
        ),
        chat_body,
    ))
    .await;
    let client = SamplingClient::new(devin_config(
        server.base_url(),
        "fusion-claude-test-medium-sidekick-swe-test-medium",
    ))
    .expect("client");

    let (raw, _meta) = client
        .conversation_stream_messages(fusion_conversation())
        .await
        .expect("stream");
    let (response, _metrics) = kigi_sampler::collect_response(kigi_sampler::stream_messages(
        raw,
        None,
        kigi_sampler::RequestId::random(),
        std::time::Duration::from_secs(30),
    ))
    .await
    .expect("collect");

    assert_eq!(
        caps.paths.lock().unwrap().as_slice(),
        &["GetUserJwt", "AssignModel", "GetChatMessage"],
        "GetUserJwt -> AssignModel -> GetChatMessage"
    );
    let assign = caps
        .assign_request
        .lock()
        .unwrap()
        .clone()
        .expect("assign request");
    assert_eq!(
        assign.model_router_uid,
        "fusion-claude-test-medium-sidekick-swe-test-medium"
    );
    let assign_meta = assign.metadata.expect("assign metadata");
    assert_eq!(assign_meta.api_key, "devin-session-token$tok-construction");
    assert_eq!(assign_meta.user_jwt, "jwt-fresh");
    assert!(
        caps.assign_auth_header.lock().unwrap().is_none(),
        "session token stays in Metadata, never the Authorization header"
    );
    let assign_prompt = assign.chat_message_prompt.expect("user prompt echoed");
    assert_eq!(assign_prompt.prompt, "fusion smoke");

    let chat = caps
        .chat_request
        .lock()
        .unwrap()
        .clone()
        .expect("chat request");
    assert_eq!(
        chat.chat_model_uid, "claude-test-medium",
        "assigned lead uid"
    );
    assert_eq!(
        chat.model_assignment_jwt.as_deref(),
        Some("FAKE-ASSIGN-JWT"),
        "tag 26 assignment jwt"
    );
    assert_eq!(
        chat.cascade_id, assign.cascade_id,
        "chat and assign share the cascade id"
    );
    let expected_prompt = "base system\n\nNative Devin Fusion is active. Configured lead model: claude-test-medium. Default delegated model: swe-test-medium. Use Kigi's existing spawn_subagent tool for suitable delegated work; unpinned subagents use the paired model. Resume a returned subagent ID to continue its context. Explicit user model choices and existing permission, cancellation, and concurrency limits still apply. Do not claim delegation occurred unless a subagent was actually started and its result received.";
    assert_eq!(chat.prompt, expected_prompt, "exact whole prompt");
    let meta = chat.metadata.expect("chat metadata");
    assert_eq!(meta.api_key, "devin-session-token$tok-construction");
    assert!(caps.chat_auth_header.lock().unwrap().is_none());

    let assistant = response.assistant().expect("assistant");
    assert_eq!(assistant.content.as_ref(), "FUSION_MAIN_OK");
    server.shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn devin_fusion_assign_auth_failure_fails_before_chat() {
    #[derive(Debug, Default)]
    struct Rec(std::sync::Mutex<Vec<(kigi_sampler::SamplingConsumer, Option<String>)>>);
    impl kigi_sampler::Auth401AttributionCallback for Rec {
        fn record_401(&self, consumer: kigi_sampler::SamplingConsumer, prefix: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push((consumer, prefix.map(|s| s.to_string())));
        }
    }
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let caps = FusionCaptures::default();
        let rec = Arc::new(Rec::default());
        let server = MockServer::spawn(fusion_app(
            caps.clone(),
            (status, b"unauthorized".to_vec()),
            Vec::new(),
        ))
        .await;
        let mut cfg = devin_config(
            server.base_url(),
            "fusion-claude-test-medium-sidekick-swe-test-medium",
        );
        cfg.attribution_callback = Some(rec.clone());
        let client = SamplingClient::new(cfg).expect("client");
        let Err(err) = client
            .conversation_stream_messages(fusion_conversation())
            .await
        else {
            panic!("assign {status} must error");
        };
        assert!(
            matches!(err, kigi_sampling_types::SamplingError::Auth(_)),
            "{status} maps to Auth"
        );
        assert_eq!(
            *caps.chat_hits.lock().unwrap(),
            0,
            "no chat after assign {status}"
        );
        let calls = rec.0.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|(c, p)| *c == kigi_sampler::SamplingConsumer::DevinStream
                    && p.as_deref() == Some(&"tok-construction"[..12])),
            "attribution carries the fake token's prefix: {calls:?}"
        );
        server.shutdown_tx.send(()).ok();
    }
}

#[tokio::test]
async fn devin_fusion_missing_or_echoed_assignment_fails_before_chat() {
    for body in [
        devin::AssignModelResponse { assignment: None }.encode_to_vec(),
        fusion_assign_body("", "claude-test-medium"),
        fusion_assign_body("   ", "claude-test-medium"),
        fusion_assign_body("FAKE-ASSIGN-JWT", ""),
        fusion_assign_body("FAKE-ASSIGN-JWT", "   "),
        fusion_assign_body(
            "FAKE-ASSIGN-JWT",
            "fusion-claude-test-medium-sidekick-swe-test-medium",
        ),
        fusion_assign_body("FAKE-ASSIGN-JWT", "fusion-other-lead-sidekick-other-helper"),
        fusion_assign_body("FAKE-ASSIGN-JWT", "fusion-unpaired"),
    ] {
        let caps = FusionCaptures::default();
        let server =
            MockServer::spawn(fusion_app(caps.clone(), (StatusCode::OK, body), Vec::new())).await;
        let client = SamplingClient::new(devin_config(
            server.base_url(),
            "fusion-claude-test-medium-sidekick-swe-test-medium",
        ))
        .expect("client");
        let Err(err) = client
            .conversation_stream_messages(fusion_conversation())
            .await
        else {
            panic!("bad assignment must error");
        };
        assert!(matches!(
            err,
            kigi_sampling_types::SamplingError::Api { .. }
                | kigi_sampling_types::SamplingError::Auth(_)
        ));
        assert_eq!(*caps.chat_hits.lock().unwrap(), 0);
        server.shutdown_tx.send(()).ok();
    }
}

#[tokio::test]
async fn devin_fusion_assign_redirect_is_never_followed() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let decoy_hits = Arc::new(AtomicU32::new(0));
    let decoy_hits2 = decoy_hits.clone();
    let decoy = Router::new().route(
        "/",
        post(move || {
            let hits = decoy_hits2.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (StatusCode::OK, "decoy reached")
            }
        }),
    );
    let decoy_server = MockServer::spawn(decoy).await;
    let decoy_url = decoy_server.base_url();
    let chat_hits = Arc::new(AtomicU32::new(0));
    let chat_hits2 = chat_hits.clone();
    let app = Router::new()
        .route(
            devin::GET_USER_JWT_PATH,
            post(move |body: axum::body::Bytes| async move {
                let req = devin::GetUserJwtRequest::decode(&body[..]).expect("decode jwt");
                let _ = req;
                let resp = devin::GetUserJwtResponse {
                    user_jwt: "jwt-fresh".to_string(),
                    custom_api_server_url: String::new(),
                };
                (StatusCode::OK, resp.encode_to_vec())
            }),
        )
        .route(
            devin::ASSIGN_MODEL_PATH,
            post(move || {
                let loc = format!("{}/", decoy_url);
                async move { (StatusCode::FOUND, [(axum::http::header::LOCATION, loc)]) }
            }),
        )
        .route(
            devin::GET_CHAT_MESSAGE_PATH,
            post(move || {
                let hits = chat_hits2.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, Vec::<u8>::new())
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let client = SamplingClient::new(devin_config(
        server.base_url(),
        "fusion-claude-test-medium-sidekick-swe-test-medium",
    ))
    .expect("client");
    let Err(err) = client
        .conversation_stream_messages(fusion_conversation())
        .await
    else {
        panic!("assign redirect must error");
    };
    assert!(matches!(
        err,
        kigi_sampling_types::SamplingError::Api { .. }
    ));
    assert_eq!(
        decoy_hits.load(Ordering::SeqCst),
        0,
        "redirect target never fetched"
    );
    assert_eq!(chat_hits.load(Ordering::SeqCst), 0, "no chat after 302");
    server.shutdown_tx.send(()).ok();
    decoy_server.shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn devin_non_fusion_model_never_calls_assign_model() {
    let caps = FusionCaptures::default();
    let server = MockServer::spawn(fusion_app(
        caps.clone(),
        (StatusCode::OK, b"unused".to_vec()),
        collect_stream(),
    ))
    .await;
    let client =
        SamplingClient::new(devin_config(server.base_url(), "MODEL_SWE_17")).expect("client");
    let (raw, _meta) = client
        .conversation_stream_messages(conversation())
        .await
        .expect("stream");
    let _ = kigi_sampler::collect_response(kigi_sampler::stream_messages(
        raw,
        None,
        kigi_sampler::RequestId::random(),
        std::time::Duration::from_secs(30),
    ))
    .await
    .expect("collect");
    assert_eq!(
        *caps.assign_hits.lock().unwrap(),
        0,
        "ordinary models skip AssignModel"
    );
    let chat = caps
        .chat_request
        .lock()
        .unwrap()
        .clone()
        .expect("chat request");
    assert!(chat.model_assignment_jwt.is_none());
    assert!(!chat.prompt.contains("Fusion"));
    server.shutdown_tx.send(()).ok();
}
