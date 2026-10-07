use prost::Message as _;

use crate::error::SamplingError;
use crate::messages;
use crate::{ContentPart, ConversationItem, ConversationRequest, ConversationToolChoice};

pub const GET_USER_JWT_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
pub const GET_CLI_MODEL_CONFIGS_PATH: &str =
    "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
pub const GET_CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
pub const ASSIGN_MODEL_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";

pub const DEVIN_SESSION_TOKEN_PREFIX: &str = "devin-session-token$";

pub const CONNECT_PROTO_CONTENT_TYPE: &str = "application/connect+proto";
pub const PROTO_CONTENT_TYPE: &str = "application/proto";
pub const CONNECT_PROTOCOL_VERSION_HEADER: &str = "connect-protocol-version";
pub const CONNECT_PROTOCOL_VERSION: &str = "1";

pub const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
pub const CONNECT_FLAG_END_STREAM: u8 = 0x02;
pub const MAX_CONNECT_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_DEVIN_UNARY_PAYLOAD: usize = 16 * 1024 * 1024;

const REQUEST_TYPE_CASCADE: i32 = 5;
const PLANNER_MODE_DEFAULT: i32 = 1;

const DEVIN_CLI_IDE_NAME: &str = "devin-cli";
const DEVIN_CLI_IDE_TYPE: &str = "chisel";
const DEVIN_CLI_IDE_VERSION: &str = "3000.11.3";
const DEVIN_CLI_EXTENSION_NAME: &str = "chisel";
const DEVIN_CLI_EXTENSION_VERSION: &str = "3000.11.3";
const DEVIN_DISCOVERY_VERSION: &str = "0.0.0-dev";
const DEVIN_LOCALE: &str = "en";
/// `Metadata` tag 30 as the released CLI (3000.11.3) sends it on
/// `GetCliModelConfigs`. Without it the server withholds every router row —
/// the `fusion-…` pairs and `adaptive` — and answers the concrete models only.
/// The field's schema name is not known; the values are opaque.
const DEVIN_DISCOVERY_CAPABILITIES: [i32; 5] = [3, 4, 6, 7, 8];

const DEVIN_STOP_PATTERNS: [&str; 5] = [
    concat!("<", "|user|>"),
    concat!("<", "|bot|>"),
    concat!("<", "|context_request|>"),
    concat!("<", "|endoftext|>"),
    concat!("<", "|end_of_turn|>"),
];

pub const DEVIN_SIGNATURE_PREFIX: &str = "kigi-devin-v1:";

