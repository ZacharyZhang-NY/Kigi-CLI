//! Local feedback data types backing the feedback records persisted in the
//! session store and the heuristics that decide when to solicit feedback. The
//! only network surface is the Kimi Code `POST {base}/feedback` call in
//! [`crate::agent::feedback_client`], which sends a small flat JSON body.

use serde::{Deserialize, Deserializer, Serialize};

pub use kigi_shared::session::FeedbackTerminalInfo;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    #[default]
    Agent,
    Tui,
    Web,
    Extension,
    /// Remote workspace / hosted agent client.
    Nebula,
    Desktop,
}

impl std::fmt::Display for ClientType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientType::Agent => write!(f, "agent"),
            ClientType::Tui => write!(f, "tui"),
            ClientType::Web => write!(f, "web"),
            ClientType::Extension => write!(f, "extension"),
            ClientType::Nebula => write!(f, "nebula"),
            ClientType::Desktop => write!(f, "desktop"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackType {
    #[default]
    Rating,
    Text,
    RatingWithText,
    ModelPreference,
    BugReport,
    FeatureRequest,
}

impl std::fmt::Display for FeedbackType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackType::Rating => write!(f, "rating"),
            FeedbackType::Text => write!(f, "text"),
            FeedbackType::RatingWithText => write!(f, "rating_with_text"),
            FeedbackType::ModelPreference => write!(f, "model_preference"),
            FeedbackType::BugReport => write!(f, "bug_report"),
            FeedbackType::FeatureRequest => write!(f, "feature_request"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatingType {
    Thumbs,
    Stars,
    Nps,
}

impl std::fmt::Display for RatingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RatingType::Thumbs => write!(f, "thumbs"),
            RatingType::Stars => write!(f, "stars"),
            RatingType::Nps => write!(f, "nps"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextType {
    Message,
    Session,
    Feature,
    ToolUse,
    General,
}

impl std::fmt::Display for ContextType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextType::Message => write!(f, "message"),
            ContextType::Session => write!(f, "session"),
            ContextType::Feature => write!(f, "feature"),
            ContextType::ToolUse => write!(f, "tool_use"),
            ContextType::General => write!(f, "general"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackMode {
    Thumbs,
    Stars,
    Text,
    ThumbsText,
    StarsText,
    Comparison,
    Survey,
    Nps,
    NpsText,
}

impl std::fmt::Display for FeedbackMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackMode::Thumbs => write!(f, "thumbs"),
            FeedbackMode::Stars => write!(f, "stars"),
            FeedbackMode::Text => write!(f, "text"),
            FeedbackMode::ThumbsText => write!(f, "thumbs_text"),
            FeedbackMode::StarsText => write!(f, "stars_text"),
            FeedbackMode::Comparison => write!(f, "comparison"),
            FeedbackMode::Survey => write!(f, "survey"),
            FeedbackMode::Nps => write!(f, "nps"),
            FeedbackMode::NpsText => write!(f, "nps_text"),
        }
    }
}

pub fn parse_feedback_mode_str(s: &str) -> FeedbackMode {
    match s {
        "thumbs" => FeedbackMode::Thumbs,
        "stars" => FeedbackMode::Stars,
        "text" => FeedbackMode::Text,
        "thumbs_text" => FeedbackMode::ThumbsText,
        "stars_text" => FeedbackMode::StarsText,
        "comparison" => FeedbackMode::Comparison,
        "survey" => FeedbackMode::Survey,
        "nps" => FeedbackMode::Nps,
        "nps_text" => FeedbackMode::NpsText,
        _ => FeedbackMode::Thumbs,
    }
}

/// Allowed `feedback_type` + value-field combinations. Construct submissions
/// via [`FeedbackSubmission::with_content`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FeedbackContent {
    Rating {
        rating_type: RatingType,
        rating_value: i32,
    },
    Text(String),
    RatingWithText {
        rating_type: RatingType,
        rating_value: i32,
        text: String,
    },
}

