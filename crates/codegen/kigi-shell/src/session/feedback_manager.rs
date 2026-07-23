//! Feedback manager for session-level feedback collection.
//!
//! This manager coordinates:
//! - Signal tracking via SessionSignalsHandle
//! - Heuristics evaluation to determine when to request feedback
//! - Local persistence of every feedback record
//! - Forwarding text feedback to the Kimi Code feedback endpoint for
//!   subscription (OAuth) sessions
//!
//! ## Usage
//! ```ignore
//! // Create the manager when a session starts
//! let manager = FeedbackManager::new(session_id, feedback_client, config);
//!
//! // Get the signals handle to pass around for event tracking
//! let signals = manager.signals_handle();
//!
//! // Track events
//! signals.increment_turn();
//! signals.record_tool_call("read_file");
//!
//! // Check for feedback after each turn
//! if let Some(request) = manager.maybe_request_feedback(None).await {
//!     // Send FeedbackRequest notification to client
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::agent::feedback_client::FeedbackClient;
use crate::session::feedback::{
    FeedbackEvaluation, FeedbackHeuristics, FeedbackRequest, FeedbackTier, TriggerCondition,
};
use crate::session::signals::{SessionSignalsActor, SessionSignalsHandle};

use crate::session::feedback_types::{
    ClientType, FeedbackContent, FeedbackMode, FeedbackSubmission, FeedbackToolOutcome,
};

use crate::session::persistence::{LocalFeedbackEntry, PersistenceMsg, UserFeedbackEntry};

pub(crate) enum SubmitOutcome {
    Submitted,
    /// Persisted locally only: no subscription session, or a rating-only
    /// record with no text content for the Kimi feedback endpoint.
    LocalOnly,
    Failed(anyhow::Error),
}

/// Shell-crate constructor: `with_content` + `shell_version`.
pub(crate) fn new_submission(
    session_id: String,
    client_type: ClientType,
    content: FeedbackContent,
) -> FeedbackSubmission {
    let mut s = FeedbackSubmission::with_content(session_id, client_type, content);
    s.shell_version = Some(kigi_version::VERSION.to_string());
    s
}

/// Pipeline: persist locally → forward text content to the Kimi feedback
/// endpoint (subscription sessions only). Callers merge `KIGI_USER_METADATA`
/// and set `submission.request_id`.
pub(crate) async fn submit_feedback_workflow(
    submission: &mut FeedbackSubmission,
    feedback_client: Option<&FeedbackClient>,
    persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
    solicited: bool,
) -> SubmitOutcome {
    if let Some(tx) = persistence_tx {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: submission.session_id.clone(),
            turn_number: submission.turn_number,
            solicited,
            request_id: submission.request_id.clone(),
            dismissed: false,
            submission: Some(submission.clone()),
        });
        if tx.send(PersistenceMsg::Feedback(entry)).is_err() {
            tracing::warn!(
                session_id = %submission.session_id,
                "feedback persistence channel closed; entry dropped",
            );
        }
    }

    let telemetry_rating_value = submission.rating_value;
    let has_feedback_text = submission
        .feedback_text
        .as_ref()
        .is_some_and(|t| !t.is_empty());
    let appearance_id = submission.request_id.clone();

    // Only text-bearing feedback goes over the wire: the Kimi endpoint takes
    // a `content` string (kimi-cli slash.py parity); ratings stay local.
    let outcome = match (feedback_client, &submission.feedback_text) {
        (Some(client), Some(text)) if !text.is_empty() => {
            let model = submission
                .model_id
                .as_deref()
                .or(submission.resolved_model_id.as_deref());
            match client
                .submit_feedback(&submission.session_id, text, model)
                .await
            {
                Ok(()) => SubmitOutcome::Submitted,
                Err(e) => {
                    tracing::warn!(error = %e, "feedback submission failed");
                    SubmitOutcome::Failed(e)
                }
            }
        }
        _ => SubmitOutcome::LocalOnly,
    };

    {
        let feedback_span = tracing::info_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "responded",
            appearance_id = %appearance_id.as_deref().unwrap_or(""),
            has_feedback_text = has_feedback_text,
            rating = tracing::field::Empty,
            is_solicited = solicited,
        );
        // Record `rating` only for star ratings; text-only feedback has no
        // rating and must not export a fake 0.
        if let Some(rating) = telemetry_rating_value {
            feedback_span.record("rating", rating);
        }
        feedback_span.in_scope(|| {});
    }

    outcome
}

/// Chat-state fields the session actor passes to [`FeedbackManager::submit_text_feedback`].
pub(crate) struct SessionFeedbackData {
    pub model_id: Option<String>,
    pub resolved_model_id: Option<String>,
    pub client_version: Option<String>,
    pub session_cwd: String,
}