#[derive(Debug, thiserror::Error)]
pub enum DevinWireError {
    #[error("connect frame length {0} exceeds the {MAX_CONNECT_FRAME_PAYLOAD}-byte cap")]
    OversizeFrame(u32),
    #[error("unsupported connect frame flags {0:#04x}")]
    UnsupportedFlags(u8),
    #[error("gzip payload exceeds the {MAX_CONNECT_FRAME_PAYLOAD}-byte decompression cap")]
    DecompressionBomb,
    #[error("malformed gzip payload")]
    Gzip,
    #[error("malformed protobuf message")]
    MalformedProtobuf,
    #[error("malformed connect end-of-stream trailer")]
    MalformedTrailer,
    #[error("stream ended mid-frame")]
    TruncatedStream,
    #[error("stream ended without a connect end-of-stream trailer")]
    MissingTrailer,
    #[error("connect stream rejected: {code}")]
    Trailer { code: String, status: u16 },
    #[error("unsupported request element for the devin wire: {0}")]
    Unsupported(&'static str),
    #[error("invalid devin tool-call stream: {0}")]
    MalformedToolCall(&'static str),
    #[error("stop_reason=error reported by the devin stream")]
    ModelError,
}

impl DevinWireError {
    pub fn into_sampling_error(self) -> SamplingError {
        match self {
            Self::Trailer { code, status } if status == 401 || status == 403 => {
                SamplingError::Auth(format!("Devin stream rejected (HTTP {status}): {code}"))
            }
            Self::Trailer { code, status } => SamplingError::Api {
                status: reqwest::StatusCode::from_u16(status)
                    .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                message: format!("Devin stream rejected: {code}"),
                model_metadata: None,
                retry_after_secs: None,
            },
            e @ (Self::Unsupported(_) | Self::MalformedToolCall(_)) => {
                SamplingError::serialization_message(e.to_string())
            }
            Self::ModelError => SamplingError::Api {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                message: "Devin stream reported stop_reason=error".to_string(),
                model_metadata: None,
                retry_after_secs: None,
            },
            other => SamplingError::EventStreamError(other.to_string()),
        }
    }
}

fn connect_code_status(code: &str) -> Option<u16> {
    Some(match code {
        "unauthenticated" => 401,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "invalid_argument" | "failed_precondition" | "out_of_range" => 400,
        "internal" | "unknown" | "data_loss" => 500,
        "unavailable" => 503,
        "unimplemented" => 501,
        "not_found" => 404,
        "already_exists" | "aborted" => 409,
        "deadline_exceeded" => 504,
        "cancelled" => 499,
        _ => return None,
    })
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Metadata {
    #[prost(string, tag = "1")]
    pub ide_name: String,
    #[prost(string, tag = "2")]
    pub extension_version: String,
    #[prost(string, tag = "3")]
    pub api_key: String,
    #[prost(string, tag = "4")]
    pub locale: String,
    #[prost(string, tag = "5")]
    pub os: String,
    #[prost(bool, tag = "6")]
    pub disable_telemetry: bool,
    #[prost(string, tag = "7")]
    pub ide_version: String,
    #[prost(uint64, tag = "9")]
    pub request_id: u64,
    #[prost(string, tag = "10")]
    pub session_id: String,
    #[prost(string, tag = "12")]
    pub extension_name: String,
    #[prost(string, tag = "21")]
    pub user_jwt: String,
    #[prost(string, tag = "28")]
    pub ide_type: String,
    #[prost(int32, repeated, tag = "30")]
    pub client_capabilities: Vec<i32>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct GetUserJwtRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct GetUserJwtResponse {
    #[prost(string, tag = "1")]
    pub user_jwt: String,
    #[prost(string, tag = "2")]
    pub custom_api_server_url: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct GetCliModelConfigsRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct GetCliModelConfigsResponse {
    #[prost(message, repeated, tag = "1")]
    pub client_model_configs: Vec<ClientModelConfig>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ClientModelConfig {
    #[prost(string, tag = "1")]
    pub label: String,
    #[prost(bool, tag = "4")]
    pub disabled: bool,
    #[prost(bool, tag = "5")]
    pub supports_images: bool,
    #[prost(int32, tag = "18")]
    pub max_tokens: i32,
    #[prost(string, tag = "22")]
    pub model_uid: String,
    #[prost(message, optional, tag = "23")]
    pub model_info: Option<ModelInfo>,
    #[prost(string, optional, tag = "27")]
    pub description: Option<String>,
    #[prost(message, optional, tag = "30")]
    pub model_family_metadata: Option<ModelFamilyMetadata>,
    #[prost(bool, tag = "31")]
    pub is_default_model_in_family: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelInfo {
    #[prost(int32, tag = "4")]
    pub max_tokens: i32,
    #[prost(message, optional, tag = "6")]
    pub model_features: Option<ModelFeatures>,
    #[prost(int32, tag = "13")]
    pub max_output_tokens: i32,
    #[prost(string, tag = "17")]
    pub model_uid: String,
    #[prost(string, tag = "23")]
    pub model_family_uid: String,
    #[prost(bool, tag = "25")]
    pub is_model_router: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelFamilyMetadataValue {
    #[prost(int32, tag = "1")]
    pub order: i32,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelFamilyMetadataEntry {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<ModelFamilyMetadataValue>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelFamilyMetadata {
    #[prost(string, tag = "1")]
    pub model_family_label: String,
    #[prost(message, repeated, tag = "2")]
    pub entries: Vec<ModelFamilyMetadataEntry>,
    #[prost(bool, tag = "3")]
    pub is_default_model_in_family: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelFeatures {
    #[prost(bool, tag = "11")]
    pub supports_images: bool,
    #[prost(bool, tag = "12")]
    pub supports_tool_calls: bool,
    #[prost(bool, tag = "15")]
    pub supports_thinking: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ImageData {
    #[prost(string, tag = "1")]
    pub base64_data: String,
    #[prost(string, tag = "2")]
    pub mime_type: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChatToolCall {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub arguments_json: String,
    #[prost(string, tag = "4")]
    pub invalid_json_str: String,
    #[prost(string, tag = "5")]
    pub invalid_json_err: String,
    #[prost(bool, tag = "6")]
    pub is_custom_tool_call: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChatMessagePrompt {
    #[prost(string, tag = "1")]
    pub message_id: String,
    #[prost(int32, tag = "2")]
    pub source: i32,
    #[prost(string, tag = "3")]
    pub prompt: String,
    #[prost(message, repeated, tag = "6")]
    pub tool_calls: Vec<ChatToolCall>,
    #[prost(string, tag = "7")]
    pub tool_call_id: String,
    #[prost(message, repeated, tag = "10")]
    pub images: Vec<ImageData>,
    #[prost(string, tag = "11")]
    pub thinking: String,
    #[prost(string, tag = "12")]
    pub signature: String,
    #[prost(string, tag = "18")]
    pub signature_type: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChatToolDefinition {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub json_schema_string: String,
    #[prost(bool, tag = "12")]
    pub strict: bool,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChatToolChoice {
    #[prost(oneof = "chat_tool_choice::Choice", tags = "1, 2")]
    pub choice: Option<chat_tool_choice::Choice>,
}

pub mod chat_tool_choice {
    #[derive(Clone, PartialEq, Eq, prost::Oneof)]
    pub enum Choice {
        #[prost(string, tag = "1")]
        OptionName(String),
        #[prost(string, tag = "2")]
        ToolName(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CompletionConfiguration {
    #[prost(uint64, tag = "1")]
    pub num_completions: u64,
    #[prost(uint64, tag = "2")]
    pub max_tokens: u64,
    #[prost(uint64, tag = "3")]
    pub max_newlines: u64,
    #[prost(double, tag = "5")]
    pub temperature: f64,
    #[prost(double, tag = "6")]
    pub first_temperature: f64,
    #[prost(uint64, tag = "7")]
    pub top_k: u64,
    #[prost(double, tag = "8")]
    pub top_p: f64,
    #[prost(string, repeated, tag = "9")]
    pub stop_patterns: Vec<String>,
    #[prost(double, tag = "11")]
    pub fim_eot_prob_threshold: f64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetChatMessageRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(string, tag = "2")]
    pub prompt: String,
    #[prost(message, repeated, tag = "3")]
    pub chat_message_prompts: Vec<ChatMessagePrompt>,
    #[prost(int32, tag = "7")]
    pub request_type: i32,
    #[prost(message, optional, tag = "8")]
    pub configuration: Option<CompletionConfiguration>,
    #[prost(message, repeated, tag = "10")]
    pub tools: Vec<ChatToolDefinition>,
    #[prost(bool, tag = "11")]
    pub disable_parallel_tool_calls: bool,
    #[prost(message, optional, tag = "12")]
    pub tool_choice: Option<ChatToolChoice>,
    #[prost(string, tag = "16")]
    pub cascade_id: String,
    #[prost(int32, tag = "20")]
    pub planner_mode: i32,
    #[prost(string, tag = "21")]
    pub chat_model_uid: String,
    #[prost(string, tag = "22")]
    pub execution_id: String,
    #[prost(string, optional, tag = "26")]
    pub model_assignment_jwt: Option<String>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct AssignModelRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(string, tag = "2")]
    pub model_router_uid: String,
    #[prost(string, tag = "3")]
    pub cascade_id: String,
    #[prost(message, optional, tag = "5")]
    pub chat_message_prompt: Option<ChatMessagePrompt>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelAssignment {
    #[prost(string, tag = "1")]
    pub assignment_jwt: String,
    #[prost(string, tag = "2")]
    pub model_uid: String,
    #[prost(string, repeated, tag = "3")]
    pub harness_uids: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct AssignModelResponse {
    #[prost(message, optional, tag = "1")]
    pub assignment: Option<ModelAssignment>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ModelUsageStats {
    #[prost(uint64, tag = "2")]
    pub input_tokens: u64,
    #[prost(uint64, tag = "3")]
    pub output_tokens: u64,
    #[prost(uint64, tag = "4")]
    pub cache_write_tokens: u64,
    #[prost(uint64, tag = "5")]
    pub cache_read_tokens: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetChatMessageResponse {
    #[prost(string, tag = "1")]
    pub message_id: String,
    #[prost(string, tag = "3")]
    pub delta_text: String,
    #[prost(int32, tag = "5")]
    pub stop_reason: i32,
    #[prost(message, repeated, tag = "6")]
    pub delta_tool_calls: Vec<ChatToolCall>,
    #[prost(message, optional, tag = "7")]
    pub usage: Option<ModelUsageStats>,
    #[prost(string, tag = "9")]
    pub delta_thinking: String,
    #[prost(string, tag = "10")]
    pub delta_signature: String,
    #[prost(bool, tag = "11")]
    pub thinking_redacted: bool,
    #[prost(string, tag = "21")]
    pub delta_signature_type: String,
    #[prost(string, optional, tag = "23")]
    pub actual_model_uid: Option<String>,
}

pub mod chat_message_source {
    pub const USER: i32 = 1;
    pub const ASSISTANT: i32 = 2;
    pub const TOOL: i32 = 4;
}

pub mod stop_reason {
    pub const INCOMPLETE: i32 = 1;
    pub const STOP_PATTERN: i32 = 2;
    pub const MAX_TOKENS: i32 = 3;
    pub const FUNCTION_CALL: i32 = 10;
    pub const CONTENT_FILTER: i32 = 11;
    pub const ERROR: i32 = 13;
}

pub fn devin_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

pub fn fusion_model_uids(uid: &str) -> Option<(&str, &str)> {
    let rest = uid.strip_prefix("fusion-")?;
    let (lead, sidekick) = rest.split_once("-sidekick-")?;
    if lead.is_empty()
        || sidekick.is_empty()
        || lead.starts_with("fusion-")
        || sidekick.starts_with("fusion-")
        || sidekick.contains("-sidekick-")
    {
        return None;
    }
    Some((lead, sidekick))
}

pub fn normalize_devin_session_token(token: &str) -> String {
    if token.starts_with(DEVIN_SESSION_TOKEN_PREFIX) {
        token.to_string()
    } else {
        format!("{DEVIN_SESSION_TOKEN_PREFIX}{token}")
    }
}

pub fn devin_cli_metadata(
    api_key_wire: &str,
    user_jwt: &str,
    request_id: u64,
    session_id: &str,
) -> Metadata {
    Metadata {
        ide_name: DEVIN_CLI_IDE_NAME.to_string(),
        ide_type: DEVIN_CLI_IDE_TYPE.to_string(),
        ide_version: DEVIN_CLI_IDE_VERSION.to_string(),
        extension_name: DEVIN_CLI_EXTENSION_NAME.to_string(),
        extension_version: DEVIN_CLI_EXTENSION_VERSION.to_string(),
        api_key: api_key_wire.to_string(),
        user_jwt: user_jwt.to_string(),
        locale: DEVIN_LOCALE.to_string(),
        os: devin_os().to_string(),
        disable_telemetry: true,
        request_id,
        session_id: session_id.to_string(),
        client_capabilities: Vec::new(),
    }
}

pub fn devin_discovery_metadata(api_key_wire: &str, request_id: u64, session_id: &str) -> Metadata {
    Metadata {
        ide_name: DEVIN_CLI_EXTENSION_NAME.to_string(),
        ide_version: DEVIN_DISCOVERY_VERSION.to_string(),
        extension_name: DEVIN_CLI_EXTENSION_NAME.to_string(),
        extension_version: DEVIN_DISCOVERY_VERSION.to_string(),
        api_key: api_key_wire.to_string(),
        locale: DEVIN_LOCALE.to_string(),
        os: devin_os().to_string(),
        disable_telemetry: true,
        request_id,
        session_id: session_id.to_string(),
        client_capabilities: DEVIN_DISCOVERY_CAPABILITIES.to_vec(),
        ..Default::default()
    }
}

pub fn build_get_user_jwt_request(
    api_key_wire: &str,
    request_id: u64,
    session_id: &str,
) -> Vec<u8> {
    GetUserJwtRequest {
        metadata: Some(devin_cli_metadata(api_key_wire, "", request_id, session_id)),
    }
    .encode_to_vec()
}

pub fn build_get_cli_model_configs_request(
    api_key_wire: &str,
    request_id: u64,
    session_id: &str,
) -> Vec<u8> {
    GetCliModelConfigsRequest {
        metadata: Some(devin_discovery_metadata(
            api_key_wire,
            request_id,
            session_id,
        )),
    }
    .encode_to_vec()
}

pub fn gunzip_bounded(bytes: &[u8], cap: usize) -> Result<Vec<u8>, DevinWireError> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut limited = decoder.by_ref().take(cap as u64 + 1);
    limited
        .read_to_end(&mut out)
        .map_err(|_| DevinWireError::Gzip)?;
    if out.len() > cap {
        return Err(DevinWireError::DecompressionBomb);
    }
    Ok(out)
}

pub fn decode_unary<T: prost::Message + Default>(bytes: &[u8]) -> Result<T, DevinWireError> {
    match T::decode(bytes) {
        Ok(msg) => Ok(msg),
        Err(raw_err) => {
            let inflated = gunzip_bounded(bytes, MAX_DEVIN_UNARY_PAYLOAD)
                .map_err(|_| DevinWireError::MalformedProtobuf)?;
            T::decode(&inflated[..]).map_err(|_| {
                let _ = raw_err;
                DevinWireError::MalformedProtobuf
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectFrame {
    Data(Vec<u8>),
    EndStream(Vec<u8>),
}

pub fn frame_connect_message(payload: &[u8], compressed: bool) -> Vec<u8> {
    let (flags, body) = if compressed {
        (CONNECT_FLAG_COMPRESSED, gzip_compress(payload))
    } else {
        (0u8, payload.to_vec())
    };
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(flags);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn gzip_compress(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(payload);
    encoder.finish().unwrap_or_default()
}

#[derive(Default)]
pub struct ConnectDecoder {
    pending: Vec<u8>,
}

fn parse_frame_header(header: &[u8]) -> Result<(u8, usize), DevinWireError> {
    let flags = header[0];
    if flags & !(CONNECT_FLAG_COMPRESSED | CONNECT_FLAG_END_STREAM) != 0 {
        return Err(DevinWireError::UnsupportedFlags(flags));
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_CONNECT_FRAME_PAYLOAD {
        return Err(DevinWireError::OversizeFrame(len as u32));
    }
    Ok((flags, len))
}

fn decode_frame(flags: u8, payload: &[u8]) -> Result<ConnectFrame, DevinWireError> {
    let payload = if flags & CONNECT_FLAG_COMPRESSED != 0 {
        gunzip_bounded(payload, MAX_CONNECT_FRAME_PAYLOAD)?
    } else {
        payload.to_vec()
    };
    Ok(if flags & CONNECT_FLAG_END_STREAM != 0 {
        ConnectFrame::EndStream(payload)
    } else {
        ConnectFrame::Data(payload)
    })
}

impl ConnectDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ConnectFrame>, DevinWireError> {
        let mut rest = chunk;
        let mut frames = Vec::new();
        while !rest.is_empty() {
            if self.pending.is_empty() && rest.len() >= 5 {
                let (flags, len) = parse_frame_header(&rest[..5])?;
                if rest.len() >= 5 + len {
                    frames.push(decode_frame(flags, &rest[5..5 + len])?);
                    rest = &rest[5 + len..];
                    continue;
                }
            }
            let need = if self.pending.len() < 5 {
                5 - self.pending.len()
            } else {
                let len = u32::from_be_bytes([
                    self.pending[1],
                    self.pending[2],
                    self.pending[3],
                    self.pending[4],
                ]) as usize;
                5 + len - self.pending.len()
            };
            let take = need.min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() >= 5 {
                let (flags, len) = parse_frame_header(&self.pending[..5])?;
                if self.pending.len() == 5 + len {
                    frames.push(decode_frame(flags, &self.pending[5..])?);
                    self.pending.clear();
                }
            }
        }
        Ok(frames)
    }

    pub fn finish(&self) -> Result<(), DevinWireError> {
        if !self.pending.is_empty() {
            return Err(DevinWireError::TruncatedStream);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTrailerError {
    pub code: String,
}

pub fn parse_connect_trailer(
    payload: &[u8],
) -> Result<Option<ConnectTrailerError>, DevinWireError> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| DevinWireError::MalformedTrailer)?;
    let Some(object) = value.as_object() else {
        return Err(DevinWireError::MalformedTrailer);
    };
    let Some(error) = object.get("error") else {
        return Ok(None);
    };
    if error.is_null() {
        return Ok(None);
    }
    let Some(code) = error
        .as_object()
        .and_then(|e| e.get("code"))
        .and_then(serde_json::Value::as_str)
        .filter(|c| !c.is_empty())
    else {
        return Err(DevinWireError::MalformedTrailer);
    };
    let code = if connect_code_status(code).is_some() {
        code.to_string()
    } else {
        "unrecognized".to_string()
    };
    Ok(Some(ConnectTrailerError { code }))
}

impl ConnectTrailerError {
    pub fn into_wire_error(self) -> DevinWireError {
        let status = connect_code_status(&self.code).unwrap_or(502);
        DevinWireError::Trailer {
            code: self.code,
            status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DevinSignatureEnvelope {
    pub model_uid: String,
    pub signature: String,
    #[serde(default)]
    pub signature_type: String,
}

pub fn pack_devin_signature(model_uid: &str, signature: &str, signature_type: &str) -> String {
    let envelope = DevinSignatureEnvelope {
        model_uid: model_uid.to_string(),
        signature: signature.to_string(),
        signature_type: signature_type.to_string(),
    };
    format!(
        "{DEVIN_SIGNATURE_PREFIX}{}",
        serde_json::to_string(&envelope).unwrap_or_default()
    )
}

pub fn unpack_devin_signature(stored: &str) -> Option<DevinSignatureEnvelope> {
    let json = stored.strip_prefix(DEVIN_SIGNATURE_PREFIX)?;
    serde_json::from_str(json).ok()
}

pub struct DevinRequestIds {
    pub cascade_id: String,
    pub execution_id: String,
    pub session_id: String,
    pub request_id: u64,
}

fn last_assistant_open_tool_loop(items: &[ConversationItem]) -> Option<usize> {
    let idx = items
        .iter()
        .rposition(|i| matches!(i, ConversationItem::Assistant(_)))?;
    let ConversationItem::Assistant(a) = &items[idx] else {
        return None;
    };
    if a.tool_calls.is_empty() {
        return None;
    }
    let tail = &items[idx + 1..];
    if tail.is_empty()
        || !tail
            .iter()
            .all(|i| matches!(i, ConversationItem::ToolResult(_)))
    {
        return None;
    }
    Some(idx)
}

fn content_parts_to_wire(content: &[ContentPart]) -> (String, Vec<ImageData>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text: t } => {
                if !text.is_empty() && !t.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentPart::Image { url } => {
                if let Some((media_type, data)) =
                    crate::conversation::parse_base64_image_data_uri(url)
                {
                    images.push(ImageData {
                        base64_data: data,
                        mime_type: media_type,
                    });
                } else {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("[unsupported image]");
                }
            }
        }
    }
    (text, images)
}

fn reasoning_text(item: &crate::rs::ReasoningItem) -> String {
    item.summary
        .iter()
        .map(|part| match part {
            crate::rs::SummaryPart::SummaryText(t) => t.text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn uid_is_gemini_routed(chat_model_uid: &str) -> bool {
    chat_model_uid.to_ascii_lowercase().contains("gemini")
}

fn normalize_schema_for_gemini(schema: &mut serde_json::Value) -> Result<(), DevinWireError> {
    let Some(obj) = schema.as_object_mut() else {
        return Ok(());
    };
    if let Some(ty) = obj.get_mut("type")
        && let Some(arr) = ty.as_array()
    {
        let has_null = arr.iter().any(|v| v.as_str() == Some("null"));
        if arr.is_empty() || !arr.iter().all(|v| v.is_string()) {
            return Err(DevinWireError::Unsupported(
                "JSON-schema type array malformed (gemini wire)",
            ));
        }
        let non_null: Vec<&str> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| *s != "null")
            .collect();
        match (non_null.len(), has_null) {
            (1, false) => *ty = serde_json::Value::String(non_null[0].to_string()),
            (1, true) => {
                *ty = serde_json::Value::String(non_null[0].to_string());
                obj.insert("nullable".into(), serde_json::Value::Bool(true));
            }
            (0, true) => {
                obj.remove("type");
                obj.insert("nullable".into(), serde_json::Value::Bool(true));
            }
            _ => {
                return Err(DevinWireError::Unsupported(
                    "JSON-schema type union beyond null+single (gemini wire)",
                ));
            }
        }
    }
    for key in ["properties", "$defs", "definitions"] {
        if let Some(serde_json::Value::Object(map)) = obj.get_mut(key) {
            for value in map.values_mut() {
                normalize_schema_for_gemini(value)?;
            }
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(value) = obj.get_mut(key) {
            normalize_schema_for_gemini(value)?;
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(serde_json::Value::Array(list)) = obj.get_mut(key) {
            for value in list.iter_mut() {
                normalize_schema_for_gemini(value)?;
            }
        }
    }
    Ok(())
}

pub fn build_devin_chat_request(
    req: &ConversationRequest,
    api_key_wire: &str,
    user_jwt: &str,
    chat_model_uid: &str,
    ids: &DevinRequestIds,
) -> Result<GetChatMessageRequest, DevinWireError> {
    if !req.hosted_tools.is_empty() {
        return Err(DevinWireError::Unsupported("hosted tools"));
    }
    if req.json_schema.is_some() {
        return Err(DevinWireError::Unsupported(
            "native json_schema response format",
        ));
    }

    let open_loop_idx = last_assistant_open_tool_loop(&req.items);

    let mut system_parts: Vec<String> = Vec::new();
    let mut prompts: Vec<ChatMessagePrompt> = Vec::new();
    let mut pending_envelope: Option<DevinSignatureEnvelope> = None;
    let mut pending_reasoning_text = String::new();
    let mut pending_assistant_prefix = String::new();
    let mut pending_tool_images: Vec<ImageData> = Vec::new();

    let flush_tool_images =
        |prompts: &mut Vec<ChatMessagePrompt>, pending: &mut Vec<ImageData>, index: usize| {
            if pending.is_empty() {
                return;
            }
            prompts.push(ChatMessagePrompt {
                message_id: format!("{}-{index}-tool-images", ids.cascade_id),
                source: chat_message_source::USER,
                prompt: "Attached image(s) from tool result:".to_string(),
                images: std::mem::take(pending),
                ..Default::default()
            });
        };

    for (index, item) in req.items.iter().enumerate() {
        match item {
            ConversationItem::System(s) => {
                system_parts.push(s.content.as_ref().to_owned());
            }
            ConversationItem::User(u) => {
                flush_tool_images(&mut prompts, &mut pending_tool_images, index);
                pending_envelope = None;
                pending_reasoning_text.clear();
                let (mut prompt, images) = content_parts_to_wire(&u.content);
                if prompt.is_empty() && images.is_empty() {
                    prompt = "[empty message]".to_string();
                }
                prompts.push(ChatMessagePrompt {
                    message_id: format!("{}-{index}", ids.cascade_id),
                    source: chat_message_source::USER,
                    prompt,
                    images,
                    ..Default::default()
                });
            }
            ConversationItem::Reasoning(r) => {
                if let Some(stored) = r.encrypted_content.as_deref()
                    && let Some(envelope) = unpack_devin_signature(stored)
                {
                    pending_envelope = Some(envelope);
                    let text = reasoning_text(r);
                    if !text.is_empty() {
                        pending_reasoning_text = text;
                    }
                }
            }
            ConversationItem::BackendToolCall(b) => {
                if !pending_assistant_prefix.is_empty() {
                    pending_assistant_prefix.push('\n');
                }
                pending_assistant_prefix.push_str(&b.text_summary());
            }
            ConversationItem::Assistant(a) => {
                flush_tool_images(&mut prompts, &mut pending_tool_images, index);
                let mut prompt_text = String::new();
                if !pending_assistant_prefix.is_empty() {
                    prompt_text.push_str(&pending_assistant_prefix);
                    pending_assistant_prefix.clear();
                }
                if !prompt_text.is_empty() && !a.content.is_empty() {
                    prompt_text.push('\n');
                }
                prompt_text.push_str(&a.content);

                let tool_calls: Vec<ChatToolCall> = a
                    .tool_calls
                    .iter()
                    .map(|tc| ChatToolCall {
                        id: tc.id.as_ref().to_owned(),
                        name: tc.name.clone(),
                        arguments_json: tc.arguments.as_ref().to_owned(),
                        ..Default::default()
                    })
                    .collect();

                let mut thinking = String::new();
                let mut signature = String::new();
                let mut signature_type = String::new();
                if Some(index) == open_loop_idx
                    && let Some(env) = pending_envelope.take()
                    && env.model_uid == chat_model_uid
                    && !env.signature.is_empty()
                {
                    thinking = pending_reasoning_text.clone();
                    signature = env.signature;
                    signature_type = env.signature_type;
                }
                pending_envelope = None;
                pending_reasoning_text.clear();

                if prompt_text.is_empty()
                    && tool_calls.is_empty()
                    && thinking.is_empty()
                    && signature.is_empty()
                {
                    continue;
                }
                prompts.push(ChatMessagePrompt {
                    message_id: format!("bot-{}-{index}", ids.cascade_id),
                    source: chat_message_source::ASSISTANT,
                    prompt: prompt_text,
                    tool_calls,
                    thinking,
                    signature,
                    signature_type,
                    ..Default::default()
                });
            }
            ConversationItem::ToolResult(t) => {
                pending_envelope = None;
                pending_reasoning_text.clear();
                for img in &t.images {
                    if let ContentPart::Image { url } = img
                        && let Some((media_type, data)) =
                            crate::conversation::parse_base64_image_data_uri(url)
                    {
                        pending_tool_images.push(ImageData {
                            base64_data: data,
                            mime_type: media_type,
                        });
                    }
                }
                prompts.push(ChatMessagePrompt {
                    message_id: format!("{}-{index}-tool", ids.cascade_id),
                    source: chat_message_source::TOOL,
                    prompt: t.content.as_ref().to_owned(),
                    tool_call_id: t.tool_call_id.clone(),
                    ..Default::default()
                });
            }
        }
    }
    flush_tool_images(&mut prompts, &mut pending_tool_images, req.items.len());

    let gemini = uid_is_gemini_routed(chat_model_uid);
    let tools: Vec<ChatToolDefinition> = req
        .tools
        .iter()
        .map(|t| {
            let mut schema = t.parameters.clone();
            if gemini {
                normalize_schema_for_gemini(&mut schema)?;
            }
            Ok(ChatToolDefinition {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                json_schema_string: serde_json::to_string(&schema)
                    .unwrap_or_else(|_| "{}".to_string()),
                strict: false,
            })
        })
        .collect::<Result<_, DevinWireError>>()?;

    let tool_choice = req.tool_choice.as_ref().map(|tc| ChatToolChoice {
        choice: Some(match tc {
            ConversationToolChoice::Auto => {
                chat_tool_choice::Choice::OptionName("auto".to_string())
            }
            ConversationToolChoice::None => {
                chat_tool_choice::Choice::OptionName("none".to_string())
            }
            ConversationToolChoice::Required => {
                chat_tool_choice::Choice::OptionName("required".to_string())
            }
            ConversationToolChoice::Function(name) => {
                chat_tool_choice::Choice::ToolName(name.clone())
            }
        }),
    });

    let temperature = req.temperature.map(|t| t as f64).unwrap_or(0.4);
    Ok(GetChatMessageRequest {
        metadata: Some(devin_cli_metadata(
            api_key_wire,
            user_jwt,
            ids.request_id,
            &ids.session_id,
        )),
        prompt: system_parts.join("\n\n"),
        chat_message_prompts: prompts,
        request_type: REQUEST_TYPE_CASCADE,
        configuration: Some(CompletionConfiguration {
            num_completions: 1,
            max_tokens: u64::from(req.max_output_tokens.unwrap_or(64_000)),
            max_newlines: 200,
            temperature,
            first_temperature: temperature,
            top_k: 50,
            top_p: req.top_p.unwrap_or(1.0) as f64,
            stop_patterns: DEVIN_STOP_PATTERNS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            fim_eot_prob_threshold: 1.0,
        }),
        tools,
        disable_parallel_tool_calls: false,
        tool_choice,
        cascade_id: ids.cascade_id.clone(),
        planner_mode: PLANNER_MODE_DEFAULT,
        chat_model_uid: chat_model_uid.to_string(),
        execution_id: ids.execution_id.clone(),
        model_assignment_jwt: None,
    })
}

pub fn build_devin_assign_model_request(
    req: &ConversationRequest,
    api_key_wire: &str,
    user_jwt: &str,
    model_router_uid: &str,
    ids: &DevinRequestIds,
) -> AssignModelRequest {
    let chat_message_prompt = req.items.iter().rev().find_map(|i| {
        let ConversationItem::User(u) = i else {
            return None;
        };
        if u.synthetic_reason.is_some() {
            return None;
        }
        let (prompt, images) = content_parts_to_wire(&u.content);
        Some(ChatMessagePrompt {
            source: chat_message_source::USER,
            prompt,
            images,
            ..Default::default()
        })
    });
    AssignModelRequest {
        metadata: Some(devin_cli_metadata(
            api_key_wire,
            user_jwt,
            ids.request_id,
            &ids.session_id,
        )),
        model_router_uid: model_router_uid.to_string(),
        cascade_id: ids.cascade_id.clone(),
        chat_message_prompt,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeafKind {
    Text,
    Thinking,
}

struct ToolBlock {
    id: String,
    name: String,
    args_acc: String,
}

pub struct DevinEventTranslator {
    model: String,
    next_index: u32,
    leaf: Option<(u32, LeafKind)>,
    tool_blocks: std::collections::BTreeMap<u32, ToolBlock>,
    tool_index_by_id: std::collections::HashMap<String, u32>,
    tool_order: Vec<u32>,
    active_tool_slot: Option<u32>,
    nonce: u128,
    thinking_signature: String,
    thinking_signature_type: String,
    thinking_redacted: bool,
    latest_stop: Option<i32>,
    usage: Option<ModelUsageStats>,
}

impl DevinEventTranslator {
    pub fn new(model: String) -> Self {
        Self {
            model,
            next_index: 0,
            leaf: None,
            tool_blocks: std::collections::BTreeMap::new(),
            tool_index_by_id: std::collections::HashMap::new(),
            tool_order: Vec::new(),
            active_tool_slot: None,
            nonce: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            thinking_signature: String::new(),
            thinking_signature_type: String::new(),
            thinking_redacted: false,
            latest_stop: None,
            usage: None,
        }
    }

    pub fn message_start(&self) -> messages::MessageStreamEvent {
        messages::MessageStreamEvent::MessageStart {
            message: messages::MessagesResponse {
                id: String::new(),
                r#type: "message".to_string(),
                role: "assistant".to_string(),
                content: Vec::new(),
                model: self.model.clone(),
                stop_reason: None,
                usage: messages::MessagesUsage::default(),
            },
        }
    }

    fn alloc_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn close_leaf(&mut self, events: &mut Vec<messages::MessageStreamEvent>) {
        let Some((index, kind)) = self.leaf.take() else {
            return;
        };
        if kind == LeafKind::Thinking
            && !self.thinking_redacted
            && !self.thinking_signature.is_empty()
        {
            events.push(messages::MessageStreamEvent::ContentBlockDelta {
                index,
                delta: messages::StreamDelta::SignatureDelta {
                    signature: pack_devin_signature(
                        &self.model,
                        &self.thinking_signature,
                        &self.thinking_signature_type,
                    ),
                },
            });
        }
        self.thinking_signature.clear();
        self.thinking_signature_type.clear();
        self.thinking_redacted = false;
        events.push(messages::MessageStreamEvent::ContentBlockStop { index });
    }

    fn open_leaf(&mut self, kind: LeafKind, events: &mut Vec<messages::MessageStreamEvent>) -> u32 {
        if let Some((index, open)) = self.leaf
            && open == kind
        {
            return index;
        }
        self.close_leaf(events);
        let index = self.alloc_index();
        let content_block = match kind {
            LeafKind::Text => messages::ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            },
            LeafKind::Thinking => messages::ContentBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        };
        events.push(messages::MessageStreamEvent::ContentBlockStart {
            index,
            content_block,
        });
        self.leaf = Some((index, kind));
        index
    }

    fn fold_tool_args(&mut self, slot: u32, incoming: &str) {
        let Some(block) = self.tool_blocks.get_mut(&slot) else {
            return;
        };
        if incoming.is_empty() || incoming == block.args_acc {
            return;
        }
        if incoming.starts_with(&block.args_acc) {
            block.args_acc = incoming.to_string();
        } else {
            block.args_acc = format!("{}{incoming}", block.args_acc);
        }
    }

    fn new_tool_slot(&mut self, id: String) -> Result<u32, DevinWireError> {
        let slot = u32::try_from(self.tool_order.len())
            .map_err(|_| DevinWireError::MalformedToolCall("too many tool calls"))?;
        self.tool_order.push(slot);
        self.tool_blocks.insert(
            slot,
            ToolBlock {
                id,
                name: String::new(),
                args_acc: String::new(),
            },
        );
        Ok(slot)
    }

    pub fn push_response(
        &mut self,
        response: &GetChatMessageResponse,
    ) -> Result<Vec<messages::MessageStreamEvent>, DevinWireError> {
        let mut events = Vec::new();

        if !response.delta_thinking.is_empty() {
            let index = self.open_leaf(LeafKind::Thinking, &mut events);
            events.push(messages::MessageStreamEvent::ContentBlockDelta {
                index,
                delta: messages::StreamDelta::ThinkingDelta {
                    thinking: response.delta_thinking.clone(),
                },
            });
        }
        if !response.delta_signature.is_empty() {
            self.thinking_signature.push_str(&response.delta_signature);
        }
        if !response.delta_signature_type.is_empty() {
            self.thinking_signature_type = response.delta_signature_type.clone();
        }
        if response.thinking_redacted {
            self.thinking_redacted = true;
        }

        if !response.delta_text.is_empty() {
            let index = self.open_leaf(LeafKind::Text, &mut events);
            events.push(messages::MessageStreamEvent::ContentBlockDelta {
                index,
                delta: messages::StreamDelta::TextDelta {
                    text: response.delta_text.clone(),
                },
            });
        }

        for call in &response.delta_tool_calls {
            if call.id.is_empty() && call.name.is_empty() && call.arguments_json.is_empty() {
                continue;
            }
            self.close_leaf(&mut events);

            let slot = if !call.id.is_empty() {
                if let Some(&slot) = self.tool_index_by_id.get(&call.id) {
                    slot
                } else if let Some(active) = self.active_tool_slot
                    && self
                        .tool_blocks
                        .get(&active)
                        .is_some_and(|b| b.id.is_empty())
                {
                    if let Some(block) = self.tool_blocks.get_mut(&active) {
                        block.id = call.id.clone();
                    }
                    self.tool_index_by_id.insert(call.id.clone(), active);
                    active
                } else {
                    self.new_tool_slot(call.id.clone())?
                }
            } else {
                let names_compatible = |stored: &str| {
                    call.name.is_empty()
                        || stored.is_empty()
                        || call.name.starts_with(stored)
                        || stored.starts_with(&call.name)
                };
                match self.active_tool_slot {
                    Some(active)
                        if self
                            .tool_blocks
                            .get(&active)
                            .is_some_and(|b| names_compatible(&b.name)) =>
                    {
                        active
                    }
                    _ => self.new_tool_slot(String::new())?,
                }
            };
            self.active_tool_slot = Some(slot);

            {
                let block = self
                    .tool_blocks
                    .get_mut(&slot)
                    .ok_or(DevinWireError::MalformedToolCall("tool-call slot missing"))?;
                if !call.name.is_empty() && call.name != block.name {
                    if block.name.is_empty() || call.name.starts_with(&block.name) {
                        block.name = call.name.clone();
                    } else if !block.name.starts_with(&call.name) {
                        return Err(DevinWireError::MalformedToolCall(
                            "tool-call name changed incompatibly",
                        ));
                    }
                }
            }
            self.fold_tool_args(slot, &call.arguments_json);
        }

        if response.stop_reason != 0 {
            self.latest_stop = Some(response.stop_reason);
        }
        if let Some(usage) = &response.usage {
            self.usage = Some(usage.clone());
        }

        Ok(events)
    }

    pub fn finish_success(&mut self) -> Result<Vec<messages::MessageStreamEvent>, DevinWireError> {
        if self.latest_stop == Some(stop_reason::ERROR) {
            return Err(DevinWireError::ModelError);
        }
        for slot in &self.tool_order {
            let block = &self.tool_blocks[slot];
            if block.name.trim().is_empty() {
                return Err(DevinWireError::MalformedToolCall(
                    "unfinished tool-call metadata",
                ));
            }
            let args = block.args_acc.trim();
            if !args.is_empty()
                && !matches!(
                    serde_json::from_str::<serde_json::Value>(args),
                    Ok(serde_json::Value::Object(_))
                )
            {
                return Err(DevinWireError::MalformedToolCall(
                    "tool-call arguments must be a JSON object",
                ));
            }
        }
        if self.latest_stop == Some(stop_reason::FUNCTION_CALL) && self.tool_order.is_empty() {
            return Err(DevinWireError::MalformedToolCall(
                "tool-use stop without a tool call",
            ));
        }
        let has_tools = !self.tool_order.is_empty();
        let mut events = Vec::new();
        self.close_leaf(&mut events);
        for slot in std::mem::take(&mut self.tool_order) {
            let Some(block) = self.tool_blocks.remove(&slot) else {
                continue;
            };
            let index = self.alloc_index();
            let id = if block.id.trim().is_empty() {
                format!("devin_call_{:x}_{slot}", self.nonce)
            } else {
                block.id
            };
            events.push(messages::MessageStreamEvent::ContentBlockStart {
                index,
                content_block: messages::ContentBlock::ToolUse {
                    id,
                    name: block.name,
                    input: serde_json::Value::Null,
                },
            });
            let args = block.args_acc.trim();
            events.push(messages::MessageStreamEvent::ContentBlockDelta {
                index,
                delta: messages::StreamDelta::InputJsonDelta {
                    partial_json: if args.is_empty() {
                        "{}".to_string()
                    } else {
                        args.to_string()
                    },
                },
            });
            events.push(messages::MessageStreamEvent::ContentBlockStop { index });
        }
        self.tool_blocks.clear();
        self.tool_index_by_id.clear();
        self.active_tool_slot = None;
        let stop = match self.latest_stop.unwrap_or(0) {
            stop_reason::INCOMPLETE | stop_reason::MAX_TOKENS => messages::StopReason::MaxTokens,
            stop_reason::CONTENT_FILTER => messages::StopReason::Refusal,
            _ if has_tools => messages::StopReason::ToolUse,
            _ => messages::StopReason::EndTurn,
        };
        events.push(messages::MessageStreamEvent::MessageDelta {
            delta: messages::MessageDeltaBody {
                stop_reason: Some(stop),
                stop_details: None,
            },
            usage: self.message_delta_usage(),
        });
        events.push(messages::MessageStreamEvent::MessageStop);
        Ok(events)
    }

    fn message_delta_usage(&self) -> messages::MessageDeltaUsage {
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        match &self.usage {
            Some(u) => messages::MessageDeltaUsage {
                output_tokens: clamp(u.output_tokens),
                input_tokens: Some(clamp(u.input_tokens)),
                cache_read_input_tokens: Some(clamp(u.cache_read_tokens)),
                cache_creation_input_tokens: Some(clamp(u.cache_write_tokens)),
            },
            None => messages::MessageDeltaUsage::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(payload: &[u8]) -> Vec<u8> {
        gzip_compress(payload)
    }

    #[test]
    fn session_token_prefix_is_idempotent() {
        assert_eq!(
            normalize_devin_session_token("abc"),
            "devin-session-token$abc"
        );
        assert_eq!(
            normalize_devin_session_token("devin-session-token$abc"),
            "devin-session-token$abc"
        );
    }

    #[test]
    fn cli_metadata_carries_released_identity_and_disabled_telemetry() {
        let meta = devin_cli_metadata("devin-session-token$t", "jwt-1", 7, "sess");
        assert_eq!(meta.ide_name, "devin-cli");
        assert_eq!(meta.ide_type, "chisel");
        assert_eq!(meta.ide_version, "3000.11.3");
        assert_eq!(meta.extension_name, "chisel");
        assert_eq!(meta.extension_version, "3000.11.3");
        assert_eq!(meta.api_key, "devin-session-token$t");
        assert_eq!(meta.user_jwt, "jwt-1");
        assert!(meta.disable_telemetry);
        assert!(["darwin", "windows", "linux"].contains(&meta.os.as_str()));
        assert_eq!(meta.request_id, 7);
        assert_eq!(meta.session_id, "sess");
        let disc = devin_discovery_metadata("k", 0, "");
        assert_eq!(disc.ide_version, "0.0.0-dev");
        assert_eq!(disc.extension_version, "0.0.0-dev");
        assert_eq!(disc.ide_type, "");
        assert!(disc.disable_telemetry);
        assert_eq!(disc.client_capabilities, [3, 4, 6, 7, 8]);
        assert!(
            disc.encode_to_vec()
                .windows(7)
                .any(|w| w == [0xf2, 0x01, 0x05, 3, 4, 6, 7]),
            "tag 30 goes out packed, as the released CLI sends it"
        );
        assert!(meta.client_capabilities.is_empty());
    }

    #[test]
    fn signature_envelope_roundtrip_and_foreign_rejection() {
        let packed = pack_devin_signature("MODEL_X", "sig-abc", "type-t");
        assert!(packed.starts_with("kigi-devin-v1:"));
        let env = unpack_devin_signature(&packed).expect("valid envelope");
        assert_eq!(env.model_uid, "MODEL_X");
        assert_eq!(env.signature, "sig-abc");
        assert_eq!(env.signature_type, "type-t");
        assert!(unpack_devin_signature("EsigCkYICxgCKkA=").is_none());
        assert!(unpack_devin_signature("kigi-devin-v1:{not json").is_none());
    }

    #[test]
    fn decode_unary_bare_and_gzipped() {
        let resp = GetUserJwtResponse {
            user_jwt: "jwt-1".into(),
            custom_api_server_url: String::new(),
        };
        let bare = resp.encode_to_vec();
        let decoded: GetUserJwtResponse = decode_unary(&bare).expect("bare decode");
        assert_eq!(decoded.user_jwt, "jwt-1");
        let decoded: GetUserJwtResponse = decode_unary(&gzip(&bare)).expect("gzip decode");
        assert_eq!(decoded.user_jwt, "jwt-1");
    }

    #[test]
    fn gunzip_bounded_rejects_bomb() {
        let big = vec![0x55u8; MAX_CONNECT_FRAME_PAYLOAD + 8];
        let bomb = gzip(&big);
        assert!(matches!(
            gunzip_bounded(&bomb, MAX_CONNECT_FRAME_PAYLOAD),
            Err(DevinWireError::DecompressionBomb)
        ));
        assert!(matches!(
            gunzip_bounded(b"not gzip", 16),
            Err(DevinWireError::Gzip)
        ));
    }

    fn framed(payload: &[u8], flags: u8) -> Vec<u8> {
        let mut out = vec![flags];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn decoder_handles_every_byte_boundary() {
        let data = framed(b"first", 0);
        let compressed = {
            let gz = gzip(b"second-payload");
            framed(&gz, CONNECT_FLAG_COMPRESSED)
        };
        let trailer = framed(b"{}", CONNECT_FLAG_END_STREAM);
        let mut wire = data;
        wire.extend_from_slice(&compressed);
        wire.extend_from_slice(&trailer);

        for split in 0..=wire.len() {
            let mut dec = ConnectDecoder::new();
            let mut frames = Vec::new();
            frames.extend(dec.push(&wire[..split]).expect("push head"));
            frames.extend(dec.push(&wire[split..]).expect("push tail"));
            assert_eq!(frames.len(), 3, "split {split}");
            assert_eq!(
                frames[0],
                ConnectFrame::Data(b"first".to_vec()),
                "split {split}"
            );
            assert_eq!(
                frames[1],
                ConnectFrame::Data(b"second-payload".to_vec()),
                "split {split}"
            );
            assert_eq!(
                frames[2],
                ConnectFrame::EndStream(b"{}".to_vec()),
                "split {split}"
            );
            dec.finish().expect("clean eof");
        }
    }

    #[test]
    fn decoder_handles_coalesced_max_frames_in_one_push() {
        let big = vec![0x42u8; MAX_CONNECT_FRAME_PAYLOAD];
        let mut wire = framed(&big, 0);
        wire.extend_from_slice(&framed(&big, 0));
        wire.extend_from_slice(&framed(b"{}", CONNECT_FLAG_END_STREAM));
        let mut dec = ConnectDecoder::new();
        let frames = dec.push(&wire).expect("coalesced push");
        assert_eq!(
            frames,
            vec![
                ConnectFrame::Data(big.clone()),
                ConnectFrame::Data(big),
                ConnectFrame::EndStream(b"{}".to_vec()),
            ]
        );
        dec.finish().expect("clean eof");
    }

    #[test]
    fn decoder_partial_then_boundary_chunk_completes_both() {
        let payload = vec![0x99u8; MAX_CONNECT_FRAME_PAYLOAD - 16];
        let mut wire = framed(&payload, 0);
        wire.extend_from_slice(&framed(b"{}", CONNECT_FLAG_END_STREAM));
        let split = wire.len() - 8;
        let mut dec = ConnectDecoder::new();
        let mut frames = dec.push(&wire[..split]).expect("head");
        assert!(frames.is_empty(), "one byte short of frame A's payload");
        frames.extend(dec.push(&wire[split..]).expect("tail"));
        assert_eq!(
            frames,
            vec![
                ConnectFrame::Data(payload),
                ConnectFrame::EndStream(b"{}".to_vec()),
            ]
        );
        dec.finish().expect("clean eof");
    }

    #[test]
    fn decoder_rejects_oversize_advertised_length_before_buffering() {
        let mut dec = ConnectDecoder::new();
        assert!(matches!(
            dec.push(&[0x00, 0x01, 0x01, 0x00, 0x00]),
            Err(DevinWireError::OversizeFrame(l)) if l == (16 * 1024 * 1024) + 65536
        ));
    }

    #[test]
    fn decoder_rejects_decompression_bomb() {
        let big = vec![0xAAu8; MAX_CONNECT_FRAME_PAYLOAD + 1];
        let gz = gzip(&big);
        let mut dec = ConnectDecoder::new();
        assert!(matches!(
            dec.push(&framed(&gz, CONNECT_FLAG_COMPRESSED)),
            Err(DevinWireError::DecompressionBomb)
        ));
    }

    #[test]
    fn decoder_rejects_unsupported_flags_and_garbage() {
        let mut dec = ConnectDecoder::new();
        assert!(matches!(
            dec.push(&framed(b"x", 0x04)),
            Err(DevinWireError::UnsupportedFlags(0x04))
        ));
        let mut dec = ConnectDecoder::new();
        assert!(dec.push(&framed(b"abc", 0)[..6]).expect("push").is_empty());
        assert!(matches!(dec.finish(), Err(DevinWireError::TruncatedStream)));
    }

    #[test]
    fn trailer_codes_map_to_statuses() {
        let cases = [
            ("unauthenticated", 401),
            ("permission_denied", 403),
            ("resource_exhausted", 429),
            ("invalid_argument", 400),
            ("internal", 500),
            ("unavailable", 503),
        ];
        for (code, want) in cases {
            let payload =
                format!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"SECRET-REFLECT\"}}}}");
            let err = parse_connect_trailer(payload.as_bytes())
                .expect("trailer parses")
                .expect("error trailer");
            assert_eq!(err.code, code);
            let wire = err.into_wire_error();
            let rendered = wire.to_string();
            assert!(
                !rendered.contains("SECRET-REFLECT"),
                "trailer message leaked into error: {rendered}"
            );
            match wire {
                DevinWireError::Trailer { status, .. } => assert_eq!(status, want),
                other => panic!("expected trailer error, got {other:?}"),
            }
        }
        let err = parse_connect_trailer(br#"{"error":{"code":"some_future_code","message":"m"}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(err.code, "unrecognized");
        assert!(matches!(
            err.into_wire_error(),
            DevinWireError::Trailer { status: 502, .. }
        ));
        let wire =
            parse_connect_trailer(br#"{"error":{"code":"leaked-token-value-1","message":"m"}}"#)
                .unwrap()
                .unwrap()
                .into_wire_error();
        assert!(!wire.to_string().contains("leaked-token-value-1"));
        assert!(parse_connect_trailer(b"{}").unwrap().is_none());
        assert!(
            parse_connect_trailer(br#"{"error":null}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_connect_trailer(br#"{"metadata":{"x":1}}"#)
                .unwrap()
                .is_none()
        );
        for bad in [
            &b"not json"[..],
            b"null",
            b"[]",
            b"\"a string\"",
            b"42",
            br#"{"error":{}}"#,
            br#"{"error":{"code":123}}"#,
            br#"{"error":{"code":""}}"#,
            br#"{"error":"unauthenticated"}"#,
        ] {
            assert!(
                matches!(
                    parse_connect_trailer(bad),
                    Err(DevinWireError::MalformedTrailer)
                ),
                "malformed trailer must fail: {}",
                String::from_utf8_lossy(bad)
            );
        }
        let err = parse_connect_trailer(br#"{"error":{"code":"unauthenticated"}}"#)
            .unwrap()
            .unwrap()
            .into_wire_error()
            .into_sampling_error();
        assert!(matches!(err, SamplingError::Auth(_)));
    }

    fn ids() -> DevinRequestIds {
        DevinRequestIds {
            cascade_id: "cascade-1".into(),
            execution_id: "exec-1".into(),
            session_id: "sess-1".into(),
            request_id: 42,
        }
    }

    fn req(items: Vec<ConversationItem>) -> ConversationRequest {
        ConversationRequest {
            items,
            model: Some("MODEL_X".into()),
            ..Default::default()
        }
    }

    #[test]
    fn build_request_decoded_tags_and_defaults() {
        let request = req(vec![
            ConversationItem::System(crate::SystemItem {
                content: std::sync::Arc::from("sys-a"),
            }),
            ConversationItem::System(crate::SystemItem {
                content: std::sync::Arc::from("sys-b"),
            }),
            ConversationItem::user("hello"),
        ]);
        let wire =
            build_devin_chat_request(&request, "devin-session-token$t", "jwt", "MODEL_X", &ids())
                .expect("build");
        let meta = wire.metadata.as_ref().expect("metadata");
        assert_eq!(meta.ide_name, "devin-cli");
        assert_eq!(meta.ide_type, "chisel");
        assert!(meta.disable_telemetry);
        assert_eq!(meta.api_key, "devin-session-token$t");
        assert_eq!(meta.user_jwt, "jwt");
        assert_eq!(wire.prompt, "sys-a\n\nsys-b");
        assert_eq!(wire.request_type, REQUEST_TYPE_CASCADE);
        assert_eq!(wire.planner_mode, PLANNER_MODE_DEFAULT);
        assert_eq!(wire.chat_model_uid, "MODEL_X");
        assert_eq!(wire.cascade_id, "cascade-1");
        assert_eq!(wire.execution_id, "exec-1");
        assert_eq!(wire.chat_message_prompts.len(), 1);
        assert_eq!(
            wire.chat_message_prompts[0].source,
            chat_message_source::USER
        );
        let cfg = wire.configuration.as_ref().expect("configuration");
        assert_eq!(cfg.num_completions, 1);
        assert_eq!(cfg.max_tokens, 64_000);
        assert_eq!(cfg.max_newlines, 200);
        assert_eq!(cfg.temperature, 0.4);
        assert_eq!(cfg.first_temperature, 0.4);
        assert_eq!(cfg.top_k, 50);
        assert_eq!(cfg.top_p, 1.0);
        assert_eq!(cfg.fim_eot_prob_threshold, 1.0);
        assert_eq!(cfg.stop_patterns.len(), 5);
        let decoded =
            GetChatMessageRequest::decode(&wire.encode_to_vec()[..]).expect("round-trip decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn assistant_maps_to_source_2_and_tool_ids_are_symmetric() {
        let request = req(vec![
            ConversationItem::user("run it"),
            ConversationItem::assistant_tool_calls(vec![crate::ToolCall {
                id: std::sync::Arc::from("call_1"),
                name: "bash".into(),
                arguments: std::sync::Arc::from("{\"cmd\":\"ls\"}"),
            }]),
            ConversationItem::tool_result("call_1", "file.txt"),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        assert_eq!(wire.chat_message_prompts.len(), 3);
        let assistant = &wire.chat_message_prompts[1];
        assert_eq!(assistant.source, chat_message_source::ASSISTANT);
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].id, "call_1");
        assert_eq!(assistant.tool_calls[0].name, "bash");
        assert_eq!(assistant.tool_calls[0].arguments_json, "{\"cmd\":\"ls\"}");
        let result = &wire.chat_message_prompts[2];
        assert_eq!(result.source, chat_message_source::TOOL);
        assert_eq!(result.tool_call_id, "call_1");
    }

    #[test]
    fn user_images_ride_the_prompt_and_tool_images_relocate() {
        let img = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
        let request = req(vec![
            ConversationItem::user_with_parts(vec![
                ContentPart::Text {
                    text: "look".into(),
                },
                ContentPart::Image {
                    url: std::sync::Arc::from(img),
                },
            ]),
            ConversationItem::assistant_tool_calls(vec![crate::ToolCall {
                id: std::sync::Arc::from("c1"),
                name: "read".into(),
                arguments: std::sync::Arc::from("{}"),
            }]),
            ConversationItem::tool_result_with_images(
                "c1",
                "ok",
                vec![ContentPart::Image {
                    url: std::sync::Arc::from(img),
                }],
            ),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        assert_eq!(wire.chat_message_prompts[0].images.len(), 1);
        assert_eq!(
            wire.chat_message_prompts[0].images[0].base64_data,
            "iVBORw0KGgoAAAANSUhEUg=="
        );
        assert_eq!(
            wire.chat_message_prompts[0].images[0].mime_type,
            "image/png"
        );
        let last = wire.chat_message_prompts.last().expect("synthetic user");
        assert_eq!(last.source, chat_message_source::USER);
        assert_eq!(last.images.len(), 1);
    }

    #[test]
    fn replayed_signature_requires_open_loop_same_model() {
        let reasoning = |sig: &str| {
            ConversationItem::Reasoning(crate::rs::ReasoningItem {
                id: String::new(),
                summary: vec![crate::rs::SummaryPart::SummaryText(
                    crate::rs::SummaryTextContent {
                        text: "thought".into(),
                    },
                )],
                content: None,
                encrypted_content: Some(sig.to_string()),
                status: None,
            })
        };
        let assistant_call = || {
            ConversationItem::assistant_tool_calls(vec![crate::ToolCall {
                id: std::sync::Arc::from("c1"),
                name: "bash".into(),
                arguments: std::sync::Arc::from("{}"),
            }])
        };
        let sig = pack_devin_signature("MODEL_X", "sig-9", "sealed");

        let request = req(vec![
            ConversationItem::user("u"),
            reasoning(&sig),
            assistant_call(),
            ConversationItem::tool_result("c1", "done"),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        let a = &wire.chat_message_prompts[1];
        assert_eq!(a.thinking, "thought");
        assert_eq!(a.signature, "sig-9");
        assert_eq!(a.signature_type, "sealed");

        let request = req(vec![
            ConversationItem::user("u"),
            reasoning(&sig),
            assistant_call(),
            ConversationItem::tool_result("c1", "done"),
            ConversationItem::user("next"),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        let a = &wire.chat_message_prompts[1];
        assert!(a.signature.is_empty());
        assert!(a.thinking.is_empty());

        let request = req(vec![
            ConversationItem::user("u"),
            reasoning(&sig),
            assistant_call(),
            ConversationItem::tool_result("c1", "done"),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_Y", &ids()).expect("build");
        assert!(wire.chat_message_prompts[1].signature.is_empty());

        let request = req(vec![
            ConversationItem::user("u"),
            reasoning("EsigCkYICxgCKkA="),
            assistant_call(),
            ConversationItem::tool_result("c1", "done"),
        ]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        assert!(wire.chat_message_prompts[1].signature.is_empty());
    }

    #[test]
    fn tool_choice_and_tool_schema_projection() {
        use crate::ToolSpec;
        let spec = ToolSpec {
            name: "bash".into(),
            description: Some("run".into()),
            parameters: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
        };
        for (choice, want) in [
            (ConversationToolChoice::Auto, "auto"),
            (ConversationToolChoice::None, "none"),
            (ConversationToolChoice::Required, "required"),
        ] {
            let mut request = req(vec![ConversationItem::user("u")]);
            request.tools = vec![spec.clone()];
            request.tool_choice = Some(choice);
            let wire =
                build_devin_chat_request(&request, "t", "j", "MODEL_X", &ids()).expect("build");
            match &wire.tool_choice.unwrap().choice {
                Some(chat_tool_choice::Choice::OptionName(name)) => assert_eq!(name, want),
                other => panic!("expected option_name {want}, got {other:?}"),
            }
            assert_eq!(wire.tools[0].name, "bash");
            assert!(!wire.tools[0].strict);
        }
        let mut request = req(vec![ConversationItem::user("u")]);
        request.tools = vec![spec];
        request.tool_choice = Some(ConversationToolChoice::Function("bash".into()));
        let wire = build_devin_chat_request(&request, "t", "j", "MODEL_X", &ids()).expect("build");
        match &wire.tool_choice.unwrap().choice {
            Some(chat_tool_choice::Choice::ToolName(name)) => assert_eq!(name, "bash"),
            other => panic!("expected tool_name, got {other:?}"),
        }
    }

    #[test]
    fn gemini_schema_normalization() {
        use crate::ToolSpec;
        let spec = ToolSpec {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "maybe": {"type": ["string", "null"]},
                    "nested": {"type": "object", "properties": {
                        "inner": {"type": ["null", "number"]}
                    }},
                }
            }),
        };
        let mut request = req(vec![ConversationItem::user("u")]);
        request.tools = vec![spec];
        let wire =
            build_devin_chat_request(&request, "t", "j", "MODEL_GOOGLE_GEMINI_3_PRO", &ids())
                .expect("build");
        let schema: serde_json::Value =
            serde_json::from_str(&wire.tools[0].json_schema_string).expect("schema json");
        assert_eq!(schema["properties"]["maybe"]["type"], "string");
        assert_eq!(schema["properties"]["maybe"]["nullable"], true);
        assert_eq!(
            schema["properties"]["nested"]["properties"]["inner"]["type"],
            "number"
        );
        assert_eq!(
            schema["properties"]["nested"]["properties"]["inner"]["nullable"],
            true
        );
        let bad = ToolSpec {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({"type": ["string", "number", "boolean"]}),
        };
        let mut request = req(vec![ConversationItem::user("u")]);
        request.tools = vec![bad];
        assert!(matches!(
            build_devin_chat_request(&request, "t", "j", "MODEL_GOOGLE_GEMINI_3_PRO", &ids()),
            Err(DevinWireError::Unsupported(_))
        ));
        let mut request = req(vec![ConversationItem::user("u")]);
        request.tools = vec![ToolSpec {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({"type": ["string", "number", "boolean"]}),
        }];
        let wire = build_devin_chat_request(&request, "t", "j", "MODEL_X", &ids()).expect("build");
        let schema: serde_json::Value =
            serde_json::from_str(&wire.tools[0].json_schema_string).expect("schema json");
        assert_eq!(
            schema["type"],
            serde_json::json!(["string", "number", "boolean"])
        );
    }

    #[test]
    fn gemini_schema_edge_cases() {
        use crate::ToolSpec;
        let gemini_req = |schema: serde_json::Value| -> ConversationRequest {
            let mut request = req(vec![ConversationItem::user("u")]);
            request.tools = vec![ToolSpec {
                name: "t".into(),
                description: None,
                parameters: schema,
            }];
            request
        };
        let schema_of = |request: &ConversationRequest| -> serde_json::Value {
            let wire =
                build_devin_chat_request(request, "t", "j", "MODEL_GOOGLE_GEMINI_3_PRO", &ids())
                    .expect("build");
            serde_json::from_str(&wire.tools[0].json_schema_string).expect("json")
        };
        let s = schema_of(&gemini_req(serde_json::json!({"type": ["string"]})));
        assert_eq!(s["type"], "string");
        assert!(s.get("nullable").is_none(), "no spurious nullable: {s}");
        let s = schema_of(&gemini_req(serde_json::json!({"type": ["string", "null"]})));
        assert_eq!(s["type"], "string");
        assert_eq!(s["nullable"], true);
        let s = schema_of(&gemini_req(
            serde_json::json!({"type": ["null"], "nullable_hint": 1}),
        ));
        assert!(s.get("type").is_none());
        assert_eq!(s["nullable"], true);
        for bad in [
            serde_json::json!({"type": []}),
            serde_json::json!({"type": ["string", 7]}),
            serde_json::json!({"type": [{"nested":"object"}]}),
            serde_json::json!({"type": ["string", "number"]}),
            serde_json::json!({"type": ["string", "number", "null"]}),
        ] {
            assert!(
                matches!(
                    build_devin_chat_request(
                        &gemini_req(bad.clone()),
                        "t",
                        "j",
                        "MODEL_GOOGLE_GEMINI_3_PRO",
                        &ids()
                    ),
                    Err(DevinWireError::Unsupported(_))
                ),
                "malformed/union schema must reject: {bad}"
            );
        }
    }

    #[test]
    fn hosted_tools_and_native_schema_are_rejected() {
        use crate::HostedTool;
        let mut request = req(vec![ConversationItem::user("u")]);
        request.hosted_tools = vec![HostedTool::WebSearch {
            allowed_domains: None,
        }];
        assert!(matches!(
            build_devin_chat_request(&request, "t", "j", "MODEL_X", &ids()),
            Err(DevinWireError::Unsupported("hosted tools"))
        ));
        let mut request = req(vec![ConversationItem::user("u")]);
        request.json_schema = Some(serde_json::json!({"type":"object"}));
        assert!(matches!(
            build_devin_chat_request(&request, "t", "j", "MODEL_X", &ids()),
            Err(DevinWireError::Unsupported(
                "native json_schema response format"
            ))
        ));
    }

    fn resp() -> GetChatMessageResponse {
        GetChatMessageResponse::default()
    }

    #[test]
    fn translator_text_thinking_signature_and_usage() {
        let mut t = DevinEventTranslator::new("MODEL_X".to_string());
        let mut events = vec![t.message_start()];
        let mut thinking = resp();
        thinking.delta_thinking = "hmm ".into();
        events.extend(t.push_response(&thinking).unwrap());
        let mut sig = resp();
        sig.delta_signature = "SIG".into();
        sig.delta_signature_type = "sealed".into();
        events.extend(t.push_response(&sig).unwrap());
        let mut text = resp();
        text.delta_text = "hi".into();
        events.extend(t.push_response(&text).unwrap());
        let mut tail = resp();
        tail.stop_reason = stop_reason::STOP_PATTERN;
        tail.usage = Some(ModelUsageStats {
            input_tokens: 10,
            output_tokens: 3,
            cache_write_tokens: 2,
            cache_read_tokens: 5,
        });
        events.extend(t.push_response(&tail).unwrap());
        events.extend(t.finish_success().unwrap());

        let sig_delta = events.iter().find_map(|e| match e {
            messages::MessageStreamEvent::ContentBlockDelta {
                delta: messages::StreamDelta::SignatureDelta { signature },
                ..
            } => Some(signature.clone()),
            _ => None,
        });
        let env = unpack_devin_signature(&sig_delta.expect("signature delta")).unwrap();
        assert_eq!(env.model_uid, "MODEL_X");
        assert_eq!(env.signature, "SIG");
        assert_eq!(env.signature_type, "sealed");

        let usage = events.iter().find_map(|e| match e {
            messages::MessageStreamEvent::MessageDelta { usage, .. } => Some(usage.clone()),
            _ => None,
        });
        let u = usage.expect("message_delta");
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cache_creation_input_tokens, Some(2));
        assert_eq!(u.cache_read_input_tokens, Some(5));
        let stop = events.iter().find_map(|e| match e {
            messages::MessageStreamEvent::MessageDelta { delta, .. } => delta.stop_reason.clone(),
            _ => None,
        });
        assert!(
            matches!(stop, Some(messages::StopReason::EndTurn)),
            "expected end_turn, got {stop:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(messages::MessageStreamEvent::MessageStop)
            ),
            "stream must end on message_stop"
        );
    }

    #[test]
    fn translator_tool_args_cumulative_and_fragment() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f1 = resp();
        f1.delta_tool_calls = vec![ChatToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments_json: "{\"a\"".into(),
            ..Default::default()
        }];
        let mut f2 = resp();
        f2.delta_tool_calls = vec![ChatToolCall {
            id: String::new(),
            name: String::new(),
            arguments_json: "{\"a\":1}".into(),
            ..Default::default()
        }];
        let mut f3 = resp();
        f3.delta_tool_calls = vec![ChatToolCall {
            id: "call_2".into(),
            name: "read".into(),
            arguments_json: "{\"p\":".into(),
            ..Default::default()
        }];
        let mut f4 = resp();
        f4.delta_tool_calls = vec![ChatToolCall {
            id: "call_2".into(),
            name: String::new(),
            arguments_json: "\"x\"}".into(),
            ..Default::default()
        }];
        let mut f5 = resp();
        f5.delta_tool_calls = vec![ChatToolCall {
            id: "call_2".into(),
            name: String::new(),
            arguments_json: "{\"p\":\"x\"}".into(),
            ..Default::default()
        }];
        let mut f6 = resp();
        f6.stop_reason = stop_reason::FUNCTION_CALL;
        f6.usage = Some(ModelUsageStats::default());

        let mut events = Vec::new();
        for f in [f1, f2, f3, f4, f5, f6] {
            events.extend(t.push_response(&f).unwrap());
        }
        events.extend(t.finish_success().unwrap());

        let deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                messages::MessageStreamEvent::ContentBlockDelta {
                    delta: messages::StreamDelta::InputJsonDelta { partial_json },
                    ..
                } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["{\"a\":1}", "{\"p\":\"x\"}"],
            "one full-json delta per buffered call at finish"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, messages::MessageStreamEvent::ContentBlockStart { .. }))
                .count(),
            2,
            "two tool blocks"
        );
        let stop = events.iter().find_map(|e| match e {
            messages::MessageStreamEvent::MessageDelta { delta, .. } => delta.stop_reason.clone(),
            _ => None,
        });
        assert!(
            matches!(stop, Some(messages::StopReason::ToolUse)),
            "expected tool_use, got {stop:?}"
        );
    }

    #[test]
    fn translator_orphan_delta_and_bad_final_args() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: String::new(),
            name: "x".into(),
            arguments_json: "{}".into(),
            ..Default::default()
        }];
        let mut fend = resp();
        fend.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f).unwrap();
        t.push_response(&fend).unwrap();
        let events = t.finish_success().expect("orphan delta is buffered");
        let id = events.iter().find_map(|e| match e {
            messages::MessageStreamEvent::ContentBlockStart {
                content_block: messages::ContentBlock::ToolUse { id, .. },
                ..
            } => Some(id.clone()),
            _ => None,
        });
        assert!(
            id.expect("tool_use").starts_with("devin_call_"),
            "anonymous calls get a synthesized id"
        );

        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "x".into(),
            arguments_json: "{\"truncated".into(),
            ..Default::default()
        }];
        t.push_response(&f).unwrap();
        let err = t.finish_success().expect_err("truncated args must fail");
        assert!(!err.into_sampling_error().is_retryable());
    }

    #[test]
    fn translator_tool_name_and_args_object_guards() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: String::new(),
            arguments_json: "{}".into(),
            ..Default::default()
        }];
        t.push_response(&f).unwrap();
        assert!(t.finish_success().is_err(), "nameless call fails at finish");
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "   ".into(),
            arguments_json: "{}".into(),
            ..Default::default()
        }];
        t.push_response(&f).unwrap();
        assert!(
            t.finish_success().is_err(),
            "whitespace name fails at finish"
        );

        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments_json: "{}".into(),
            ..Default::default()
        }];
        t.push_response(&f).unwrap();
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments_json: String::new(),
            ..Default::default()
        }];
        assert!(t.push_response(&f).is_err(), "name change must fail");

        for args in ["[1,2]", "\"text\"", "42"] {
            let mut t = DevinEventTranslator::new("M".to_string());
            let mut f = resp();
            f.delta_tool_calls = vec![ChatToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments_json: args.into(),
                ..Default::default()
            }];
            t.push_response(&f).unwrap();
            assert!(
                t.finish_success().is_err(),
                "non-object final args must fail: {args}"
            );
        }
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments_json: String::new(),
            ..Default::default()
        }];
        t.push_response(&f).unwrap();
        let events = t.finish_success().expect("empty args ok");
        assert!(events.iter().any(|e| matches!(
            e,
            messages::MessageStreamEvent::ContentBlockDelta {
                delta: messages::StreamDelta::InputJsonDelta { partial_json },
                ..
            } if partial_json == "{}"
        )));
    }

    #[test]
    fn translator_stop_reason_mapping() {
        for (wire, want) in [
            (stop_reason::INCOMPLETE, messages::StopReason::MaxTokens),
            (stop_reason::MAX_TOKENS, messages::StopReason::MaxTokens),
            (stop_reason::CONTENT_FILTER, messages::StopReason::Refusal),
            (stop_reason::STOP_PATTERN, messages::StopReason::EndTurn),
            (0, messages::StopReason::EndTurn),
        ] {
            let mut t = DevinEventTranslator::new("M".to_string());
            let mut f = resp();
            f.stop_reason = wire;
            t.push_response(&f).unwrap();
            let events = t.finish_success().unwrap();
            let got = events.iter().find_map(|e| match e {
                messages::MessageStreamEvent::MessageDelta { delta, .. } => {
                    delta.stop_reason.clone()
                }
                _ => None,
            });
            let expected = match want {
                messages::StopReason::MaxTokens => "max_tokens",
                messages::StopReason::ToolUse => "tool_use",
                messages::StopReason::Refusal => "refusal",
                _ => "end_turn",
            };
            assert!(
                got.as_ref().is_some_and(|g| {
                    std::mem::discriminant(g) == std::mem::discriminant(&want)
                }),
                "wire {wire}: expected {expected}, got {got:?}"
            );
        }
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.stop_reason = stop_reason::ERROR;
        t.push_response(&f).unwrap();
        assert!(matches!(
            t.finish_success(),
            Err(DevinWireError::ModelError)
        ));
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f).unwrap();
        assert!(
            matches!(
                t.finish_success(),
                Err(DevinWireError::MalformedToolCall(_))
            ),
            "function_call without any tool call is fatal"
        );
    }

    #[test]
    fn translator_redacted_thinking_carries_no_signature() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_thinking = "x".into();
        f.delta_signature = "SIG".into();
        f.thinking_redacted = true;
        let mut events = t.push_response(&f).unwrap();
        events.extend(t.finish_success().unwrap());
        let has_sig = events.iter().any(|e| {
            matches!(
                e,
                messages::MessageStreamEvent::ContentBlockDelta {
                    delta: messages::StreamDelta::SignatureDelta { .. },
                    ..
                }
            )
        });
        assert!(!has_sig, "redacted thinking must not emit a signature");
    }

    #[test]
    fn catalog_family_metadata_roundtrips_at_wire_tags() {
        let cfg = ClientModelConfig {
            model_uid: "MODEL_SW_2_HIGH".into(),
            label: "SWE-2 High".into(),
            description: Some("frontier coding".into()),
            is_default_model_in_family: true,
            model_info: Some(ModelInfo {
                model_family_uid: "swe-2".into(),
                ..Default::default()
            }),
            model_family_metadata: Some(ModelFamilyMetadata {
                model_family_label: "SWE-2".into(),
                is_default_model_in_family: false,
                entries: vec![ModelFamilyMetadataEntry {
                    key: "variant".into(),
                    value: Some(ModelFamilyMetadataValue {
                        order: 2,
                        name: "High".into(),
                    }),
                }],
            }),
            ..Default::default()
        };
        let resp = GetCliModelConfigsResponse {
            client_model_configs: vec![cfg],
        };
        let decoded: GetCliModelConfigsResponse =
            decode_unary(&resp.encode_to_vec()).expect("roundtrip");
        let d = &decoded.client_model_configs[0];
        assert_eq!(d.description.as_deref(), Some("frontier coding"));
        assert!(d.is_default_model_in_family);
        let meta = d.model_family_metadata.as_ref().expect("metadata");
        assert_eq!(meta.model_family_label, "SWE-2");
        assert!(!meta.is_default_model_in_family);
        assert_eq!(meta.entries.len(), 1);
        assert_eq!(meta.entries[0].key, "variant");
        let v = meta.entries[0].value.as_ref().expect("entry value");
        assert_eq!(v.order, 2);
        assert_eq!(v.name, "High");
        let info = d.model_info.as_ref().expect("model info");
        assert_eq!(info.model_family_uid, "swe-2");
    }

    #[test]
    fn catalog_absent_family_metadata_stays_absent() {
        let resp = GetCliModelConfigsResponse {
            client_model_configs: vec![ClientModelConfig {
                model_uid: "MODEL_PLAIN".into(),
                ..Default::default()
            }],
        };
        let decoded: GetCliModelConfigsResponse =
            decode_unary(&resp.encode_to_vec()).expect("roundtrip");
        let d = &decoded.client_model_configs[0];
        assert!(d.model_family_metadata.is_none());
        assert!(!d.is_default_model_in_family);
        assert!(d.description.is_none());
        let blank_meta = ClientModelConfig {
            model_uid: "MODEL_BLANK".into(),
            model_family_metadata: Some(ModelFamilyMetadata {
                model_family_label: "   ".into(),
                entries: vec![],
                is_default_model_in_family: false,
            }),
            ..Default::default()
        };
        let resp2 = GetCliModelConfigsResponse {
            client_model_configs: vec![blank_meta],
        };
        let d2: GetCliModelConfigsResponse =
            decode_unary(&resp2.encode_to_vec()).expect("roundtrip");
        assert_eq!(
            d2.client_model_configs[0]
                .model_family_metadata
                .as_ref()
                .map(|m| m.model_family_label.trim().is_empty()),
            Some(true)
        );
    }

    #[test]
    fn catalog_family_fields_decode_at_exact_wire_tags() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0xb2, 0x01, 0x01, 0x61]);
        wire.extend_from_slice(&[0xf2, 0x01, 0x03, 0x0a, 0x01, 0x46]);
        wire.extend_from_slice(&[0xf8, 0x01, 0x01]);
        wire.extend_from_slice(&[0xba, 0x01, 0x04, 0xba, 0x01, 0x01, 0x66]);
        let cfg = ClientModelConfig::decode(&wire[..]).expect("decode raw fixture");
        assert_eq!(cfg.model_uid, "a");
        assert_eq!(
            cfg.model_family_metadata
                .as_ref()
                .map(|m| m.model_family_label.as_str()),
            Some("F")
        );
        assert!(cfg.is_default_model_in_family);
        assert_eq!(
            cfg.model_info.as_ref().map(|i| i.model_family_uid.as_str()),
            Some("f")
        );
    }

    #[test]
    fn fusion_model_uids_parses_pair_and_rejects_malformed() {
        assert_eq!(
            fusion_model_uids("fusion-claude-x-medium-sidekick-swe-y-medium"),
            Some(("claude-x-medium", "swe-y-medium"))
        );
        for bad in [
            "",
            "fusion-",
            "fusion-a",
            "fusion--sidekick-b",
            "fusion-a-sidekick-",
            "fusion-fusion-a-sidekick-b",
            "fusion-a-sidekick-fusion-b",
            "fusion-a-sidekick-b-sidekick-c",
            "claude-medium",
        ] {
            assert!(fusion_model_uids(bad).is_none(), "rejected: {bad}");
        }
    }

    #[test]
    fn assign_model_response_decodes_lead_fixture_bytes() {
        let resp =
            AssignModelResponse::decode(&[0x0a, 0x06, 0x0a, 0x01, 0x6a, 0x12, 0x01, 0x6c][..])
                .expect("decode");
        let assignment = resp.assignment.expect("assignment present");
        assert_eq!(assignment.assignment_jwt, "j");
        assert_eq!(assignment.model_uid, "l");
    }

    #[test]
    fn chat_request_tag26_assignment_jwt_decodes() {
        let wire = [0xd2u8, 0x01, 0x01, 0x6a];
        let req = GetChatMessageRequest::decode(&wire[..]).expect("decode");
        assert_eq!(req.model_assignment_jwt.as_deref(), Some("j"));
        let plain = GetChatMessageRequest::decode(&[][..]).expect("decode empty");
        assert!(plain.model_assignment_jwt.is_none());
    }

    #[test]
    fn assign_model_request_uses_latest_user_prompt_only() {
        let request = req(vec![
            ConversationItem::user("earlier"),
            ConversationItem::user("Fusion smoke"),
            ConversationItem::tool_result("t1", "x"),
        ]);
        let assign = build_devin_assign_model_request(
            &request,
            "devin-session-token$t",
            "jwt",
            "fusion-a-sidekick-b",
            &ids(),
        );
        assert_eq!(assign.model_router_uid, "fusion-a-sidekick-b");
        assert_eq!(assign.cascade_id, "cascade-1");
        let meta = assign.metadata.expect("metadata");
        assert_eq!(meta.api_key, "devin-session-token$t");
        assert_eq!(meta.user_jwt, "jwt");
        let prompt = assign.chat_message_prompt.expect("latest user prompt");
        assert_eq!(prompt.source, chat_message_source::USER);
        assert_eq!(prompt.prompt, "Fusion smoke");
        assert!(prompt.tool_call_id.is_empty());
        assert!(prompt.tool_calls.is_empty());
        assert!(prompt.thinking.is_empty());
        assert!(prompt.message_id.is_empty());
    }

    #[test]
    fn assign_model_request_preserves_user_image() {
        let request = req(vec![ConversationItem::user_with_parts(vec![
            ContentPart::Text {
                text: "look".into(),
            },
            ContentPart::Image {
                url: "data:image/png;base64,aGk=".into(),
            },
        ])]);
        let assign =
            build_devin_assign_model_request(&request, "t", "jwt", "fusion-a-sidekick-b", &ids());
        let prompt = assign.chat_message_prompt.expect("prompt");
        assert_eq!(prompt.prompt, "look");
        assert_eq!(prompt.images.len(), 1);
    }

    #[test]
    fn assign_model_request_no_user_gives_none() {
        let request = req(vec![ConversationItem::tool_result("t1", "x")]);
        let assign =
            build_devin_assign_model_request(&request, "t", "jwt", "fusion-a-sidekick-b", &ids());
        assert!(assign.chat_message_prompt.is_none());

        let synthetic_only = req(vec![ConversationItem::user_meta("synthetic marker")]);
        let assign = build_devin_assign_model_request(
            &synthetic_only,
            "t",
            "jwt",
            "fusion-a-sidekick-b",
            &ids(),
        );
        assert!(
            assign.chat_message_prompt.is_none(),
            "synthetic user skipped"
        );
    }

    #[test]
    fn assign_model_request_skips_synthetic_tail_user() {
        let request = req(vec![
            ConversationItem::user("Fusion smoke"),
            ConversationItem::user_meta("synthetic marker"),
            ConversationItem::tool_result("t1", "x"),
        ]);
        let assign =
            build_devin_assign_model_request(&request, "t", "jwt", "fusion-a-sidekick-b", &ids());
        assert_eq!(
            assign.chat_message_prompt.expect("prompt").prompt,
            "Fusion smoke",
            "the last REAL user message wins over synthetic tails"
        );
    }

    #[test]
    fn assign_model_request_encodes_exact_wire_tags() {
        let request = req(vec![ConversationItem::user("p")]);
        let assign = build_devin_assign_model_request(&request, "t", "jwt", "r", &ids());
        let wire = AssignModelRequest {
            metadata: None,
            model_router_uid: "r".to_string(),
            cascade_id: "c".to_string(),
            ..assign
        }
        .encode_to_vec();
        assert_eq!(
            wire,
            vec![
                0x12, 0x01, 0x72, 0x1a, 0x01, 0x63, 0x2a, 0x05, 0x10, 0x01, 0x1a, 0x01, 0x70
            ],
            "tags 2/3/5 + nested prompt source(2)/prompt(3)"
        );
    }

    #[test]
    fn plain_chat_request_has_no_assignment_jwt() {
        let request = req(vec![ConversationItem::user("hi")]);
        let wire =
            build_devin_chat_request(&request, "t", "jwt", "MODEL_X", &ids()).expect("build");
        assert!(wire.model_assignment_jwt.is_none());
    }

    fn tool_delta(id: &str, name: &str, args: &str) -> ChatToolCall {
        ChatToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments_json: args.to_string(),
            ..Default::default()
        }
    }

    fn tool_use_ids(events: &[messages::MessageStreamEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                messages::MessageStreamEvent::ContentBlockStart {
                    content_block: messages::ContentBlock::ToolUse { id, name, .. },
                    ..
                } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn translator_idless_continuation_targets_active_tool() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f1 = resp();
        f1.delta_tool_calls = vec![tool_delta("call_1", "bash", "{\"cmd\":\"ls\"}")];
        t.push_response(&f1).unwrap();
        let mut f2 = resp();
        f2.delta_tool_calls = vec![tool_delta("call_2", "read", "{\"path\":")];
        t.push_response(&f2).unwrap();
        let mut f3 = resp();
        f3.delta_tool_calls = vec![tool_delta("", "", "\"f.txt\"}")];
        t.push_response(&f3).unwrap();
        let mut f4 = resp();
        f4.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f4).unwrap();
        let events = t.finish_success().expect("finish");
        assert_eq!(
            tool_use_ids(&events),
            vec![
                ("call_1".to_string(), "bash".to_string()),
                ("call_2".to_string(), "read".to_string()),
            ],
            "id-less continuation lands on call_2 even with two calls open"
        );
        let args: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                messages::MessageStreamEvent::ContentBlockDelta {
                    delta: messages::StreamDelta::InputJsonDelta { partial_json },
                    ..
                } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, vec!["{\"cmd\":\"ls\"}", "{\"path\":\"f.txt\"}"]);
    }

    #[test]
    fn translator_late_id_adopts_anonymous_block() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f1 = resp();
        f1.delta_tool_calls = vec![tool_delta("", "", "{\"cmd\":\"ls\"}")];
        t.push_response(&f1).unwrap();
        let mut f2 = resp();
        f2.delta_tool_calls = vec![tool_delta("c9", "bash", "")];
        t.push_response(&f2).unwrap();
        let mut f3 = resp();
        f3.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f3).unwrap();
        let events = t.finish_success().expect("finish");
        assert_eq!(
            tool_use_ids(&events),
            vec![("c9".to_string(), "bash".to_string())],
            "late id + name adopt the anonymous block"
        );
    }

    #[test]
    fn translator_idless_incompatible_name_starts_new_call() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f1 = resp();
        f1.delta_tool_calls = vec![tool_delta("call_1", "bash", "{\"cmd\":\"ls\"}")];
        t.push_response(&f1).unwrap();
        let mut f2 = resp();
        f2.delta_tool_calls = vec![tool_delta("", "read", "{\"path\":\"f\"}")];
        t.push_response(&f2).unwrap();
        let mut f3 = resp();
        f3.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f3).unwrap();
        let events = t.finish_success().expect("finish");
        let ids = tool_use_ids(&events);
        assert_eq!(ids.len(), 2, "parallel id-less call kept separate");
        assert_eq!(ids[0], ("call_1".to_string(), "bash".to_string()));
        assert_eq!(ids[1].1, "read");
        assert!(ids[1].0.starts_with("devin_call_"));
    }

    #[test]
    fn translator_invalid_json_flags_do_not_fail_early() {
        let mut t = DevinEventTranslator::new("M".to_string());
        let mut f = resp();
        f.delta_tool_calls = vec![ChatToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments_json: "{}".into(),
            invalid_json_str: "nope".into(),
            invalid_json_err: "server-side complain".into(),
            is_custom_tool_call: true,
        }];
        t.push_response(&f).unwrap();
        let mut f2 = resp();
        f2.stop_reason = stop_reason::FUNCTION_CALL;
        t.push_response(&f2).unwrap();
        t.finish_success().expect("invalid/custom flags tolerated");
    }

    #[test]
    fn translator_tool_errors_are_not_retryable() {
        for err in [
            DevinWireError::Unsupported("x"),
            DevinWireError::MalformedToolCall("x"),
        ] {
            let se = err.into_sampling_error();
            assert!(!se.is_retryable(), "{se:?} must be non-retryable");
        }
    }
}
