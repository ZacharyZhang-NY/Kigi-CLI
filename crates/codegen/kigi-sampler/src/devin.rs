use futures_util::{StreamExt, stream::BoxStream};
use kigi_sampling_types::Result;
use kigi_sampling_types::devin;
use kigi_sampling_types::error::ResponseModelMetadata;
use kigi_sampling_types::messages;
use kigi_sampling_types::{ConversationRequest, SamplingError};

use crate::client::SamplingClient;

const DEVIN_FUSION_PROMPT: &str = "Native Devin Fusion is active. Configured lead model: {lead}. \
     Default delegated model: {sidekick}. Use Kigi's existing spawn_subagent tool for suitable \
     delegated work; unpinned subagents use the paired model. Resume a returned subagent ID to \
     continue its context. Explicit user model choices and existing permission, cancellation, and \
     concurrency limits still apply. Do not claim delegation occurred unless a subagent was \
     actually started and its result received.";

/// Bound on the wait for `GetChatMessage` response headers, matching the
/// sampler's default stream idle window. Never a total request timeout: the
/// body must stay free to stream for as long as chunks keep arriving.
const DEVIN_CHAT_HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn devin_http_client() -> Result<reqwest::Client> {
    static CELL: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CELL.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(4)
        .connect_timeout(std::time::Duration::from_secs(10))
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(5))
        .http2_keep_alive_while_idle(true)
        .build()
        .map_err(|_| {
            SamplingError::InvalidConfiguration("failed to build devin Connect HTTP client")
        })?;
    let _ = CELL.set(client.clone());
    Ok(client)
}