impl FeedbackContent {
    fn apply_to(self, s: &mut FeedbackSubmission) {
        s.rating_type = None;
        s.rating_value = None;
        s.feedback_text = None;
        match self {
            Self::Rating {
                rating_type,
                rating_value,
            } => {
                s.feedback_type = FeedbackType::Rating;
                s.rating_type = Some(rating_type);
                s.rating_value = Some(rating_value);
            }
            Self::Text(text) => {
                s.feedback_type = FeedbackType::Text;
                s.feedback_text = Some(text);
            }
            Self::RatingWithText {
                rating_type,
                rating_value,
                text,
            } => {
                s.feedback_type = FeedbackType::RatingWithText;
                s.rating_type = Some(rating_type);
                s.rating_value = Some(rating_value);
                s.feedback_text = Some(text);
            }
        }
    }
}

fn empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.is_empty()))
}

/// A user feedback record. Persisted locally in the session store; the text
/// content is forwarded to the Kimi Code feedback endpoint for subscription
/// sessions. Construct via [`FeedbackSubmission::with_content`]; the `Default`
/// impl exists for builder-style construction and test fixtures and does not
/// produce a valid submission on its own (empty `session_id`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSubmission {
    pub session_id: String,

    pub client_type: ClientType,

    pub feedback_type: FeedbackType,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_number: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_type: Option<RatingType>,

    /// Rating value (interpretation depends on rating_type)
    /// - thumbs: -1 (down), 0 (neutral), 1 (up)
    /// - stars: 1-5
    /// - nps: 0-10
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_value: Option<i32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_text: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback_categories: Vec<String>,

    /// Model ID used for the response being rated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,

    /// Server-resolved model ID from the actual chat completion response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_model_id: Option<String>,

    /// Checkpoint fingerprint from the inference provider (`system_fingerprint`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_string_as_none"
    )]
    pub model_fingerprint: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_type: Option<ContextType>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "last_user_turn"
    )]
    pub last_user_message: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "last_assistant_turn"
    )]
    pub last_assistant_message: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_outcomes: Vec<FeedbackToolOutcome>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cwd: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_count: Option<i64>,

    /// Context window usage percentage (0–100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_usage: Option<u8>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens_used: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_info: Option<FeedbackTerminalInfo>,
}

impl FeedbackSubmission {
    /// Construct from typed content; set optional fields after.
    pub fn with_content(
        session_id: String,
        client_type: ClientType,
        content: FeedbackContent,
    ) -> Self {
        let mut s = Self {
            session_id,
            client_type,
            ..Default::default()
        };
        content.apply_to(&mut s);
        s
    }

    /// Merge a JSON object into `metadata`, inserting if absent.
    pub fn merge_metadata(&mut self, extra: serde_json::Value) {
        match &mut self.metadata {
            Some(existing) if existing.is_object() => {
                if let (Some(dst), Some(src)) = (existing.as_object_mut(), extra.as_object()) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
            }
            _ => {
                self.metadata = Some(extra);
            }
        }
    }
}

/// Per-tool call/failure counts for a single tool in a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackToolOutcome {
    pub tool_name: String,
    pub calls: u32,
    pub failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TierConfig {
    pub enabled: bool,
    /// Sample rate (0.0 to 1.0, e.g., 0.0005 = 0.05%)
    pub sample_rate: f64,
    pub min_turns: i64,
    #[serde(default)]
    pub min_tool_calls: i64,
    #[serde(default)]
    pub min_compactions: i64,
    #[serde(default)]
    pub min_errors: i64,
    #[serde(default)]
    pub no_cancellations: bool,
    #[serde(default)]
    pub requires_cancellation: bool,
    #[serde(default)]
    pub requires_revert: bool,
    #[serde(default)]
    pub requires_recovery: bool,
    pub feedback_mode: FeedbackMode,
    #[serde(default = "default_true")]
    pub dismissible: bool,
    #[serde(default)]
    pub prompt: String,
    #[serde(default = "default_one")]
    pub max_triggers: i32,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 0.0005,
            min_turns: 10,
            min_tool_calls: 5,
            min_compactions: 2,
            min_errors: 0,
            no_cancellations: false,
            requires_cancellation: false,
            requires_revert: false,
            requires_recovery: false,
            feedback_mode: FeedbackMode::Thumbs,
            dismissible: true,
            prompt: String::new(),
            max_triggers: 1,
        }
    }
}

