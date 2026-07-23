use async_trait::async_trait;

pub struct TurnInputContext {
    pub turn_id: String,
    /// True when the harness produced the turn (auto-wake, drain, cron, continuation), not the user.
    pub synthetic: bool,
}

/// Raw fragment text: the host owns wrapping, origin stamping, and placement.
pub struct TurnInputFragment {
    pub text: String,
}

/// Fragments land in the turn the host is already sampling, never a new one.
#[async_trait]
pub trait TurnInputContributor: Send + Sync {
    async fn contribute_turn_input(&self, _input: &TurnInputContext) -> Vec<TurnInputFragment> {
        Vec::new()
    }
}
