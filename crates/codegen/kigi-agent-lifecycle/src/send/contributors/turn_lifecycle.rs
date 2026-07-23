use async_trait::async_trait;

pub struct TurnStartInput {
    /// True when the harness produced the turn (auto-wake, drain, cron, continuation), not the user.
    pub synthetic: bool,
}

impl TurnStartInput {
    pub fn new(synthetic: bool) -> Self {
        TurnStartInput { synthetic }
    }
}

pub struct TurnDoneInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAbortReason {
    /// The client went away mid-turn.
    Disconnected,
    /// The user cancelled mid-turn.
    Interrupted,
}

pub struct TurnAbortInput {
    pub reason: TurnAbortReason,
}

impl TurnAbortInput {
    pub fn new(reason: TurnAbortReason) -> Self {
        TurnAbortInput { reason }
    }
}

pub struct TurnErrorInput<'a> {
    pub message: &'a str,
}

#[async_trait]
pub trait TurnLifecycleContributor: Send + Sync {
    async fn on_turn_start(&self, _input: &TurnStartInput) {}

    async fn on_turn_done(&self, _input: &TurnDoneInput) {}

    async fn on_turn_abort(&self, _input: &TurnAbortInput) {}

    async fn on_turn_error(&self, _input: &TurnErrorInput<'_>) {}
}
