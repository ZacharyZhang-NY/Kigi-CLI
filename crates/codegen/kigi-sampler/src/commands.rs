//! Internal actor protocol.
//!
//! `SamplerCommand` is `pub(crate)` because it is the wire between
//! [`SamplerHandle`](crate::handle::SamplerHandle) and the actor task.
//! External callers always go through `SamplerHandle`.

use tokio::sync::oneshot;

use kigi_sampling_types::{ConversationRequest, ConversationResponse, SamplingError};

use crate::config::SamplerConfig;
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Large payloads (`ConversationRequest`, `SamplerConfig`) are boxed so every
/// variant stays cheap to move through the mpsc channel.
pub(crate) enum SamplerCommand {
    /// Fire-and-forget — results arrive as events. When `completion_tx` is set
    /// the per-request task also signals that channel, for
    /// `submit_and_collect` callers.
    Submit {
        request_id: RequestId,
        request: Box<ConversationRequest>,
        config: Option<Box<SamplerConfig>>,
        completion_tx: Option<
            oneshot::Sender<Result<(ConversationResponse, InferenceLatencyStats), SamplingError>>,
        >,
    },

    /// Cancel an in-flight request.
    Cancel { request_id: RequestId },

    /// Sent on a model switch or an auth refresh.
    UpdateConfig { config: Box<SamplerConfig> },

    IsActive {
        request_id: RequestId,
        reply: oneshot::Sender<bool>,
    },

    /// Query: how many requests are in flight?
    ActiveCount { reply: oneshot::Sender<usize> },
}
