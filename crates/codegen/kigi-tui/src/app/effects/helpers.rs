#![cfg_attr(rustfmt, rustfmt::skip)]
use std::path::Path;
use agent_client_protocol as acp;
use tokio::task::JoinSet;
use kigi_acp_lib::{AcpAgentTx, acp_send};
use super::actions::{PermissionModePersist, SubagentKillOutcome, TaskResult};
use super::agent::AgentId;
use crate::unified_log as ulog;
use kigi_shell::sampling::error::{
    RATE_LIMITED_ERROR_CODE, rate_limited_user_message,
};
use kigi_shell::session::ExtMethodResult;
/// Typed progress message for session restore.
/// Keeps the progress channel from accepting arbitrary `TaskResult` variants.
pub(crate) struct RestoreProgressMsg {
    pub agent_id: AgentId,
    pub message: String,
}
pub(super) fn log_prompt_result(
    session_id: &acp::SessionId,
    result: &Result<acp::PromptResponse, acp::Error>,
) {
    let sid = &session_id.0;
    match result {
        Ok(_) => ulog::info("agent response complete", Some(sid), None),
        Err(e) => {
            ulog::error(
                "agent response failed",
                Some(sid),
                Some(serde_json::json!({ "error" : e.to_string() })),
            )
        }
    }
}
/// Upper bound on the off-thread clipboard-attachment probe. A wedged osascript
/// read must not pin `paste_probe_in_flight` and silently stash every later send.
pub(super) const CLIPBOARD_PROBE_TIMEOUT_SECS: u64 = 10;
/// Picker search debounce ([`Effect::DebounceSessionSearch`]):
/// long enough to coalesce a typing burst, short enough to feel live.
pub(super) const SESSION_SEARCH_DEBOUNCE_MS: u64 = 250;
/// Convert an ACP error to a user-friendly string for display.
/// Rate-limit errors get auth-aware copy instead of the raw server error.
/// All other errors are sanitized to remove internal service names and jargon.
pub(super) fn format_acp_error(err: &acp::Error, is_api_key_auth: bool) -> String {
    if i32::from(err.code) == RATE_LIMITED_ERROR_CODE {
        return rate_limited_user_message(is_api_key_auth).into();
    }
    if err.code == acp::ErrorCode::InvalidParams && let Some(data) = &err.data
        && let Some(msg) = kigi_shell::sampling::error::error_detail_from_data(data)
        && !msg.is_empty()
    {
        return msg;
    }
    sanitize_user_error(&err.to_string())
}
/// Format a Duration for user-visible restore progress messages.
pub(super) fn format_restore_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}
/// CANONICAL wire parser for the worktree resume response. Any other code
/// consuming the `codeRestored` / `restoreSummary` / `restoreDegree` shape
/// MUST go through this function — do not re-implement.
pub(super) fn parse_worktree_restore_payload(
    result_obj: &serde_json::Value,
) -> (bool, Option<String>, Option<kigi_workspace::session::git::RestoreDegree>) {
    let code_restored = result_obj
        .get("codeRestored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let restore_summary = result_obj
        .get("restoreSummary")
        .and_then(|v| v.as_str())
        .map(String::from);
    let restore_degree = result_obj
        .get("restoreDegree")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok());
    (code_restored, restore_summary, restore_degree)
}
/// CANONICAL wire parser for `LoadSessionResponse._meta.codeRestore`. Any
/// other code consuming this shape MUST go through this function — do not
/// re-implement.
pub(super) fn parse_session_load_restore_meta(
    resp_meta: Option<&acp::Meta>,
) -> (bool, Option<String>, Option<kigi_workspace::session::git::RestoreDegree>) {
    let code_restore = resp_meta.and_then(|m| m.get("codeRestore"));
    let code_restored = code_restore
        .and_then(|r| r.get("restored"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let restore_summary = code_restore
        .and_then(|r| r.get("summary"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let restore_degree = code_restore
        .and_then(|r| r.get("degree"))
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok());
    (code_restored, restore_summary, restore_degree)
}
/// CANONICAL wire parser for `LoadSessionResponse._meta["x.ai/runningPromptId"]`.
///
/// Returns the session's in-flight running prompt id when the session was
/// loaded MID-turn (some other client is driving), otherwise `None`. The
/// loader adopts this id so subsequent live `session/update` deltas pass the
/// `current_prompt_id` gate (see `app/acp_handler.rs`). `pub(super)` for the
/// reconnect re-init in `event_loop.rs`, which reads the same response meta.
pub(crate) fn parse_session_load_running_prompt_id(
    resp_meta: Option<&acp::Meta>,
) -> Option<String> {
    resp_meta
        .and_then(|m| m.get("x.ai/runningPromptId"))
        .and_then(|v| v.as_str())
        .map(String::from)
}
/// Sanitize an error string before showing it to the user.
///
/// Strips protocol jargon (ACP, JSON-RPC) and other technical noise that would
/// be meaningless in a toast, and collapses known disk-full markers.
pub(crate) fn sanitize_user_error(raw: &str) -> String {
    if raw.contains(kigi_fast_worktree::OUT_OF_DISK_CONTEXT)
        || raw.contains(kigi_fast_worktree::ENOSPC_OS_MESSAGE)
    {
        return "Out of disk space.".to_string();
    }
    static REPLACEMENTS: &[(&str, &str)] = &[
        ("inference-api", "server"),
        ("inference_api", "server"),
        ("research-api", "server"),
        ("research_api", "server"),
        ("grok-code-backend", "server"),
        ("ACP error:", "error:"),
        ("ACP request failed:", "request failed:"),
        ("JSON-RPC error", "request error"),
        ("acp_send", "request"),
        ("ExtRequest", "request"),
        ("ExtNotification", "notification"),
        ("Authentication required: ", ""),
        ("Authentication failed: ", ""),
    ];
    let mut result = raw.to_string();
    for (pattern, replacement) in REPLACEMENTS {
        result = result.replace(pattern, replacement);
    }
    if result.chars().count() > 200 {
        let truncated: String = result.chars().take(180).collect();
        result = format!("{truncated}...");
    }
    result
}
/// Additive session creation flags passed from CLI → AppView → effects.
///
/// The flags map to built-in `BuiltinAgentName` profiles (`agentProfile`)
/// and, independently, gate the `ask_user_question` tool at the builder
/// (`askUserQuestion`). `--no-ask-user` always strips the tool, regardless
/// of which profile was selected.
///
/// The `askUserQuestion` column is the value the pager stamps into `_meta`;
/// `omitted` means the shell resolves the gate itself (default ON).
///
/// | plan  | subagents | ask-user | agentProfile                   | askUserQuestion    |
/// |-------|-----------|----------|--------------------------------|--------------------|
/// | false | false     | false    | `grok-build` (default)         | `false`            |
/// | false | true      | false    | `grok-build` (default)         | `false`            |
/// | false | false     | true     | `grok-build-ask-user`          | omitted (shell gate) |
/// | false | true      | true     | `grok-build-ask-user`          | omitted (shell gate) |
/// | true  | false     | false    | `grok-build-plan-no-subagents` | `false`            |
/// | true  | true      | false    | `grok-build-plan`              | `false`            |
/// | true  | false     | true     | `grok-build-plan-no-subagents` | omitted (shell gate) |
/// | true  | true      | true     | `grok-build-plan`              | omitted (shell gate) |
///
/// When [`Self::chat_mode`] is set (gateway light-frontend / `--chat`), Build
/// `agentProfile` injection is omitted (K12) and `_meta["x.ai/session"].kind`
/// is stamped `"chat"` so the shell takes `require_gateway` / thin profile.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionFlags {
    pub plan_mode: bool,
    pub subagents: bool,
    pub ask_user: bool,
    /// Restore code state on resume (`--restore-code`).
    /// Injected as `x.ai/restore_code` into `LoadSession` meta, or passed
    /// as `restoreCode` in the `resume_session` ACP payload for worktrees.
    pub restore_code: Option<bool>,
    pub agent_override: Option<serde_json::Value>,
    /// Always-approve for this session (`_meta.yoloMode`).
    pub yolo_mode: bool,
    /// Auto (classifier) permission mode (`_meta.autoMode`). Mutually exclusive
    /// with `yolo_mode` on the agent; both may be set only if yolo wins at spawn.
    pub auto_mode: bool,
    /// Gateway light-frontend (`kind: "chat"`) — `--chat` / `/chat`.
    /// Mutual exclusivity with Build plan profiles: profiles are omitted and a
    /// warn is logged when plan flags are also set (K12).
    pub chat_mode: bool,
    /// Effective screen mode label (`ScreenMode::meta_label`), stamped into
    /// every `PromptRequest._meta.screenMode` for minimal-vs-regular usage
    /// telemetry. `None` (key omitted) only under `Default` in tests; real
    /// launches always know their mode.
    pub screen_mode_label: Option<&'static str>,
    /// Active auth is API key (not OAuth/session). Drives rate-limit copy in
    /// `format_acp_error`. Default `false` (OAuth copy) for tests.
    pub is_api_key_auth: bool,
}
impl SessionFlags {
    /// Resolve the agent profile name from the flags.
    ///
    /// Returns `None` for the default `grok-build` profile (no `_meta`
    /// needed; it already includes TaskTool). Chat mode never injects a
    /// Build profile (remote owns agent behavior).
    pub(super) fn agent_profile(&self) -> Option<&'static str> {
        if self.chat_mode {
            return None;
        }
        match (self.plan_mode, self.subagents, self.ask_user) {
            (true, true, _) => Some("grok-build-plan"),
            (true, false, _) => Some("grok-build-plan-no-subagents"),
            (false, _, true) => Some("grok-build-ask-user"),
            (false, _, false) => None,
        }
    }
    /// Build the `_meta` JSON value for ACP `NewSessionRequest` / `LoadSessionRequest`.
    ///
    /// In practice always `Some`: the permission seeds (`yoloMode` /
    /// `autoMode`) are emitted unconditionally (absent key ≠ off; see the
    /// emit-site comment below). `--no-ask-user` always forces
    /// `askUserQuestion: false` into the meta, even when paired with
    /// `KIGI_AGENT` — the env var chooses the *agent*, but the tool-strip is
    /// independent. Chat mode additionally stamps `x.ai/session.kind`.
    pub(super) fn to_meta(&self) -> Option<acp::Meta> {
        let mut meta = serde_json::Map::new();
        if self.chat_mode {
            if self.plan_mode || self.agent_override.is_some()
                || std::env::var("KIGI_AGENT").ok().is_some_and(|s| !s.trim().is_empty())
            {
                tracing::warn!(
                    "chat mode active: omitting Build agentProfile (plan/agent override ignored)"
                );
            }
        } else if let Some(ref profile) = self.agent_override {
            meta.insert("agentProfile".into(), profile.clone());
        } else if std::env::var("KIGI_AGENT").ok().is_some_and(|s| !s.trim().is_empty())
        {} else if let Some(profile) = self.agent_profile() {
            meta.insert("agentProfile".into(), serde_json::json!(profile));
        }
        if self.chat_mode {
            meta.insert("x.ai/session".into(), serde_json::json!({ "kind" : "chat" }));
        }
        if !self.ask_user {
            meta.insert("askUserQuestion".into(), serde_json::json!(false));
        }
        meta.insert("yoloMode".into(), serde_json::json!(self.yolo_mode));
        meta.insert(
            "autoMode".into(),
            serde_json::json!(
                super::dispatch::effective_auto(self.yolo_mode, self.auto_mode)
            ),
        );
        if meta.is_empty() { None } else { Some(meta) }
    }
}
/// Workspace-bind `_meta` keys forbidden on chat create/load: backend owns
/// workspace for `kind=chat`; the client must not bind Direct/envId/attach.
pub(super) const CHAT_FORBIDDEN_WORKSPACE_BIND_KEYS: &[&str] = &[
    "envId",
    "x.ai/cloud_server_id",
    "x.ai/cloud_existing_workspace",
];
/// Stamp `_meta["x.ai/session"].kind = "chat"` and strip Build `agentProfile` (K12).
pub(super) fn apply_chat_kind_meta(meta: &mut Option<acp::Meta>) {
    let obj = meta.get_or_insert_with(acp::Meta::new);
    obj.insert("x.ai/session".into(), serde_json::json!({ "kind" : "chat" }));
    obj.remove("agentProfile");
}
/// Remove client workspace-bind keys from chat create/load meta (defense in depth).
pub(super) fn scrub_chat_workspace_bind_meta(meta: &mut Option<acp::Meta>) {
    let Some(obj) = meta.as_mut() else {
        return;
    };
    for key in CHAT_FORBIDDEN_WORKSPACE_BIND_KEYS {
        obj.remove(*key);
    }
}
/// Metadata returned from effect execution so the event loop can patch
/// state that requires a spawned task handle (e.g., auth AbortHandle).
#[derive(Default)]
pub(crate) struct EffectMeta {
    /// Auth abort handle + its request sequence. The event loop must
    /// install this into `AppView.auth_state` if the current auth state
    /// still matches the sequence.
    pub auth_abort_handle: Option<(u64, tokio::task::AbortHandle)>,
}
/// Extract the first user prompt text from a session's `chat_history.jsonl`.
///
/// Returns the first line of the `<user_query>` content (if present),
/// or the first line of the raw user message text.
pub(super) fn extract_first_user_prompt(
    info: &kigi_shell::session::info::Info,
) -> Option<String> {
    use std::io::BufRead;
    let history_path = kigi_shell::session::persistence::session_dir(info)
        .join("chat_history.jsonl");
    let file = std::fs::File::open(history_path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("content");
        let text = content
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr
                    .iter()
                    .find_map(|block| {
                        if block.get("type")?.as_str()? == "text" {
                            block.get("text")?.as_str().map(String::from)
                        } else {
                            None
                        }
                    })
            })
            .or_else(|| content.and_then(|c| c.as_str()).map(String::from))?;
        if let Some(start) = text.find("<user_query>") {
            let after = &text[start + "<user_query>".len()..];
            let end = after.find("</user_query>").unwrap_or(after.len());
            let query = after[..end].trim();
            if !query.is_empty() && !query.starts_with('<') {
                return Some(query.to_string());
            }
        }
    }
    None
}
/// Typed deserialization so schema drift is caught at compile time.
/// Synthetic user messages (auto-continue, doom-loop) are excluded.
pub(super) fn count_chat_history_stats(history_path: &Path) -> (usize, usize) {
    use std::io::BufRead;
    use kigi_shell::sampling::{AssistantItem, ConversationItem, UserItem};
    let mut turn_count = 0usize;
    let mut tool_call_count = 0usize;
    let Ok(file) = std::fs::File::open(history_path) else {
        return (0, 0);
    };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        match serde_json::from_str::<ConversationItem>(&line) {
            Ok(ConversationItem::User(UserItem { synthetic_reason: None, .. })) => {
                turn_count += 1;
            }
            Ok(ConversationItem::Assistant(AssistantItem { ref tool_calls, .. })) => {
                tool_call_count += tool_calls.len();
            }
            _ => {}
        }
    }
    (turn_count, tool_call_count)
}
/// Parse the `x.ai/session/list` response payload (the unwrapped
/// `{ "sessions": [...] }` object) into [`SessionPickerEntry`] rows.
///
/// Shared by the resume picker ([`Effect::FetchSessionList`]) and the
/// dashboard's non-leader idle-session fallback
/// ([`Effect::FetchDashboardSessions`]) so both produce identical labels.
/// Sessions older than 30 days, and sessions with no usable user prompt
/// (empty `summary` after fallbacks), are dropped.
pub(super) fn parse_session_picker_entries(
    payload: &serde_json::Value,
) -> Vec<crate::app::app_view::SessionPickerEntry> {
    use crate::app::app_view::SessionPickerEntry;
    let entries: Vec<serde_json::Value> = payload
        .get("sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::days(30);
    entries
        .into_iter()
        .filter_map(|v| {
            let id = v
                .get("sessionId")
                .or_else(|| v.get("session_id"))
                .and_then(|s| s.as_str())?
                .to_string();
            let summary = v
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let first_prompt = v
                .get("firstPrompt")
                .or_else(|| v.get("first_prompt"))
                .and_then(|s| s.as_str())
                .map(String::from);
            let is_conversation = v
                .get("_meta")
                .and_then(|m| m.get("x.ai/session"))
                .and_then(|s| s.get("kind"))
                .and_then(|k| k.as_str()) == Some("chat");
            let parsed_updated: Option<chrono::DateTime<chrono::Utc>> = v
                .get("updatedAt")
                .or_else(|| v.get("updated_at"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok());
            let parsed_created: Option<chrono::DateTime<chrono::Utc>> = v
                .get("createdAt")
                .or_else(|| v.get("created_at"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok());
            let updated_at: chrono::DateTime<chrono::Utc> = match parsed_updated {
                Some(ts) => {
                    if !is_conversation && ts < cutoff {
                        return None;
                    }
                    ts
                }
                None => {
                    if !is_conversation {
                        return None;
                    }
                    parsed_created.unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
                }
            };
            use kigi_tools::implementations::skills::skill::extract_skill_display_text;
            let display = if let Some(ref fp) = first_prompt {
                if let Some(d) = extract_skill_display_text(fp) {
                    d
                } else if !summary.is_empty() {
                    extract_skill_display_text(&summary).unwrap_or(summary)
                } else {
                    fp.lines().next().unwrap_or_default().trim().to_string()
                }
            } else if !summary.is_empty() {
                extract_skill_display_text(&summary).unwrap_or(summary)
            } else {
                let info_cwd = v
                    .get("cwd")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let info = kigi_shell::session::info::Info {
                    id: acp::SessionId::new(id.clone()),
                    cwd: info_cwd,
                };
                extract_first_user_prompt(&info).unwrap_or_default()
            };
            let created_at: chrono::DateTime<chrono::Utc> = parsed_created
                .unwrap_or(updated_at);
            let cwd_str = v
                .get("cwd")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let hostname = v.get("hostname").and_then(|s| s.as_str()).map(String::from);
            let source = if is_conversation {
                "conversation".to_string()
            } else {
                v.get("source").and_then(|s| s.as_str()).unwrap_or("local").to_string()
            };
            let model_id = v
                .get("modelId")
                .or_else(|| v.get("model_id"))
                .and_then(|s| s.as_str())
                .map(String::from);
            let num_messages = v
                .get("numMessages")
                .or_else(|| v.get("num_messages"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            let last_active_at: Option<chrono::DateTime<chrono::Utc>> = v
                .get("lastActiveAt")
                .or_else(|| v.get("last_active_at"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok());
            let branch = v.get("branch").and_then(|s| s.as_str()).map(String::from);
            let worktree_label = v
                .get("worktreeLabel")
                .or_else(|| v.get("worktree_label"))
                .and_then(|s| s.as_str())
                .map(String::from);
            let repo_name = crate::views::session_picker::repo_name_from_cwd(&cwd_str);
            Some(SessionPickerEntry {
                id,
                summary: display,
                updated_at,
                created_at,
                cwd: cwd_str,
                hostname,
                source,
                model_id,
                num_messages,
                last_active_at,
                branch,
                repo_name,
                worktree_label,
                card_detail: None,
            })
        })
        .filter_map(|mut e| {
            if e.summary.is_empty() {
                if e.source == "conversation" {
                    e.summary = "Untitled".to_string();
                } else {
                    return None;
                }
            }
            if e.source == "remote"
                && kigi_shell::session::resolve_local_session_any_cwd(&e.id)
                    .is_some()
            {
                e.source = "local".to_string();
            }
            Some(e)
        })
        .collect()
}
/// Convert a resume-picker session into a dormant dashboard roster row.
///
/// Used by the non-leader dashboard fallback: local on-disk sessions have no
/// live activity signal, so they map to [`RosterActivity::Dormant`] and render
/// in the dashboard's **Inactive** group. The label, cwd, model, and worktree
/// badge all come straight from the picker entry.
pub(super) fn session_picker_entry_to_roster(
    e: &crate::app::app_view::SessionPickerEntry,
) -> crate::app::roster::RosterEntry {
    use crate::app::roster::{RosterActivity, RosterEntry, RosterOrigin};
    let last_change = e.last_active_at.unwrap_or(e.updated_at);
    RosterEntry {
        session_id: e.id.clone(),
        title: Some(e.summary.clone()).filter(|s| !s.trim().is_empty()),
        cwd: e.cwd.clone(),
        is_worktree: e.worktree_label.is_some(),
        model_id: e.model_id.clone(),
        yolo: false,
        activity: RosterActivity::Dormant,
        resident: false,
        last_change_unix_ms: last_change.timestamp_millis(),
        origin: RosterOrigin {
            kind: e.source.clone(),
            host: e.hostname.clone(),
        },
    }
}
pub(super) async fn send_logout(tx: &AcpAgentTx) {
    let req = acp::ExtRequest::new(
        "x.ai/auth/logout",
        serde_json::value::to_raw_value(&serde_json::json!({}))
            .expect("serialize auth/logout params")
            .into(),
    );
    if let Err(e) = acp_send(req, tx).await {
        tracing::warn!(error = % e, "logout failed");
    }
}
pub(super) async fn send_authenticate(
    tx: &AcpAgentTx,
    request_seq: u64,
    method_id: acp::AuthMethodId,
    use_oauth: bool,
    force_interactive: bool,
) -> TaskResult {
    let mut meta = serde_json::json!({ "use_oauth" : use_oauth });
    if force_interactive {
        meta["force_interactive"] = serde_json::json!(true);
    }
    let req = acp::AuthenticateRequest::new(method_id).meta(meta.as_object().cloned());
    match acp_send(req, tx).await {
        Ok(resp) => {
            ulog::info("auth completed", None, None);
            TaskResult::AuthComplete {
                request_seq,
                meta: resp.meta.map(serde_json::Value::Object),
            }
        }
        Err(e) => {
            let error = sanitize_user_error(&e.to_string());
            ulog::error(
                "auth failed",
                None,
                Some(serde_json::json!({ "error" : & error })),
            );
            TaskResult::AuthFailed {
                request_seq,
                error,
            }
        }
    }
}
/// Translate a settings-registry key + value into the matching shell
/// helper call. Type mismatches return an error (not panic) so a
/// spawned task doesn't crash the pager. Unknown keys also return
/// a descriptive error.
pub(crate) async fn persist_setting(
    key: crate::settings::SettingKey,
    value: crate::settings::SettingValue,
) -> Result<(), String> {
    use crate::settings::SettingValue;
    fn kind_mismatch(key: &str, expected: &str, got: &SettingValue) -> String {
        format!("persist_setting({key}) expected {expected}, got {got:?}")
    }
    match key {
        "compact_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("compact_mode", "Bool", &value));
            };
            kigi_shell::util::config::set_compact_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "show_timestamps" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_timestamps", "Bool", &value));
            };
            kigi_shell::util::config::set_show_timestamps(b)
                .await
                .map_err(|e| e.to_string())
        }
        "show_timeline" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_timeline", "Bool", &value));
            };
            kigi_shell::util::config::set_show_timeline(b)
                .await
                .map_err(|e| e.to_string())
        }
        "simple_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("simple_mode", "Bool", &value));
            };
            kigi_shell::util::config::set_simple_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.undo" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.undo", "Bool", &value));
            };
            kigi_shell::util::config::set_contextual_hint_undo(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.plan_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.plan_mode", "Bool", &value));
            };
            kigi_shell::util::config::set_contextual_hint_plan_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.image_input" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.image_input", "Bool", &value),
                );
            };
            kigi_shell::util::config::set_contextual_hint_image_input(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.send_now" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.send_now", "Bool", &value));
            };
            kigi_shell::util::config::set_contextual_hint_send_now(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.small_screen" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.small_screen", "Bool", &value),
                );
            };
            kigi_shell::util::config::set_contextual_hint_small_screen(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.word_select" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.word_select", "Bool", &value),
                );
            };
            kigi_shell::util::config::set_contextual_hint_word_select(b)
                .await
                .map_err(|e| e.to_string())
        }
        "theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("theme", "Enum", &value));
            };
            kigi_shell::util::config::set_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "auto_dark_theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("auto_dark_theme", "Enum", &value));
            };
            kigi_shell::util::config::set_auto_dark_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "auto_light_theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("auto_light_theme", "Enum", &value));
            };
            kigi_shell::util::config::set_auto_light_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "default_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("default_model", "String", &value));
            };
            kigi_shell::util::config::set_default_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_speed" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("scroll_speed", "Int", &value));
            };
            kigi_shell::util::config::set_scroll_speed(i)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("scroll_mode", "Enum", &value));
            };
            kigi_shell::util::config::set_scroll_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "invert_scroll" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("invert_scroll", "Bool", &value));
            };
            kigi_shell::util::config::set_invert_scroll(b)
                .await
                .map_err(|e| e.to_string())
        }
        "display_refresh_auto_cadence" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("display_refresh_auto_cadence", "Bool", &value),
                );
            };
            kigi_shell::util::config::set_display_refresh_auto_cadence(b)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_lines" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("scroll_lines", "Int", &value));
            };
            kigi_shell::util::config::set_scroll_lines(i)
                .await
                .map_err(|e| e.to_string())
        }
        "default_selected_permission" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("default_selected_permission", "Enum", &value));
            };
            kigi_shell::util::config::set_default_selected_permission(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "cancel_subagents_on_turn_cancel" => {
            let SettingValue::Enum(s) = value else {
                return Err(
                    kind_mismatch("cancel_subagents_on_turn_cancel", "Enum", &value),
                );
            };
            kigi_shell::util::config::set_cancel_subagents_on_turn_cancel(
                    s.to_string(),
                )
                .await
                .map_err(|e| e.to_string())
        }
        "vim_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("vim_mode", "Bool", &value));
            };
            kigi_shell::util::config::set_vim_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "remember_tool_approvals" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("remember_tool_approvals", "Bool", &value));
            };
            kigi_shell::util::config::set_remember_tool_approvals(b)
                .await
                .map_err(|e| e.to_string())
        }
        "toolset.ask_user_question.timeout_enabled" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch(
                        "toolset.ask_user_question.timeout_enabled",
                        "Bool",
                        &value,
                    ),
                );
            };
            kigi_shell::util::config::set_ask_user_question_timeout_enabled(b)
                .await
                .map_err(|e| e.to_string())
        }
        "show_thinking_blocks" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_thinking_blocks", "Bool", &value));
            };
            kigi_shell::util::config::set_show_thinking_blocks(b)
                .await
                .map_err(|e| e.to_string())
        }
        "group_tool_verbs" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("group_tool_verbs", "Bool", &value));
            };
            kigi_shell::util::config::set_group_tool_verbs(b)
                .await
                .map_err(|e| e.to_string())
        }
        "collapsed_edit_blocks" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("collapsed_edit_blocks", "Bool", &value));
            };
            kigi_shell::util::config::set_collapsed_edit_blocks(b)
                .await
                .map_err(|e| e.to_string())
        }
        "prompt_suggestions" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("prompt_suggestions", "Bool", &value));
            };
            kigi_shell::util::config::set_prompt_suggestions(b)
                .await
                .map_err(|e| e.to_string())
        }
        "keep_text_selection" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("keep_text_selection", "Enum", &value));
            };
            kigi_shell::util::config::set_keep_text_selection(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "respect_manual_folds" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("respect_manual_folds", "Bool", &value));
            };
            tokio::task::spawn_blocking(move || crate::appearance::persist_respect_manual_folds(
                    b,
                ))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        }
        "render_mermaid" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("render_mermaid", "Enum", &value));
            };
            kigi_shell::util::config::set_render_mermaid(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "hunk_tracker_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("hunk_tracker_mode", "Enum", &value));
            };
            kigi_shell::util::config::set_hunk_tracker_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "screen_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("screen_mode", "Enum", &value));
            };
            kigi_shell::util::config::set_screen_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "max_thoughts_width" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("max_thoughts_width", "Int", &value));
            };
            kigi_shell::util::config::set_max_thoughts_width(i)
                .await
                .map_err(|e| e.to_string())
        }
        "show_tips" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_tips", "Bool", &value));
            };
            kigi_shell::util::config::set_show_tips(b)
                .await
                .map_err(|e| e.to_string())
        }
        "auto_update" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("auto_update", "Bool", &value));
            };
            kigi_shell::util::config::set_auto_update(b)
                .await
                .map_err(|e| e.to_string())
        }
        "fork_secondary_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("fork_secondary_model", "String", &value));
            };
            kigi_shell::util::config::set_fork_secondary_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown setting key for persist: `{other}`")),
    }
}
/// Body for `Effect::PersistPermissionMode`. Factored out for testability.
///
/// 1. Persist `ui.permission_mode` to disk.
/// 2. Fire ACP `x.ai/yolo_mode_changed` (gated on disk success for
///    `WithRollback`; always for `BestEffort`).
/// 3. Return the matching `TaskResult`.
pub(crate) async fn persist_permission_mode_and_notify(
    canonical: &'static str,
    session_id: Option<acp::SessionId>,
    persist: PermissionModePersist,
    tx: AcpAgentTx,
) -> TaskResult {
    let enabled = canonical == "always-approve";
    let auto_mode = canonical == "auto";
    let config_str: &'static str = canonical;
    let disk_result = kigi_shell::util::config::update_config(|cfg| {
            cfg.ui.permission_mode = Some(config_str.to_string());
        })
        .await;
    let disk_outcome: Result<(), String> = disk_result.map_err(|e| e.to_string());
    if should_send_yolo_acp_notification(&disk_outcome, persist) && session_id.is_some()
    {
        let params = serde_json::json!(
            { "yolo_mode" : enabled, "auto_mode" : auto_mode, "permission_mode" :
            config_str, }
        );
        let notification = acp::ExtNotification::new(
            "x.ai/yolo_mode_changed",
            serde_json::value::to_raw_value(&params)
                .expect("serialize yolo_mode_changed params")
                .into(),
        );
        if let Err(e) = acp_send(notification, &tx).await {
            tracing::warn!("Failed to send yolo_mode_changed notification: {e}");
        }
    }
    route_permission_mode_result(disk_outcome, persist, config_str)
}
/// Whether to fire the ACP `x.ai/yolo_mode_changed` notification.
/// `WithRollback` suppresses on disk failure (agent must not see the
/// optimistic value). `BestEffort` always fires.
pub(super) fn should_send_yolo_acp_notification(
    disk_outcome: &Result<(), String>,
    persist: PermissionModePersist,
) -> bool {
    match (disk_outcome, persist) {
        (_, PermissionModePersist::BestEffort) => true,
        (Ok(()), PermissionModePersist::WithRollback(_)) => true,
        (Err(_), PermissionModePersist::WithRollback(_)) => false,
    }
}
/// Extract the typed kill outcome from an `x.ai/task/kill` ext response.
///
/// The agent serializes `ExtMethodResult<KillTaskResponse>`, so the outcome
/// lives at `result.outcome` (`{"result":{"taskId":..,"outcome":
/// "not_found"}}`). Deserializes through the same wire DTOs the agent
/// serializes (`kigi_shell::extensions::task::KillTaskResponse` +
/// `kigi_shell::session::result::ExtMethodResult`) so the contract stays
/// typed end-to-end. Returns `None` — which the dispatcher treats as "clear
/// pending state, keep the row" — for error envelopes (`result: null`) or
/// unparseable payloads. Probing the top level with untyped JSON here was
/// why the tasks-pane ✗ never removed stale (`not_found`) rows after a
/// session resume.
pub(super) fn parse_kill_outcome(
    resp: &str,
) -> Option<kigi_tools::types::KillOutcome> {
    use kigi_shell::extensions::task::KillTaskResponse;
    use kigi_shell::session::result::ExtMethodResult;
    serde_json::from_str::<ExtMethodResult<KillTaskResponse>>(resp)
        .ok()
        .and_then(|envelope| envelope.result)
        .map(|payload| payload.outcome)
}
/// Map an `x.ai/subagent/cancel` response (payload under `result`) to a kill
/// outcome. Prefers the typed `outcome`; falls back to the legacy `cancelled`
/// bool for an older shell or an unknown future `kind`. An error/unparseable
/// body is `RpcFailed` (subagent may still be running — leave the row alone).
pub(super) fn parse_subagent_kill_outcome(resp: &str) -> SubagentKillOutcome {
    use kigi_shell::extensions::task::{
        CancelSubagentResponse, SubagentCancelOutcomeDto,
    };
    let Some(payload) = serde_json::from_str::<
        ExtMethodResult<CancelSubagentResponse>,
    >(resp)
        .ok()
        .and_then(|envelope| envelope.result) else {
        return SubagentKillOutcome::RpcFailed;
    };
    match payload.outcome {
        Some(SubagentCancelOutcomeDto::Cancelled) => SubagentKillOutcome::StoppedLive,
        Some(SubagentCancelOutcomeDto::AlreadyFinished { status }) => {
            SubagentKillOutcome::NothingLive {
                status: Some(status),
            }
        }
        Some(SubagentCancelOutcomeDto::NotFound) => {
            SubagentKillOutcome::NothingLive {
                status: None,
            }
        }
        Some(SubagentCancelOutcomeDto::Unknown) | None => {
            if payload.cancelled {
                SubagentKillOutcome::StoppedLive
            } else {
                SubagentKillOutcome::NothingLive {
                        status: None,
                    }
            }
        }
    }
}
/// Map disk-write outcome + persist variant to the correct `TaskResult`.
pub(super) fn route_permission_mode_result(
    disk_outcome: Result<(), String>,
    persist: PermissionModePersist,
    config_str: &'static str,
) -> TaskResult {
    match (disk_outcome, persist) {
        (Ok(()), _) => {
            TaskResult::SettingPersisted {
                key: "permission_mode",
                value: crate::settings::SettingValue::Enum(config_str),
            }
        }
        (Err(e), PermissionModePersist::WithRollback(prev_canonical)) => {
            tracing::warn!(
                "failed to save permission mode preference: {e} — rolling back"
            );
            TaskResult::SettingPersistFailed {
                key: "permission_mode",
                rollback_value: crate::settings::SettingValue::Enum(prev_canonical),
                error: e,
            }
        }
        (Err(e), PermissionModePersist::BestEffort) => {
            tracing::warn!(
                "failed to save permission mode preference (best-effort): {e}"
            );
            TaskResult::SettingPersistFailedBestEffort {
                key: "permission_mode",
                error: e,
            }
        }
    }
}
/// Fire-and-forget blocking write of one `[hints]` value to config.toml.
/// `what` names the preference for log messages.
pub(super) fn persist_hint(
    tasks: &mut JoinSet<TaskResult>,
    key: &'static str,
    value: impl Into<toml_edit::Value> + Send + 'static,
    what: &'static str,
) {
    tasks
        .spawn(async move {
            match tokio::task::spawn_blocking(move || crate::config_toml_edit::set_hint(
                    key,
                    value,
                ))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("failed to persist {what}: {e}"),
                Err(e) => tracing::warn!("failed to persist {what} (join error): {e}"),
            }
            TaskResult::CancelComplete
        });
}
/// Parse an `x.ai/billing` ext response body (the unwrapped `result`
/// payload) into Kimi usage rows. A body that fails to deserialize is an
/// error, not an empty quota list, so a malformed response can't render
/// as "no usage data".
pub(super) fn parse_usage_response(
    result: &serde_json::Value,
) -> Result<Vec<kigi_shell::extensions::billing::UsageRow>, String> {
    serde_json::from_value::<kigi_shell::extensions::billing::UsageResponse>(result.clone())
        .map(|usage| usage.rows)
        .map_err(|e| format!("Parse error: {e}"))
}
/// A blocking flock on the shared, possibly-network `~/.kigi` lock must never
/// stall the event-loop thread (and would hang exit on `/quit`); the registry
/// is best-effort, so skip on contention.
pub(super) fn unregister_active_session_best_effort(session_id: &acp::SessionId) {
    unregister_active_session_best_effort_in(
        &kigi_shell::util::kigi_home::kigi_home(),
        session_id,
    );
}
pub(super) fn unregister_active_session_best_effort_in(
    root: &Path,
    session_id: &acp::SessionId,
) {
    match kigi_shell::active_sessions::try_unregister_in(root, session_id) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                session_id = % session_id.0,
                "Skipped active-session unregister under lock contention; \
             reaped by collect_crashed on next launch"
            )
        }
        Err(e) => tracing::warn!(? e, "Failed to unregister active session"),
    }
}