impl SamplingClient {
    pub(crate) async fn conversation_stream_devin(
        &self,
        request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        let session_token = self.devin_bearer().ok_or_else(|| {
            SamplingError::Auth(
                "Devin platform has no session token — run `kigi login` for devin".to_string(),
            )
        })?;
        let client = devin_http_client()?;
        let api_key_wire = devin::normalize_devin_session_token(&session_token);
        let base = self.devin_base_url();
        let chat_model_uid = request
            .model
            .clone()
            .unwrap_or_else(|| self.devin_default_model().to_string());

        let session_id = uuid::Uuid::new_v4().to_string();
        let request_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();

        let jwt_url = format!("{}{}", base.trim_end_matches('/'), devin::GET_USER_JWT_PATH);
        let jwt_body = devin::build_get_user_jwt_request(&api_key_wire, request_id, &session_id);
        let resp = client
            .post(&jwt_url)
            .timeout(std::time::Duration::from_secs(30))
            .header(reqwest::header::CONTENT_TYPE, devin::PROTO_CONTENT_TYPE)
            .header(
                devin::CONNECT_PROTOCOL_VERSION_HEADER,
                devin::CONNECT_PROTOCOL_VERSION,
            )
            .header(reqwest::header::ACCEPT, devin::PROTO_CONTENT_TYPE)
            .body(jwt_body)
            .send()
            .await
            .map_err(SamplingError::Http)?;
        let resp = self.check_devin_status(resp, &session_token).await?;
        let body = bounded_body(resp, devin::MAX_DEVIN_UNARY_PAYLOAD).await?;
        let jwt_resp: devin::GetUserJwtResponse =
            devin::decode_unary(&body).map_err(devin::DevinWireError::into_sampling_error)?;
        let custom = jwt_resp.custom_api_server_url.trim();
        if !custom.is_empty() && !custom_eq_base(custom, base) {
            return Err(SamplingError::Api {
                status: reqwest::StatusCode::BAD_GATEWAY,
                message:
                    "Devin account advertises a custom API endpoint — set KIGI_DEVIN_BASE_URL \
                     to that endpoint explicitly to route the session token there"
                        .to_string(),
                model_metadata: None,
                retry_after_secs: None,
            });
        }
        let user_jwt = jwt_resp.user_jwt;
        if user_jwt.trim().is_empty() {
            return Err(SamplingError::Api {
                status: reqwest::StatusCode::BAD_GATEWAY,
                message: "Devin GetUserJwt minted an empty user JWT".to_string(),
                model_metadata: None,
                retry_after_secs: None,
            });
        }

        let ids = devin::DevinRequestIds {
            cascade_id: request
                .x_kigi_conv_id
                .clone()
                .or_else(|| request.x_kigi_session_id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            execution_id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            request_id,
        };

        let fusion_pair = devin::fusion_model_uids(&chat_model_uid);
        let mut resolved_uid = chat_model_uid.clone();
        let mut assignment_jwt: Option<String> = None;
        if fusion_pair.is_some() {
            let assign_body = devin::build_devin_assign_model_request(
                &request,
                &api_key_wire,
                &user_jwt,
                &chat_model_uid,
                &ids,
            )
            .encode_to_vec();
            let assign_url = format!("{}{}", base.trim_end_matches('/'), devin::ASSIGN_MODEL_PATH);
            let resp = client
                .post(&assign_url)
                .timeout(std::time::Duration::from_secs(30))
                .header(reqwest::header::CONTENT_TYPE, devin::PROTO_CONTENT_TYPE)
                .header(
                    devin::CONNECT_PROTOCOL_VERSION_HEADER,
                    devin::CONNECT_PROTOCOL_VERSION,
                )
                .header(reqwest::header::ACCEPT, devin::PROTO_CONTENT_TYPE)
                .body(assign_body)
                .send()
                .await
                .map_err(SamplingError::Http)?;
            let resp = self.check_devin_status(resp, &session_token).await?;
            let body = bounded_body(resp, devin::MAX_DEVIN_UNARY_PAYLOAD).await?;
            let decoded: devin::AssignModelResponse =
                devin::decode_unary(&body).map_err(devin::DevinWireError::into_sampling_error)?;
            let assignment = decoded.assignment.ok_or_else(|| SamplingError::Api {
                status: reqwest::StatusCode::BAD_GATEWAY,
                message: "Devin AssignModel returned no assignment".to_string(),
                model_metadata: None,
                retry_after_secs: None,
            })?;
            let assigned_uid = assignment.model_uid.trim().to_string();
            if assignment.assignment_jwt.trim().is_empty()
                || assigned_uid.is_empty()
                || assigned_uid.starts_with("fusion-")
            {
                return Err(SamplingError::Api {
                    status: reqwest::StatusCode::BAD_GATEWAY,
                    message: "Devin AssignModel returned an unusable assignment".to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                });
            }
            resolved_uid = assigned_uid;
            assignment_jwt = Some(assignment.assignment_jwt);
        }

        let mut wire = devin::build_devin_chat_request(
            &request,
            &api_key_wire,
            &user_jwt,
            &resolved_uid,
            &ids,
        )
        .map_err(devin::DevinWireError::into_sampling_error)?;
        wire.model_assignment_jwt = assignment_jwt;
        if let Some((lead_uid, helper_uid)) = fusion_pair {
            let fusion_line = DEVIN_FUSION_PROMPT
                .replace("{lead}", lead_uid)
                .replace("{sidekick}", helper_uid);
            if wire.prompt.is_empty() {
                wire.prompt = fusion_line;
            } else {
                wire.prompt.push_str("\n\n");
                wire.prompt.push_str(&fusion_line);
            }
        }
        use prost::Message as _;
        let body = devin::frame_connect_message(&wire.encode_to_vec(), false);
        let chat_url = format!(
            "{}{}",
            base.trim_end_matches('/'),
            devin::GET_CHAT_MESSAGE_PATH
        );
        let send = client
            .post(&chat_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                devin::CONNECT_PROTO_CONTENT_TYPE,
            )
            .header(
                devin::CONNECT_PROTOCOL_VERSION_HEADER,
                devin::CONNECT_PROTOCOL_VERSION,
            )
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header("connect-accept-encoding", "gzip")
            .body(body)
            .send();
        let resp = tokio::time::timeout(DEVIN_CHAT_HEADER_TIMEOUT, send)
            .await
            .map_err(|_| SamplingError::IdleTimeout {
                elapsed_secs: DEVIN_CHAT_HEADER_TIMEOUT.as_secs(),
            })?
            .map_err(SamplingError::Http)?;
        let resp = self.check_devin_status(resp, &session_token).await?;

        let translator = devin::DevinEventTranslator::new(resolved_uid);
        let attribution_client = self.clone();
        let stream = decode_connect_stream(resp, translator, move |status| {
            if status == 401 || status == 403 {
                attribution_client.record_401_attribution_for_token(
                    crate::attribution::SamplingConsumer::DevinStream,
                    &session_token,
                );
            }
        });
        Ok((Box::pin(stream), None))
    }

    async fn check_devin_status(
        &self,
        resp: reqwest::Response,
        sent_token: &str,
    ) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let retry_after = super::client::extract_retry_after(resp.headers());
        drop(resp);
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            self.record_401_attribution_for_token(
                crate::attribution::SamplingConsumer::DevinStream,
                sent_token,
            );
            return Err(SamplingError::Auth(format!(
                "Devin session rejected (HTTP {status}) — sign in again with `kigi login`"
            )));
        }
        Err(SamplingError::Api {
            status,
            message: format!("Devin request failed (HTTP {status})"),
            model_metadata: None,
            retry_after_secs: retry_after,
        })
    }
}

