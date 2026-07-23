pub mod conversation;
pub mod error;
pub mod types;

pub use self::conversation::*;
pub use self::error::{ResponseModelMetadata, Result, SamplingError};
pub use self::types::*;
pub use kigi_sampler::ApiBackend;
pub use kigi_sampler::SamplingClient as Client;

pub use async_openai::types::responses as rs;

// The streaming / retry / HTTP-client logic lives in `kigi-sampler`; these
// re-exports keep `crate::sampling::{SamplerHandle, SamplerConfig, ...}` paths
// working for callers not yet ported to `kigi_sampler::*` directly.
pub use kigi_sampler::{
    InferenceLatencyStats, OriginClientInfo, RequestId, SamplerActor, SamplerConfig, SamplerHandle,
    SamplingChannel, SamplingClient, SamplingErrorInfo, SamplingErrorKind, SamplingEvent,
};
