pub mod conversation;
pub mod error;
pub mod types;

// `Client` is the legacy alias used throughout the shell. A later refactor
// retired the bespoke shell HTTP client and points `Client` at the sampler crate's
// `SamplingClient` -- the two have identical method sets, so call-sites
// compile unchanged.
pub use self::conversation::*;
pub use self::error::{ResponseModelMetadata, Result, SamplingError};
pub use self::types::*;
pub use kigi_sampler::ApiBackend;
pub use kigi_sampler::SamplingClient as Client;

// Re-export async-openai Responses API types under `rs` namespace
pub use async_openai::types::responses as rs;

// ---------------------------------------------------------------------------
// kigi-sampler re-exports
// ---------------------------------------------------------------------------
//
// The actual streaming / retry / HTTP-client logic lives in the
// `kigi-sampler` crate. We re-export the public surface here so
// `crate::sampling::{SamplerHandle, SamplerConfig, ...}` paths keep working
// for callers that haven't been ported to spell these directly via
// `kigi_sampler::*`. The shell-side `sampling::client::Config`
// composite was removed when its only remaining role -- session-snapshot
// state for `MvpAgent` -- was migrated to `RefCell<SamplerConfig>` directly.
pub use kigi_sampler::{
    InferenceLatencyStats, OriginClientInfo, RequestId, SamplerActor, SamplerConfig, SamplerHandle,
    SamplingChannel, SamplingClient, SamplingErrorInfo, SamplingErrorKind, SamplingEvent,
};