fn custom_eq_base(custom: &str, base: &str) -> bool {
    let (Ok(c), Ok(b)) = (url::Url::parse(custom), url::Url::parse(base)) else {
        return false;
    };
    c.scheme() == b.scheme()
        && c.host_str() == b.host_str()
        && c.port_or_known_default() == b.port_or_known_default()
}

async fn bounded_body(resp: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    let mut body = resp.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(SamplingError::Http)?;
        if out.len() + chunk.len() > cap {
            return Err(SamplingError::EventStreamError(
                "devin unary response exceeds the size cap".to_string(),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn decode_connect_stream(
    resp: reqwest::Response,
    mut translator: devin::DevinEventTranslator,
    on_auth_trailer: impl Fn(u16) + Send,
) -> impl futures_util::Stream<Item = Result<messages::MessageStreamEvent>> + Send {
    use prost::Message as _;
    async_stream::try_stream! {
        yield translator.message_start();
        let mut decoder = devin::ConnectDecoder::new();
        let mut saw_trailer = false;
        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(SamplingError::Http)?;
            if saw_trailer {
                if !chunk.is_empty() {
                    Err(devin::DevinWireError::TruncatedStream.into_sampling_error())?;
                }
                continue;
            }
            let frames = decoder
                .push(&chunk)
                .map_err(devin::DevinWireError::into_sampling_error)?;
            for frame in frames {
                if saw_trailer {
                    Err(devin::DevinWireError::TruncatedStream.into_sampling_error())?;
                }
                match frame {
                    devin::ConnectFrame::Data(payload) => {
                        let response = devin::GetChatMessageResponse::decode(&payload[..])
                            .map_err(|_| {
                                devin::DevinWireError::MalformedProtobuf.into_sampling_error()
                            })?;
                        for event in translator
                            .push_response(&response)
                            .map_err(devin::DevinWireError::into_sampling_error)?
                        {
                            yield event;
                        }
                    }
                    devin::ConnectFrame::EndStream(payload) => {
                        if let Some(err) = devin::parse_connect_trailer(&payload)
                            .map_err(devin::DevinWireError::into_sampling_error)?
                        {
                            let wire = err.into_wire_error();
                            if let devin::DevinWireError::Trailer { status, .. } = &wire {
                                on_auth_trailer(*status);
                            }
                            Err(wire.into_sampling_error())?;
                        }
                        saw_trailer = true;
                    }
                }
            }
        }
        decoder
            .finish()
            .map_err(devin::DevinWireError::into_sampling_error)?;
        if !saw_trailer {
            Err(devin::DevinWireError::MissingTrailer.into_sampling_error())?;
        }
        for event in translator
            .finish_success()
            .map_err(devin::DevinWireError::into_sampling_error)?
        {
            yield event;
        }
    }
}