/// Feedback feature flags threaded through session spawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct FeedbackFlags {
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct FeedbackManagerConfig {
    /// Interval for the signals actor's periodic bookkeeping tick.
    pub sync_interval: Duration,
    /// Whether user-facing feedback features are enabled (popups, `/feedback`,
    /// ratings). Gated by `KIGI_FEEDBACK_ENABLED`.
    pub feedback_enabled: bool,
    pub client_type: ClientType,
}

impl Default for FeedbackManagerConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(60),
            feedback_enabled: false,
            client_type: ClientType::Agent,
        }
    }
}

/// Manages feedback collection for a single session.
pub struct FeedbackManager {
    session_id: String,
    signals_handle: SessionSignalsHandle,
    heuristics: Arc<RwLock<FeedbackHeuristics>>,
    /// Client for the Kimi Code feedback endpoint (subscription sessions).
    feedback_client: Option<FeedbackClient>,
    config: FeedbackManagerConfig,
}

impl FeedbackManager {
    /// Create a new feedback manager for a session.
    ///
    /// If `feedback_client` is None, submissions stay local but tracking and
    /// heuristics evaluation still work.
    pub fn new(
        session_id: impl Into<String>,
        feedback_client: Option<FeedbackClient>,
        config: FeedbackManagerConfig,
    ) -> Self {
        let (signals_handle, actor) = SessionSignalsActor::with_sync_interval(config.sync_interval);

        tokio::spawn(actor.run());

        let session_id = session_id.into();
        let feedback_client = feedback_client.map(|c| c.with_session_id(session_id.clone()));
        tracing::info!(
            session_id = %session_id,
            feedback_enabled = config.feedback_enabled,
            has_client = feedback_client.is_some(),
            "FeedbackManager initialized"
        );

        Self {
            session_id,
            signals_handle,
            heuristics: Arc::new(RwLock::new(FeedbackHeuristics::new())),
            feedback_client,
            config,
        }
    }

    /// Create a feedback manager without a REST client (local tracking only).
    pub fn local_only(session_id: impl Into<String>) -> Self {
        Self::new(session_id, None, FeedbackManagerConfig::default())
    }

    pub fn signals_handle(&self) -> SessionSignalsHandle {
        self.signals_handle.clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_enabled(&self) -> bool {
        self.config.feedback_enabled
    }

    /// Client for the Kimi Code feedback endpoint, if this is a subscription
    /// session.
    pub fn feedback_client(&self) -> Option<&FeedbackClient> {
        self.feedback_client.as_ref()
    }

    pub fn client_type(&self) -> ClientType {
        self.config.client_type
    }

    /// Build and submit text feedback from the `/feedback` slash command.
    pub(crate) async fn submit_text_feedback(
        &self,
        text: String,
        session_data: SessionFeedbackData,
        persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
    ) -> SubmitOutcome {
        let sh = self.signals_handle();
        let (signals, tool_outcomes) = tokio::join!(sh.snapshot(), sh.last_turn_tool_outcomes());
        let signals = signals.unwrap_or_default();
        let turn_number = signals.turn_count.saturating_sub(1) as i64;
        let tool_outcomes: Vec<FeedbackToolOutcome> = tool_outcomes
            .into_iter()
            .map(|o| FeedbackToolOutcome {
                tool_name: o.tool_name,
                calls: o.successes + o.failures,
                failures: o.failures,
            })
            .collect();

        let mut submission = new_submission(
            self.session_id.clone(),
            self.config.client_type,
            FeedbackContent::Text(text),
        );
        submission.turn_number = Some(turn_number);
        submission.model_id = session_data.model_id;
        submission.resolved_model_id = session_data.resolved_model_id;
        submission.last_user_message = None;
        submission.last_assistant_message = None;
        submission.tool_outcomes = tool_outcomes;
        submission.session_cwd = Some(session_data.session_cwd);
        submission.compaction_count = Some(signals.compaction_count as i64);
        submission.context_window_usage = Some(signals.context_window_usage);
        submission.context_tokens_used = Some(signals.context_tokens_used);
        submission.context_window_tokens = Some(signals.context_window_tokens);
        submission.client_version = session_data.client_version;

        if let Some(user_meta) =
            crate::agent::mvp_agent::parse_json_object_env("KIGI_USER_METADATA")
        {
            submission.merge_metadata(user_meta);
        }

        submit_feedback_workflow(
            &mut submission,
            self.feedback_client.as_ref(),
            persistence_tx,
            // solicited=false: the slash command isn't responding to a request
            false,
        )
        .await
    }

    /// Evaluate heuristics and return a FeedbackRequest if one should be sent.
    ///
    /// Call this after each turn to check if feedback should be requested.
    /// Returns None if:
    /// - No tier criteria are met
    /// - The tier was already triggered this session
    /// - Probabilistic sampling says no
    #[tracing::instrument(name = "feedback.maybe_request_feedback", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn maybe_request_feedback(
        &self,
        prompt_id: Option<String>,
    ) -> Option<FeedbackRequest> {
        if !self.config.feedback_enabled {
            return None;
        }

        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;

        if !heuristics.is_enabled() {
            return None;
        }

        let eval = heuristics.evaluate(&signals);

        if let (true, Some(trigger_condition)) =
            (eval.should_request, eval.trigger_condition.as_ref())
        {
            let tier = trigger_condition.tier;
            let feedback_mode = heuristics.feedback_mode(tier);
            let dismissible = heuristics.dismissible(tier);
            let prompt = heuristics.prompt(tier);
            let request = FeedbackRequest::with_mode(
                self.session_id.clone(),
                trigger_condition.clone(),
                feedback_mode,
                dismissible,
                Some(prompt),
            );
            tracing::info!(
                session_id = %self.session_id,
                tier = ?request.tier,
                trigger_type = %request.trigger_type,
                feedback_mode = ?request.feedback_mode,
                prompt_id = ?prompt_id,
                "Feedback request triggered"
            );

            return Some(request);
        }

        None
    }

