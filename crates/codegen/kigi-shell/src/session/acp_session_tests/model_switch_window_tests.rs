//! `handle_set_session_model` reports the session's effective context
//! window so clients repaint their header without waiting for a turn.
use super::support::*;

#[tokio::test(flavor = "current_thread")]
async fn set_session_model_returns_the_effective_context_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = build_actor().await;
            let cfg = kigi_sampler::SamplerConfig {
                model: "narrow".to_string(),
                context_window: 128_000,
                ..Default::default()
            };
            let (model_id, window) = actor
                .handle_set_session_model(cfg, None, false, false, true, 85)
                .await
                .expect("model switch");
            assert_eq!(model_id.0.as_ref(), "narrow");
            assert_eq!(window.get(), 128_000);
        })
        .await;
}