/// Configuration for feedback heuristics. The built-in [`Default`] is the only
/// production source; the struct exists so tests and future config surfaces can
/// tune the tiers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FeedbackHeuristicsConfig {
    pub config_id: String,
    pub config_version: i64,

    pub enabled: bool,
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: i64,
    #[serde(default = "default_max_requests")]
    pub max_requests_per_session: i64,

    #[serde(default = "default_true")]
    pub tier1_enabled: bool,
    #[serde(default = "default_tier1_sample_rate")]
    pub tier1_sample_rate: f64,
    #[serde(default = "default_tier1_min_turns")]
    pub tier1_min_turns: i64,
    #[serde(default = "default_tier1_min_tool_calls")]
    pub tier1_min_tool_calls: i64,
    #[serde(default = "default_tier1_min_compactions")]
    pub tier1_min_compactions: i64,
    #[serde(default = "default_true")]
    pub tier1_no_cancellations: bool,
    #[serde(default = "default_feedback_mode_thumbs")]
    pub tier1_feedback_mode: String,
    #[serde(default = "default_true")]
    pub tier1_dismissible: bool,
    #[serde(default = "default_tier1_prompt")]
    pub tier1_prompt: String,
    #[serde(default = "default_one")]
    pub tier1_max_triggers: i32,

    #[serde(default = "default_true")]
    pub tier2_enabled: bool,
    #[serde(default = "default_tier2_sample_rate")]
    pub tier2_sample_rate: f64,
    #[serde(default = "default_tier2_min_turns")]
    pub tier2_min_turns: i64,
    #[serde(default = "default_tier2_min_tool_calls")]
    pub tier2_min_tool_calls: i64,
    #[serde(default = "default_tier2_min_compactions")]
    pub tier2_min_compactions: i64,
    #[serde(default = "default_tier2_min_errors")]
    pub tier2_min_errors: i64,
    #[serde(default = "default_feedback_mode_thumbs_text")]
    pub tier2_feedback_mode: String,
    #[serde(default = "default_true")]
    pub tier2_dismissible: bool,
    #[serde(default = "default_tier2_prompt")]
    pub tier2_prompt: String,
    #[serde(default = "default_one")]
    pub tier2_max_triggers: i32,

    #[serde(default = "default_true")]
    pub tier3_enabled: bool,
    #[serde(default = "default_tier3_sample_rate")]
    pub tier3_sample_rate: f64,
    #[serde(default = "default_tier3_min_turns")]
    pub tier3_min_turns: i64,
    #[serde(default)]
    pub tier3_requires_cancellation: bool,
    #[serde(default)]
    pub tier3_requires_revert: bool,
    #[serde(default = "default_true")]
    pub tier3_requires_recovery: bool,
    #[serde(default = "default_feedback_mode_stars_text")]
    pub tier3_feedback_mode: String,
    #[serde(default = "default_true")]
    pub tier3_dismissible: bool,
    #[serde(default = "default_tier3_prompt")]
    pub tier3_prompt: String,
    #[serde(default = "default_one")]
    pub tier3_max_triggers: i32,
}

impl Default for FeedbackHeuristicsConfig {
    fn default() -> Self {
        Self {
            config_id: "default".to_string(),
            config_version: 1,
            enabled: true,
            cooldown_seconds: 300,
            max_requests_per_session: 3,
            tier1_enabled: true,
            tier1_sample_rate: 0.0005,
            tier1_min_turns: 10,
            tier1_min_tool_calls: 5,
            tier1_min_compactions: 2,
            tier1_no_cancellations: true,
            tier1_feedback_mode: "thumbs".to_string(),
            tier1_dismissible: true,
            tier1_prompt: default_tier1_prompt(),
            tier1_max_triggers: 1,
            tier2_enabled: true,
            tier2_sample_rate: 0.0002,
            tier2_min_turns: 15,
            tier2_min_tool_calls: 10,
            tier2_min_compactions: 3,
            tier2_min_errors: 1,
            tier2_feedback_mode: "thumbs_text".to_string(),
            tier2_dismissible: true,
            tier2_prompt: default_tier2_prompt(),
            tier2_max_triggers: 1,
            tier3_enabled: true,
            tier3_sample_rate: 0.0001,
            tier3_min_turns: 20,
            tier3_requires_cancellation: false,
            tier3_requires_revert: false,
            tier3_requires_recovery: true,
            tier3_feedback_mode: "stars_text".to_string(),
            tier3_dismissible: true,
            tier3_prompt: default_tier3_prompt(),
            tier3_max_triggers: 1,
        }
    }
}