    /// Force check heuristics without sampling (for testing).
    pub async fn evaluate_heuristics(&self) -> Option<FeedbackEvaluation> {
        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;
        Some(heuristics.evaluate(&signals))
    }

    /// Force-generate a feedback request for local testing, bypassing all
    /// heuristics, sampling, cooldown, and enabled checks.
    ///
    /// Engineers developing clients can call this via the
    /// `kigi/debug/trigger_feedback` ACP extension method to exercise
    /// the full feedback notification ↔ response flow without needing a
    /// real session that meets tier criteria.
    #[tracing::instrument(name = "feedback.force_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn force_feedback_request(
        &self,
        tier: FeedbackTier,
        mode: FeedbackMode,
    ) -> FeedbackRequest {
        use crate::session::feedback::TriggerSignalSnapshot;

        let condition = TriggerCondition {
            tier,
            condition: "debug/trigger_feedback (manual test trigger)".to_string(),
            signal_snapshot: TriggerSignalSnapshot {
                turn_count: 0,
                tool_calls_count: 0,
                compactions_count: 0,
                errors_count: 0,
                cancellations_count: 0,
                has_reverted: false,
            },
        };

        // Manual/debug triggers are always dismissible regardless of tier config,
        // since they exist for developer testing, not real user feedback collection.
        FeedbackRequest::with_mode(self.session_id.clone(), condition, mode, true, None)
    }

    pub async fn shutdown(&self) {
        self.signals_handle.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_feedback_manager_local_only() {
        let manager = FeedbackManager::local_only("test-session-123");

        let signals = manager.signals_handle();
        for _ in 0..10 {
            signals.increment_turn();
        }
        for _ in 0..5 {
            signals.record_tool_call("read_file");
        }
        for _ in 0..2 {
            signals.record_compaction(10_000);
        }

        // Give time for actor to process
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Check signals were tracked
        let snapshot = signals.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_count, 10);
        assert_eq!(snapshot.tool_call_count, 5);
        assert_eq!(snapshot.compaction_count, 2);

        // Evaluate heuristics - should trigger Tier 1
        let eval = manager.evaluate_heuristics().await.unwrap();
        assert!(eval.trigger_condition.is_some());
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            crate::session::feedback::FeedbackTier::Tier1
        );

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_feedback_manager_disabled() {
        let config = FeedbackManagerConfig {
            feedback_enabled: false,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session", None, config);

        // Even with signals, disabled manager should not request feedback
        let signals = manager.signals_handle();
        for _ in 0..20 {
            signals.increment_turn();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let request = manager.maybe_request_feedback(None).await;
        assert!(request.is_none());

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_shutdown_completes() {
        let manager = FeedbackManager::local_only("test-session-no-queue");

        manager.shutdown().await;

        // Verify signals actor was shut down (snapshot returns None after shutdown)
        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    /// A rating-only submission (no text) must not hit the network even when
    /// no client is configured — the workflow reports LocalOnly.
    #[tokio::test]
    async fn test_rating_only_submission_stays_local() {
        use crate::session::feedback_types::RatingType;

        let mut submission = new_submission(
            "sess-local".into(),
            ClientType::Tui,
            FeedbackContent::Rating {
                rating_type: RatingType::Thumbs,
                rating_value: 1,
            },
        );
        let outcome = submit_feedback_workflow(&mut submission, None, None, false).await;
        assert!(matches!(outcome, SubmitOutcome::LocalOnly));
    }
}
