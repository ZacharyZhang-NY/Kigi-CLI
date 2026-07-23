//! `AskUserQuestion` tool.
//!
//! Presents the user with structured questions and option sets. In plan mode
//! it is the interview mechanism the agent uses to clarify requirements before
//! finalizing a plan; outside plan mode it gathers preferences during
//! implementation.
//!
//! Flow: the tool hands a [`UserQuestionRequest`] to the session-owned
//! coordinator in `kigi-shell` over an mpsc channel, emits a
//! `UserQuestionAsked` notification for observers, then blocks on a oneshot
//! until the coordinator's ACP `ext_method` round-trip with the client
//! resolves (or the wait budget elapses).
//!
//! In plan mode the client offers two extra actions — "Chat about this"
//! (partial answers, agent reformulates) and "Skip interview" (agent proceeds
//! with what it has). Those only change the tool-result text; the tool itself
//! behaves identically in and out of plan mode.

pub mod format;
pub mod types;

pub use types::{
    AskUserQuestionExtRequest, AskUserQuestionExtResponse, AskUserQuestionMode, QuestionAnnotation,
    UserQuestionError, UserQuestionRequest, UserQuestionResponse, UserQuestionResult,
    UserQuestionSender,
};

use crate::notification::types::UserQuestionAsked;
use crate::types::output::AskUserQuestionOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{NotificationHandle, SharedResources};
use crate::types::tool::{ToolKind, ToolNamespace};

/// TODO: set to `false` and drop [`AskUserQuestionTool::fallback_fire_and_forget`]
/// once the shell coordinator (TS-03) is wired up. While `true`, a missing
/// `UserQuestionSender` degrades to fire-and-forget `QuestionsSent` instead of
/// failing the tool call.
const MIGRATION_FALLBACK: bool = true;

/// Default wait budget for one questionnaire. On expiry the tool returns the
/// same text as a user dismiss (`CANCEL_TEXT`), not a tool failure.
pub const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Default for `timeout_enabled` across every resolver tier and settings
/// surface. Single source — the shell resolver's `.default(...)` and the
/// pager's settings registry both anchor on this const.
pub const DEFAULT_ASK_USER_QUESTION_TIMEOUT_ENABLED: bool = true;

/// Env var overriding [`RESPONSE_TIMEOUT`], in **seconds**.
pub const RESPONSE_TIMEOUT_ENV: &str = "KIGI_ASK_USER_QUESTION_TIMEOUT_SECS";

/// Invalid or non-positive values are warned about and treated as unset.
/// Single source for this parse — the shell's env tier calls it too, so the
/// two resolutions can't drift.
pub fn response_timeout_env_secs() -> Option<u64> {
    let raw = std::env::var(RESPONSE_TIMEOUT_ENV).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Some(secs),
        _ => {
            tracing::warn!(
                env = RESPONSE_TIMEOUT_ENV,
                value = %raw,
                "invalid timeout override; ignoring"
            );
            None
        }
    }
}

/// Env override if set, otherwise [`RESPONSE_TIMEOUT`].
pub fn response_timeout() -> std::time::Duration {
    response_timeout_env_secs()
        .map(std::time::Duration::from_secs)
        .unwrap_or(RESPONSE_TIMEOUT)
}

/// Runtime-configurable parameters, injected via
/// `Params<AskUserQuestionParams>` in `SharedResources`.
///
/// The shell resolves `[toolset.ask_user_question]` across requirements >
/// env > user `config.toml` > managed > remote feature config and injects the
/// concrete result. Every field is optional, and `None` means "unset" — that
/// falls back to the env→default budget, so registry consumers that never
/// resolve config (workspace toolset) still get a sane timeout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AskUserQuestionParams {
    /// `Some(false)` disarms the questionnaire timer entirely (wait forever
    /// for an answer). `None`/`Some(true)` keep the timer armed.
    #[serde(default)]
    pub timeout_enabled: Option<bool>,
    /// Positive integer; `None` falls back to [`response_timeout`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

crate::register_resource!("kigi", "AskUserQuestion", AskUserQuestionParams);

impl AskUserQuestionParams {
    /// `Some(duration)` = bounded wait, `None` = wait forever.
    pub fn wait_budget(&self) -> Option<std::time::Duration> {
        if !self
            .timeout_enabled
            .unwrap_or(DEFAULT_ASK_USER_QUESTION_TIMEOUT_ENABLED)
        {
            return None;
        }
        match self.timeout_secs {
            Some(secs) if secs > 0 => Some(std::time::Duration::from_secs(secs)),
            Some(secs) => {
                // 0 must never mean "wait forever" — that is `timeout_enabled`'s job.
                tracing::warn!(
                    value = secs,
                    "ask_user_question timeout_secs must be > 0; using default budget"
                );
                Some(response_timeout())
            }
            None => Some(response_timeout()),
        }
    }
}