impl FeedbackHeuristicsConfig {
    pub fn tier1_config(&self) -> TierConfig {
        TierConfig {
            enabled: self.tier1_enabled,
            sample_rate: self.tier1_sample_rate,
            min_turns: self.tier1_min_turns,
            min_tool_calls: self.tier1_min_tool_calls,
            min_compactions: self.tier1_min_compactions,
            min_errors: 0,
            no_cancellations: self.tier1_no_cancellations,
            requires_cancellation: false,
            requires_revert: false,
            requires_recovery: false,
            feedback_mode: parse_feedback_mode_str(&self.tier1_feedback_mode),
            dismissible: self.tier1_dismissible,
            prompt: self.tier1_prompt.clone(),
            max_triggers: self.tier1_max_triggers,
        }
    }

    pub fn tier2_config(&self) -> TierConfig {
        TierConfig {
            enabled: self.tier2_enabled,
            sample_rate: self.tier2_sample_rate,
            min_turns: self.tier2_min_turns,
            min_tool_calls: self.tier2_min_tool_calls,
            min_compactions: self.tier2_min_compactions,
            min_errors: self.tier2_min_errors,
            no_cancellations: false,
            requires_cancellation: false,
            requires_revert: false,
            requires_recovery: false,
            feedback_mode: parse_feedback_mode_str(&self.tier2_feedback_mode),
            dismissible: self.tier2_dismissible,
            prompt: self.tier2_prompt.clone(),
            max_triggers: self.tier2_max_triggers,
        }
    }

    pub fn tier3_config(&self) -> TierConfig {
        TierConfig {
            enabled: self.tier3_enabled,
            sample_rate: self.tier3_sample_rate,
            min_turns: self.tier3_min_turns,
            min_tool_calls: 0,
            min_compactions: 0,
            min_errors: 0,
            no_cancellations: false,
            requires_cancellation: self.tier3_requires_cancellation,
            requires_revert: self.tier3_requires_revert,
            requires_recovery: self.tier3_requires_recovery,
            feedback_mode: parse_feedback_mode_str(&self.tier3_feedback_mode),
            dismissible: self.tier3_dismissible,
            prompt: self.tier3_prompt.clone(),
            max_triggers: self.tier3_max_triggers,
        }
    }
}

// Default value functions for serde
fn default_true() -> bool {
    true
}
fn default_cooldown_seconds() -> i64 {
    300
}
fn default_max_requests() -> i64 {
    3
}
fn default_tier1_sample_rate() -> f64 {
    0.0005
}
fn default_tier1_min_turns() -> i64 {
    10
}
fn default_tier1_min_tool_calls() -> i64 {
    5
}
fn default_tier1_min_compactions() -> i64 {
    2
}
fn default_tier2_sample_rate() -> f64 {
    0.0002
}
fn default_tier2_min_turns() -> i64 {
    15
}
fn default_tier2_min_tool_calls() -> i64 {
    10
}
fn default_tier2_min_compactions() -> i64 {
    3
}
fn default_tier2_min_errors() -> i64 {
    1
}
fn default_tier3_sample_rate() -> f64 {
    0.0001
}
fn default_tier3_min_turns() -> i64 {
    20
}
fn default_feedback_mode_thumbs() -> String {
    "thumbs".to_string()
}
fn default_feedback_mode_thumbs_text() -> String {
    "thumbs_text".to_string()
}
fn default_feedback_mode_stars_text() -> String {
    "stars_text".to_string()
}
fn default_tier1_prompt() -> String {
    "You've been having a productive session! Would you mind sharing quick feedback?".to_string()
}
fn default_tier2_prompt() -> String {
    "You've worked through a complex session. Your feedback would help us improve.".to_string()
}
fn default_tier3_prompt() -> String {
    "Thanks for sticking with us through that session. Got a moment to share feedback?".to_string()
}
fn default_one() -> i32 {
    1
}
