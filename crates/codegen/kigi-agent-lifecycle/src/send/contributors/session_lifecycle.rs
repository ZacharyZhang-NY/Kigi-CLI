use async_trait::async_trait;

pub struct SessionIdleInput;

#[async_trait]
pub trait SessionLifecycleContributor: Send + Sync {
    /// Idle means no running turn and no queued work; the host owns that check.
    async fn on_session_idle(&self, _input: &SessionIdleInput) {}
}
