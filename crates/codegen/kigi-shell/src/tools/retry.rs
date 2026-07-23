//! Retry utilities — re-exported from `kigi-tools`.
//!
//! The canonical implementation now lives in `kigi_tools::retry`.
//! This module re-exports with backward-compatible aliases.

pub use kigi_tools::retry::BackoffConfig as RetryConfig;
pub use kigi_tools::retry::{BackoffConfig, execute_with_backoff};

use std::future::Future;
use std::time::Duration;

/// Wrapper around `execute_with_backoff` fixed to `anyhow::Error`, for
/// callers that don't want to be generic over the error type.
pub async fn execute_with_retry<T, E, EFut, R, RFut>(
    config: &RetryConfig,
    execute: E,
    on_retry: R,
) -> Result<T, anyhow::Error>
where
    E: FnMut() -> EFut,
    EFut: Future<Output = Result<T, anyhow::Error>>,
    R: FnMut(u32, u32, Duration) -> RFut,
    RFut: Future<Output = ()>,
{
    execute_with_backoff(config, execute, on_retry).await
}