/// A single option within a question.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct QuestionOption {
    #[schemars(description = "Option text shown to the user. A few words at most.")]
    pub label: String,

    #[schemars(description = "What picking this option means or implies.")]
    pub description: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Optional content shown while the option is focused — mockups, code snippets, anything the user should compare. Single-select questions only."
    )]
    pub preview: Option<String>,

    /// Opaque id, hidden from the model. Kigi callers leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub id: Option<String>,
}

/// A single question with its options.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    #[schemars(description = "The question to ask, phrased as a full question.")]
    pub question: String,

    #[schemars(description = "The choices for this question.")]
    pub options: Vec<QuestionOption>,

    // The model-facing schema name is snake_case (`multi_select`), but
    // deserialization also accepts `multiSelect` so this shared type stays
    // wire-compatible with the camelCase ACP ext_method.
    #[serde(
        default,
        alias = "multi_select",
        deserialize_with = "crate::types::schema::deserialize_lenient_option_bool"
    )]
    #[schemars(
        rename = "multi_select",
        description = "Let the user pick more than one option (default false)."
    )]
    pub multi_select: Option<bool>,

    /// See `QuestionOption::id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct AskUserQuestionInput {
    #[schemars(description = "The questions to ask, each with its own options.")]
    pub questions: Vec<Question>,

    /// Formats the tool result keyed by option id rather than label. Skipped
    /// on the wire and from the JSON schema so the model never sees or
    /// controls it.
    #[serde(default, skip)]
    #[schemars(skip)]
    pub use_id_keyed_format: bool,
}

/// `run()` blocks until the user responds or the wait budget elapses for the
/// whole questionnaire, and the timeout policy comes from
/// [`AskUserQuestionParams`].
#[derive(Debug, Default)]
pub struct AskUserQuestionTool;

impl crate::types::tool_metadata::ToolMetadata for AskUserQuestionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::AskUser
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Kigi
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["UserQuestionAsked"]
    }

    fn description_template(&self) -> &str {
        r#"Ask the user one or more multiple-choice questions.

- Every question automatically gets an "Other" choice where the user can type their own answer.
- Put your recommended option first and append "(Recommended)" to its label."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        // Standalone. The plan-mode prompt note is
        // `${% if tools.by_kind.exit_plan %}`-guarded, so it renders
        // fine without the plan tools.
        Expr::True
    }
}

impl AskUserQuestionTool {
    /// Fallback for when the shell has not injected a `UserQuestionSender`:
    /// send the notification and return `QuestionsSent` without waiting for an
    /// answer. Gated on [`MIGRATION_FALLBACK`].
    async fn fallback_fire_and_forget(
        &self,
        input: &AskUserQuestionInput,
        ctx: &kigi_tool_runtime::ToolCallContext,
        resources: &SharedResources,
    ) -> Result<AskUserQuestionOutput, kigi_tool_runtime::ToolError> {
        let question_count = input.questions.len();

        let questions_json = serde_json::to_value(&input.questions)
            .unwrap_or_else(|_| serde_json::Value::Array(vec![]));

        {
            let res = resources.lock().await;
            if let Some(handle) = res.get::<NotificationHandle>() {
                handle.0.send_user_question_asked(UserQuestionAsked {
                    tool_call_id: ctx.call_id.as_str().to_owned(),
                    questions_json,
                });
            }
        }

        tracing::info!(question_count, "Asked user questions (fallback path)");

        let question_summary: Vec<String> = input
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let options: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
                format!(
                    "{}. {} [options: {}]",
                    i + 1,
                    q.question,
                    options.join(", ")
                )
            })
            .collect();

        let message = format!(
            "Your questions have been presented to the user for answering:\n{}",
            question_summary.join("\n")
        );

        Ok(AskUserQuestionOutput::QuestionsSent {
            message,
            question_count,
        })
    }
}

impl kigi_tool_runtime::Tool for AskUserQuestionTool {
    type Args = AskUserQuestionInput;
    type Output = AskUserQuestionOutput;

