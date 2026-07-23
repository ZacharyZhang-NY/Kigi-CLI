pub mod command;
pub mod http;

use std::time::Duration;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use crate::result::{HookDecision, HttpInfo};

pub struct RunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a str,
}

#[derive(Debug)]
pub enum HookRunnerResult {
    Decision(HookDecision),
    Success,
    /// Callers must fail open on this variant: a broken hook never blocks the
    /// session.
    Failed(String),
}

/// Result, wall-clock duration, and HTTP metadata for scrollback enrichment.
pub type HookRunOutput = (HookRunnerResult, Duration, Option<HttpInfo>);

pub async fn run_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    is_blocking: bool,
) -> HookRunOutput {
    match spec.handler_type.as_str() {
        "command" => {
            let (result, elapsed) =
                command::run_command_hook(spec, envelope, ctx, is_blocking).await;
            (result, elapsed, None)
        }
        "http" => http::run_http_hook(spec, envelope, ctx, is_blocking).await,
        _ => (
            HookRunnerResult::Failed(format!("unsupported handler type '{}'", spec.handler_type)),
            Duration::ZERO,
            None,
        ),
    }
}