    fn id(&self) -> kigi_tool_protocol::ToolId {
        kigi_tool_protocol::ToolId::new("ask_user_question").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::kigi_tool_runtime::ListToolsContext,
    ) -> kigi_tool_types::ToolDescription {
        kigi_tool_types::ToolDescription::new(
            "ask_user_question",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> kigi_tool_protocol::ToolCapabilities {
        kigi_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(kigi_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.ask_user_question",
        skip_all,
        fields(question_count = input.questions.len()),
    )]
    async fn run(
        &self,
        ctx: kigi_tool_runtime::ToolCallContext,
        input: AskUserQuestionInput,
    ) -> Result<AskUserQuestionOutput, kigi_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let question_count = input.questions.len();

        if question_count == 0 {
            return Ok(AskUserQuestionOutput::QuestionsSent {
                message: "No questions provided. Continue with the task.".to_string(),
                question_count: 0,
            });
        }

        {
            let mut seen = std::collections::HashSet::new();
            for q in &input.questions {
                if !seen.insert(&q.question) {
                    return Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                        "Duplicate question text: \"{}\"",
                        q.question
                    )));
                }
            }
        }

        let sender = {
            let res = resources.lock().await;
            res.get::<UserQuestionSender>().cloned()
        };

        let sender = match sender {
            Some(s) => s,
            None => {
                if MIGRATION_FALLBACK {
                    tracing::warn!(
                        "UserQuestionSender not available; falling back to fire-and-forget QuestionsSent. \
                         This is expected during migration (TS-03 not yet wired)."
                    );
                    return self
                        .fallback_fire_and_forget(&input, &ctx, &resources)
                        .await;
                }
                return Err(kigi_tool_runtime::ToolError::custom(
                    "missing_resource",
                    "UserQuestionSender".to_string(),
                ));
            }
        };

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        let request = types::UserQuestionRequest {
            tool_call_id: ctx.call_id.as_str().to_owned(),
            questions: input.questions.clone(),
            result_tx,
        };

        if sender.0.send(request).is_err() {
            return Err(kigi_tool_runtime::ToolError::execution(
                kigi_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                "User question session ended unexpectedly (coordinator channel closed)",
            ));
        }

        let wait = {
            let questions_json = serde_json::to_value(&input.questions)
                .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
            let res = resources.lock().await;
            if let Some(handle) = res.get::<NotificationHandle>() {
                handle.0.send_user_question_asked(UserQuestionAsked {
                    tool_call_id: ctx.call_id.as_str().to_owned(),
                    questions_json,
                });
            }
            res.get::<crate::types::resources::Params<AskUserQuestionParams>>()
                .map(|p| p.0)
                .unwrap_or_default()
                .wait_budget()
        };
        tracing::info!(
            question_count,
            timeout_secs = ?wait.map(|d| d.as_secs()),
            "Asked user questions, blocking for response"
        );

        // One timer covers the whole questionnaire, not each question: N
        // questions in one call share a single wait. A `None` budget
        // (`timeout_enabled = false`) awaits with no timer at all, wrapped in
        // the timed shape so one match arm handles both.
        let outcome = match wait {
            Some(dur) => tokio::time::timeout(dur, result_rx).await,
            None => Ok(result_rx.await),
        };
        let result = match outcome {
            Ok(Ok(r)) => r,
            Ok(Err(_recv_error)) => {
                return Err(kigi_tool_runtime::ToolError::execution(
                    kigi_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                    "User question session ended unexpectedly (client may have disconnected)",
                ));
            }
            Err(_elapsed) => {
                tracing::info!(
                    question_count,
                    timeout_secs = ?wait.map(|d| d.as_secs()),
                    "User question timed out; continuing without answers"
                );
                // Returning drops the oneshot receiver. The shell coordinator
                // races `result_tx.closed()` against ACP, so it unblocks and
                // can open the next questionnaire; stale UI is dismissed when
                // the new ext_method arrives.
                return Ok(AskUserQuestionOutput::UserAnswered {
                    message: format::CANCEL_TEXT.to_string(),
                });
            }
        };

        match result {
            Ok(UserQuestionResponse::Accepted {
                answers,
                annotations,
            }) => {
                let message = if input.use_id_keyed_format {
                    format::format_id_keyed_accepted_tool_result(
                        &input.questions,
                        &answers,
                        &annotations,
                    )
                } else {
                    format::format_accepted_tool_result(&answers, &annotations)
                };
                Ok(AskUserQuestionOutput::UserAnswered { message })
            }
            Ok(UserQuestionResponse::ChatAboutThis {
                questions,
                partial_answers,
            }) => {
                let message = format::format_chat_about_this(&questions, &partial_answers);
                Ok(AskUserQuestionOutput::UserAnswered { message })
            }
            Ok(UserQuestionResponse::SkipInterview {
                questions,
                partial_answers,
            }) => {
                let message = format::format_skip_interview(&questions, &partial_answers);
                Ok(AskUserQuestionOutput::UserAnswered { message })
            }
            Ok(UserQuestionResponse::Cancelled) => Ok(AskUserQuestionOutput::UserAnswered {
                message: format::CANCEL_TEXT.to_string(),
            }),
            Err(UserQuestionError::TransportError(msg)) => {
                Err(kigi_tool_runtime::ToolError::execution(
                    kigi_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                    format!("Failed to reach the client for user question: {msg}"),
                ))
            }
            Err(UserQuestionError::MalformedResponse(msg)) => {
                Err(kigi_tool_runtime::ToolError::execution(
                    kigi_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                    format!("Client returned an invalid response to user question: {msg}"),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx_with_call_id;
    use indexmap::IndexMap;
    use tokio::sync::mpsc;

    fn make_question(question: &str, labels: &[&str]) -> Question {
        Question {
            question: question.to_string(),
            options: labels
                .iter()
                .map(|l| QuestionOption {
                    label: l.to_string(),
                    description: format!("Description for {l}"),
                    preview: None,
                    id: None,
                })
                .collect(),
            multi_select: None,
            id: None,
        }
    }

    fn resources_with_sender() -> (
        SharedResources,
        mpsc::UnboundedReceiver<types::UserQuestionRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut resources = Resources::new();
        resources.insert(UserQuestionSender(tx));
        (resources.into_shared(), rx)
    }

    fn resources_with_sender_and_params(
        params: AskUserQuestionParams,
    ) -> (
        SharedResources,
        mpsc::UnboundedReceiver<types::UserQuestionRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut resources = Resources::new();
        resources.insert(UserQuestionSender(tx));
        resources.insert(crate::types::resources::Params(params));
        (resources.into_shared(), rx)
    }

    #[test]
    fn tool_name_and_description() {
        let tool = AskUserQuestionTool;
        assert_eq!(
            kigi_tool_runtime::Tool::id(&tool).as_str(),
            "ask_user_question"
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("Ask the user"));
        assert!(desc.contains("Other"));
        assert!(desc.contains("(Recommended)"));
    }

    #[test]
    fn tool_is_read_only() {
        assert!(kigi_tool_runtime::Tool::capabilities(&AskUserQuestionTool).is_read_only);
    }

    #[test]
    fn tool_kind_is_ask_user() {
        assert_eq!(
            crate::types::tool_metadata::ToolMetadata::kind(&AskUserQuestionTool),
            ToolKind::AskUser
        );
    }

    #[test]
    fn input_deserializes_from_json() {
        let json = serde_json::json!({
            "questions": [{
                "question": "Pick DB?",
                "options": [
                    {"label": "Postgres", "description": "Relational DB"},
                    {"label": "SQLite", "description": "Embedded SQL database", "preview": "```\nSELECT 1;\n```"}
                ],
                "multiSelect": false
            }]
        });

        let input: AskUserQuestionInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.questions.len(), 1);
        assert_eq!(input.questions[0].question, "Pick DB?");
        assert_eq!(input.questions[0].options.len(), 2);
        assert_eq!(input.questions[0].options[0].label, "Postgres");
        assert!(input.questions[0].options[0].preview.is_none());
        assert_eq!(input.questions[0].options[1].label, "SQLite");
        assert!(input.questions[0].options[1].preview.is_some());
        assert_eq!(input.questions[0].multi_select, Some(false));
    }

    #[test]
    fn model_schema_advertises_snake_case_multi_select() {
        let schema = schemars::schema_for!(AskUserQuestionInput);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(
            json.contains("multi_select"),
            "model schema should advertise multi_select: {json}"
        );
        assert!(
            !json.contains("multiSelect"),
            "model schema should not advertise camelCase multiSelect: {json}"
        );
    }

    #[test]
    fn input_accepts_snake_case_multi_select() {
        let json = serde_json::json!({
            "questions": [{
                "question": "Pick DB?",
                "options": [{"label": "Postgres", "description": "Relational DB"}],
                "multi_select": true
            }]
        });
        let input: AskUserQuestionInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.questions[0].multi_select, Some(true));
    }

    #[tokio::test]
    async fn fallback_ask_single_question() {
        let resources = Resources::new();
        let shared = resources.into_shared();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question(
                "Which database?",
                &["Redis (Recommended)", "Memcached"],
            )],
            use_id_keyed_format: false,
        };

        let result =
            kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "test-call"), input)
                .await
                .unwrap();

        match result {
            AskUserQuestionOutput::QuestionsSent {
                ref message,
                question_count,
            } => {
                assert_eq!(question_count, 1);
                assert!(message.contains("Which database?"));
            }
            _ => panic!("Expected QuestionsSent fallback"),
        }
    }

    #[tokio::test]
    async fn empty_questions_handled() {
        let resources = Resources::new();
        let shared = resources.into_shared();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![],
            use_id_keyed_format: false,
        };

        let result =
            kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "test-call"), input)
                .await
                .unwrap();

        match result {
            AskUserQuestionOutput::QuestionsSent {
                ref message,
                question_count,
            } => {
                assert_eq!(question_count, 0);
                assert!(message.contains("No questions provided"));
            }
            _ => panic!("Expected QuestionsSent for empty"),
        }
    }

    #[tokio::test]
    async fn fallback_sends_notification() {
        use crate::notification::types::{ToolNotification, ToolNotificationHandle};

        let (handle, mut rx) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(NotificationHandle(handle));
        let shared = resources.into_shared();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Pick one?", &["A", "B"])],
            use_id_keyed_format: false,
        };

        kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "call-q"), input)
            .await
            .unwrap();

        let notification = rx.try_recv().expect("should have received a notification");
        match notification {
            ToolNotification::UserQuestionAsked(asked) => {
                assert_eq!(asked.tool_call_id, "call-q");
            }
            other => panic!("Expected UserQuestionAsked, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn validate_duplicate_question_text() {
        let resources = Resources::new();
        let shared = resources.into_shared();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![
                make_question("Same question?", &["A"]),
                make_question("Same question?", &["B"]),
            ],
            use_id_keyed_format: false,
        };

        let err =
            kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "test-call"), input)
                .await
                .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Duplicate question text"), "got: {msg}");
        assert!(msg.contains("Same question?"), "got: {msg}");
    }

    #[tokio::test]
    async fn blocking_round_trip_accepted() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Which database?", &["Redis", "Postgres"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "tc-1"), input)
                    .await
            }
        });

        let request = rx.recv().await.expect("should receive request");
        assert_eq!(request.tool_call_id, "tc-1");
        assert_eq!(request.questions.len(), 1);

        let mut answers = IndexMap::new();
        answers.insert("Which database?".to_string(), vec!["Redis".to_string()]);

        request
            .result_tx
            .send(Ok(UserQuestionResponse::Accepted {
                answers,
                annotations: None,
            }))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert!(message.starts_with("User has answered your questions:"));
                assert!(message.contains("\"Which database?\"=\"Redis\""));
            }
            other => panic!("Expected UserAnswered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn blocking_round_trip_cancelled() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Q?", &["A"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "tc-4"), input)
                    .await
            }
        });

        let request = rx.recv().await.unwrap();
        request
            .result_tx
            .send(Ok(UserQuestionResponse::Cancelled))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert_eq!(message, format::CANCEL_TEXT);
            }
            other => panic!("Expected UserAnswered with cancel text, got {:?}", other),
        }
    }

    /// No `Params` injected, which pins the env→default budget for consumers
    /// that never resolve `[toolset.ask_user_question]`.
    #[tokio::test(start_paused = true)]
    async fn blocking_times_out_after_default_budget_for_batch() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![
                make_question("Q1?", &["A", "B"]),
                make_question("Q2?", &["C", "D"]),
            ],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(
                    &tool,
                    test_ctx_with_call_id(shared, "tc-timeout"),
                    input,
                )
                .await
            }
        });

        let request = rx.recv().await.expect("should receive request");
        assert_eq!(request.questions.len(), 2);
        // The effective budget, so the test still passes under an env override.
        let wait = response_timeout();
        tokio::time::advance(wait + std::time::Duration::from_secs(1)).await;

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert_eq!(message, format::CANCEL_TEXT);
            }
            other => panic!(
                "Expected UserAnswered with skip/cancel text, got {:?}",
                other
            ),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn answer_before_timeout_still_succeeds() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Which database?", &["Redis", "Postgres"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "tc-ok"), input)
                    .await
            }
        });

        let request = rx.recv().await.expect("should receive request");
        // Stay well under the effective budget, whatever an env override made it.
        let advance = response_timeout()
            .checked_div(6)
            .unwrap_or(std::time::Duration::from_secs(1))
            .max(std::time::Duration::from_secs(1));
        tokio::time::advance(advance).await;

        let mut answers = IndexMap::new();
        answers.insert("Which database?".to_string(), vec!["Redis".to_string()]);
        request
            .result_tx
            .send(Ok(UserQuestionResponse::Accepted {
                answers,
                annotations: None,
            }))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert!(message.contains("\"Which database?\"=\"Redis\""));
            }
            other => panic!("Expected UserAnswered, got {:?}", other),
        }
    }

    #[test]
    fn wait_budget_mapping() {
        // Compared against `response_timeout()` rather than the raw constant so
        // the assertions pin the delegation and hold under a dev's env override.
        assert_eq!(
            AskUserQuestionParams::default().wait_budget(),
            Some(response_timeout()),
            "registry-default (all-None) params must keep the legacy budget"
        );
        assert_eq!(
            RESPONSE_TIMEOUT,
            std::time::Duration::from_secs(30 * 60),
            "default ask_user_question budget is 30 minutes"
        );
        let disabled = AskUserQuestionParams {
            timeout_enabled: Some(false),
            timeout_secs: Some(30),
        };
        assert_eq!(disabled.wait_budget(), None, "disabled timer waits forever");
        let zero = AskUserQuestionParams {
            timeout_enabled: Some(true),
            timeout_secs: Some(0),
        };
        assert_eq!(
            zero.wait_budget(),
            Some(response_timeout()),
            "0 secs must fall back to the default, never wait forever"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn short_params_timeout_fires_with_cancel_text() {
        let (shared, mut rx) = resources_with_sender_and_params(AskUserQuestionParams {
            timeout_enabled: Some(true),
            timeout_secs: Some(5),
        });
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Q?", &["A", "B"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(
                    &tool,
                    test_ctx_with_call_id(shared, "tc-short"),
                    input,
                )
                .await
            }
        });

        let _request = rx.recv().await.expect("should receive request");
        tokio::time::advance(std::time::Duration::from_secs(6)).await;

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert_eq!(message, format::CANCEL_TEXT);
            }
            other => panic!("Expected UserAnswered with cancel text, got {:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_disabled_waits_beyond_default_budget() {
        let (shared, mut rx) = resources_with_sender_and_params(AskUserQuestionParams {
            timeout_enabled: Some(false),
            timeout_secs: Some(1),
        });
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Which database?", &["Redis", "Postgres"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(
                    &tool,
                    test_ctx_with_call_id(shared, "tc-forever"),
                    input,
                )
                .await
            }
        });

        let request = rx.recv().await.expect("should receive request");
        // Far past both the default and any env-overridden budget.
        tokio::time::advance(RESPONSE_TIMEOUT.max(response_timeout()) * 4).await;

        let mut answers = IndexMap::new();
        answers.insert("Which database?".to_string(), vec!["Redis".to_string()]);
        request
            .result_tx
            .send(Ok(UserQuestionResponse::Accepted {
                answers,
                annotations: None,
            }))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        match result {
            AskUserQuestionOutput::UserAnswered { message } => {
                assert!(message.contains("\"Which database?\"=\"Redis\""));
            }
            other => panic!("Expected UserAnswered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn channel_drop_returns_error() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Q?", &["A"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "tc-5"), input)
                    .await
            }
        });

        let request = rx.recv().await.unwrap();
        drop(request.result_tx);

        let err = handle.await.unwrap().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unexpectedly"), "msg: {msg}");
    }

    #[tokio::test]
    async fn transport_error_not_cancel() {
        let (shared, mut rx) = resources_with_sender();
        let tool = AskUserQuestionTool;

        let input = AskUserQuestionInput {
            questions: vec![make_question("Q?", &["A"])],
            use_id_keyed_format: false,
        };

        let handle = tokio::spawn({
            let shared = shared.clone();
            async move {
                kigi_tool_runtime::Tool::run(&tool, test_ctx_with_call_id(shared, "tc-6"), input)
                    .await
            }
        });

        let request = rx.recv().await.unwrap();
        request
            .result_tx
            .send(Err(UserQuestionError::TransportError(
                "connection reset".to_string(),
            )))
            .unwrap();

        let err = handle.await.unwrap().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Failed to reach the client"), "msg: {msg}");
        assert!(msg.contains("connection reset"), "msg: {msg}");
    }
}
