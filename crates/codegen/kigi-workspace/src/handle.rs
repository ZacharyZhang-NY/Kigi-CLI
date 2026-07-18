//! [`WorkspaceHandle`] -- public handle to a workspace instance.
use kigi_hunk_tracker::{HunkTrackerActor, HunkTrackerHandle, TrackingMode};
use kigi_tool_protocol::ToolServerStatusPayload;
use kigi_tool_protocol::turn_hook::TurnHookOutcome;
use prometheus::{
    Histogram, HistogramVec, IntCounter, IntCounterVec, register_histogram, register_histogram_vec,
    register_int_counter, register_int_counter_vec,
};
use std::path::PathBuf;
use std::sync::Arc;
/// Tripwire, expected 0 in production. `path="swap"`: a toolset swap found
/// the outgoing toolset's `Terminal` resource pointing at a backend other
/// than the session-owned one — a resolve path bypassed the session-owned
/// backend, and that backend's background tasks die with the old toolset.
/// Non-zero means background tasks were (or are about to be) killed by a
/// toolset swap: page the owning team. (`path="actor"` — actor-loop
/// channel-closure detection — is not emitted yet.)
pub(crate) static WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "kigi_workspace_terminal_backend_orphaned_total",
            "Terminal backends detected orphaned from their session, by detection path \
             (tripwire, expected 0)",
            &["path"]
        )
        .unwrap()
    });
use crate::capability::CapabilityMode;
use crate::config::{
    AgentSessionConfig, DEFAULT_EVENT_BUFFER_CAPACITY, HookSourceConfig, WorkspaceConfig,
};
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::session::swap_policy::{
    DeferReason, SessionSnapshot, SwapAction, SwapDecision, SwapPolicy, SwapTrigger,
    record_swap_decision, record_toolset_swap,
};
use crate::session::tool_config::resolve_session_toolset;
use crate::session::{WorkspaceSession, WorkspaceShared};
use kigi_file_utils::events::types::CancellationCategory;
use kigi_file_utils::events::{Event, SessionRelationship, TurnOutcomeLabel};
use kigi_tool_protocol::turn_hook::{AfterTurnAckPayload, AfterTurnAckStatus};
/// Per-domain checkpoint captures, by domain and turn outcome.
pub(crate) static REWIND_CHECKPOINT_CAPTURE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "kigi_workspace_rewind_checkpoint_capture_total",
            "Total rewind-checkpoint domain captures",
            &["domain", "outcome"]
        )
        .unwrap()
    });
/// Checkpoint finalizes, by turn outcome.
pub(crate) static REWIND_CHECKPOINT_FINALIZE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "kigi_workspace_rewind_checkpoint_finalize_total",
            "Total rewind-checkpoint finalizes",
            &["outcome"]
        )
        .unwrap()
    });
/// Per-domain restores (the user-initiated `rewind_to` path), by domain and result.
pub(crate) static REWIND_RESTORE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "kigi_workspace_rewind_restore_total",
            "Total rewind-checkpoint domain restores",
            &["domain", "result"]
        )
        .unwrap()
    });
/// Duration of per-domain capture operations.
pub(crate) static REWIND_CHECKPOINT_DURATION: std::sync::LazyLock<HistogramVec> =
    std::sync::LazyLock::new(|| {
        register_histogram_vec!(
            "kigi_workspace_rewind_checkpoint_duration_seconds",
            "Duration of rewind-checkpoint per-domain capture operations",
            &["domain"],
            vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]
        )
        .unwrap()
    });
/// Correctness canary: non-`Completed` `after_turn` boundaries that produced
/// a rewind finalize. Stays 0 unless `workspace_rewind_all_outcomes` is on.
pub(crate) static REWIND_NON_COMPLETED_FINALIZE_TOTAL: std::sync::LazyLock<IntCounterVec> =
    std::sync::LazyLock::new(|| {
        register_int_counter_vec!(
            "kigi_workspace_rewind_non_completed_finalize_total",
            "Non-Completed after_turn boundaries that produced a rewind finalize",
            &["outcome"]
        )
        .unwrap()
    });
/// `domain` label for the rewind metrics. Typed so the closed fs/hunk/git
/// vocabulary can't be mistyped at a call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindDomain {
    Fs,
    Hunk,
    Git,
}
impl RewindDomain {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RewindDomain::Fs => "fs",
            RewindDomain::Hunk => "hunk",
            RewindDomain::Git => "git",
        }
    }
}
/// Map a turn outcome to a stable, bounded `outcome` metric label. The catch-all
/// keeps label cardinality bounded (`TurnHookOutcome` is `#[non_exhaustive]`).
pub(crate) fn rewind_outcome_label(outcome: TurnHookOutcome) -> &'static str {
    match outcome {
        TurnHookOutcome::Completed => "completed",
        TurnHookOutcome::Cancelled => "cancelled",
        TurnHookOutcome::Error => "error",
        _ => "other",
    }
}
/// Map a restore result to its `result` metric label.
pub(crate) fn rewind_result_label(success: bool) -> &'static str {
    if success { "success" } else { "failure" }
}
/// Record a per-domain checkpoint capture, labeled by turn outcome.
pub(crate) fn record_rewind_capture(domain: RewindDomain, outcome: TurnHookOutcome) {
    REWIND_CHECKPOINT_CAPTURE_TOTAL
        .with_label_values(&[domain.as_str(), rewind_outcome_label(outcome)])
        .inc();
}
/// Observe how long a per-domain capture operation took (seconds).
pub(crate) fn observe_rewind_capture_duration(domain: RewindDomain, seconds: f64) {
    REWIND_CHECKPOINT_DURATION
        .with_label_values(&[domain.as_str()])
        .observe(seconds);
}
/// Record a per-domain restore, labeled by result (success/failure).
pub(crate) fn record_rewind_restore(domain: RewindDomain, success: bool) {
    REWIND_RESTORE_TOTAL
        .with_label_values(&[domain.as_str(), rewind_result_label(success)])
        .inc();
}
/// Record the metrics common to every finalize: FS-domain capture + finalize
/// counter (both by `outcome`) + FS capture duration. Shared by the RPC finalize
/// and the non-`Completed` cross-over so the two paths can't drift.
pub(crate) fn record_fs_finalize(outcome: TurnHookOutcome, fs_capture_seconds: f64) {
    observe_rewind_capture_duration(RewindDomain::Fs, fs_capture_seconds);
    record_rewind_capture(RewindDomain::Fs, outcome);
    REWIND_CHECKPOINT_FINALIZE_TOTAL
        .with_label_values(&[rewind_outcome_label(outcome)])
        .inc();
}
/// Record the correctness canary: a non-`Completed` `after_turn` boundary that
/// produced a finalize.
pub(crate) fn record_non_completed_finalize_canary(outcome: TurnHookOutcome) {
    REWIND_NON_COMPLETED_FINALIZE_TOTAL
        .with_label_values(&[rewind_outcome_label(outcome)])
        .inc();
}
/// Zero-init this module's metric families. See [`crate::init_metrics`].
pub(crate) fn init_metrics() {
    WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL
        .with_label_values(&["swap"])
        .inc_by(0);
    for domain in [RewindDomain::Fs, RewindDomain::Hunk, RewindDomain::Git] {
        for outcome in ["completed", "cancelled", "error", "other"] {
            REWIND_CHECKPOINT_CAPTURE_TOTAL
                .with_label_values(&[domain.as_str(), outcome])
                .inc_by(0);
        }
        for result in ["success", "failure"] {
            REWIND_RESTORE_TOTAL
                .with_label_values(&[domain.as_str(), result])
                .inc_by(0);
        }
        let _ = REWIND_CHECKPOINT_DURATION.with_label_values(&[domain.as_str()]);
    }
    for outcome in ["completed", "cancelled", "error", "other"] {
        REWIND_CHECKPOINT_FINALIZE_TOTAL
            .with_label_values(&[outcome])
            .inc_by(0);
        REWIND_NON_COMPLETED_FINALIZE_TOTAL
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}
/// Public handle to a workspace instance. Owns shared state (sessions,
/// MCP snapshot, tool config, event bus) and session lifecycle.
#[derive(Clone)]
pub struct WorkspaceHandle {
    pub(crate) shared: Arc<WorkspaceShared>,
}
impl WorkspaceHandle {
    /// Construct a handle with zero sessions.
    ///
    /// Sessions are created explicitly via [`Self::create_session`] or
    /// [`Self::fork_session`]. There is no implicit "main" session —
    /// callers (TUI, workspace-server binary) create their first
    /// session after construction.
    ///
    /// # Panics
    /// Requires a Tokio runtime to be entered (for broadcast channel).
    pub fn new(config: WorkspaceConfig) -> WorkspaceResult<Self> {
        Self::build(
            config,
            ephemeral_workspace_home(),
            events_enabled(),
            rewind_all_outcomes_from_env(),
        )
    }
    fn build(
        config: WorkspaceConfig,
        workspace_home: std::path::PathBuf,
        events_enabled: bool,
        workspace_rewind_all_outcomes: bool,
    ) -> WorkspaceResult<Self> {
        let sessions = std::collections::HashMap::new();
        let local_registry = kigi_tool_runtime::LocalRegistry::new();
        let capacity = if config.event_buffer_capacity == 0 {
            DEFAULT_EVENT_BUFFER_CAPACITY
        } else {
            config.event_buffer_capacity
        };
        let (events, _drop_rx) = tokio::sync::broadcast::channel(capacity);
        let (hook_registry, hook_load_errors) = {
            use kigi_hooks::discovery::{HookSource, load_hooks_from_sources};
            fn to_hook_source(s: &HookSourceConfig) -> HookSource<'_> {
                match s {
                    HookSourceConfig::SettingsFile(p) => HookSource::SettingsFile(p.as_path()),
                    HookSourceConfig::Directory(p) => HookSource::Directory(p.as_path()),
                }
            }
            let global_refs: Vec<HookSource<'_>> = config
                .hook_global_sources
                .iter()
                .map(to_hook_source)
                .collect();
            let project_refs: Vec<HookSource<'_>> = config
                .hook_project_sources
                .iter()
                .map(to_hook_source)
                .collect();
            let (registry, errors) = load_hooks_from_sources(&global_refs, &project_refs);
            for err in &errors {
                tracing::warn!(error = % err, "hook discovery error (non-fatal)");
            }
            tracing::info!(
                hook_count = registry.len(),
                error_count = errors.len(),
                "hook discovery complete"
            );
            (registry, errors)
        };
        let lsp: Option<Arc<dyn kigi_tools::implementations::lsp::LspBackend>> = {
            let sourced =
                kigi_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
                    &config.root_cwd,
                    &[],
                    &[],
                    &[],
                    &[],
                );
            let servers =
                kigi_tools::implementations::lsp::config::filter_project_lsp_when_untrusted(
                    sourced,
                    config.project_lsp_trusted,
                );
            if servers.is_empty() {
                None
            } else {
                use kigi_tools::implementations::lsp::{LspBackend, LspBackendAdapter, LspManager};
                let mgr = Arc::new(tokio::sync::Mutex::new(LspManager::new(
                    servers,
                    config.root_cwd.clone(),
                    true,
                    kigi_tools::notification::ToolNotificationHandle::noop(),
                )));
                let adapter = Arc::new(LspBackendAdapter::new(mgr));
                adapter.ensure_started_background();
                Some(adapter)
            }
        };
        let session_event_writers: Arc<
            dashmap::DashMap<String, kigi_file_utils::events::EventWriter>,
        > = Arc::new(dashmap::DashMap::new());
        let activity_tracker = Arc::new(
            crate::activity::ActivityTracker::with_prune_window(
                config.status_config.session_idle_prune,
            )
            .with_idle_ignores_background(config.status_config.idle_ignores_background)
            .with_preview_activity_window_ms(
                config.status_config.preview_activity_window.as_millis() as u64,
            ),
        );
        activity_tracker.set_event_writers(session_event_writers.clone());
        let shared = WorkspaceShared {
            default_tool_config: config.default_tool_config,
            confine_fs_to_workspace_root: config.confine_fs_to_workspace_root,
            root_cwd: config.root_cwd.clone(),
            sessions: parking_lot::RwLock::new(sessions),
            session_factory: config.session_factory,
            mcp_tools_snapshot: arc_swap::ArcSwap::new(Arc::new(vec![])),
            events,
            respect_gitignore: config.respect_gitignore,
            memory_config: config.memory_config,
            hook_registry: Arc::new(parking_lot::RwLock::new(hook_registry)),
            hook_load_errors,
            skills_config: config.skills_config,
            plugin_discovery_config: config.plugin_discovery_config,
            client_ext_sink: arc_swap::ArcSwap::new(Arc::new(None)),
            local_registry,
            activity_tracker,
            status_config: config.status_config,
            fuzzy_searches: Arc::new(tokio::sync::Mutex::new(
                crate::file_system::FuzzySearchManager::new(std::time::Duration::from_secs(300)),
            )),
            lsp,
            codebase_indexes: Arc::new(parking_lot::Mutex::new(
                crate::file_system::CodebaseIndexManager::new(),
            )),
            workspace_rewind_all_outcomes,
            workspace_home,
            events_enabled,
            session_event_writers,
            #[cfg(test)]
            post_resolve_test_hook: parking_lot::Mutex::new(None),
        };
        Ok(Self {
            shared: Arc::new(shared),
        })
    }
    #[allow(dead_code)]
    pub fn shared(&self) -> &Arc<WorkspaceShared> {
        &self.shared
    }
    pub fn activity_tracker(&self) -> &std::sync::Arc<crate::activity::ActivityTracker> {
        &self.shared.activity_tracker
    }
    /// Get the workspace root directory.
    pub(crate) fn root_cwd(&self) -> crate::error::WorkspaceResult<PathBuf> {
        Ok(self.shared.root_cwd.clone())
    }
    /// Create a new top-level session from the workspace's default config.
    ///
    /// Unlike [`fork_session`](Self::fork_session), this does not inherit
    /// from a parent — it creates a fresh session with
    /// `CapabilityMode::All` and the workspace's `root_cwd`. Both the
    /// TUI and server use this as the primary session creation path.
    ///
    /// Returns the newly created session, or an error if a session with
    /// the given ID already exists.
    pub fn create_session(
        &self,
        session_id: impl Into<String>,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_cwd(session_id, None)
    }
    /// Create a session with an optional CWD override, using the workspace
    /// default toolset and `CapabilityMode::All`.
    pub fn create_session_with_cwd(
        &self,
        session_id: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_config(session_id, cwd, None, CapabilityMode::All, None, false)
    }
    /// Create a session with an optional CWD override, per-session toolset, and
    /// capability mode. Bind-time entry point; `tool_config: None` uses the default.
    /// `viewer_ctx` is `None` for sessions that don't go through the server bind path.
    pub fn create_session_with_config(
        &self,
        session_id: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
        tool_config: Option<kigi_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
        viewer_ctx: Option<kigi_tool_runtime::WorkspaceViewerContext>,
        system_notifications: bool,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        let session_id = session_id.into();
        let session_cwd = cwd.unwrap_or_else(|| self.shared.root_cwd.clone());
        let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let hunk_cancel = tokio_util::sync::CancellationToken::new();
        let hunk_tracker = HunkTrackerActor::spawn(
            session_id.clone(),
            session_cwd.clone(),
            hunk_event_tx,
            TrackingMode::AllDirty,
            hunk_cancel.clone(),
        );
        let result = self.create_session_with_tracker_and_viewer_ctx(
            session_id,
            session_cwd,
            hunk_tracker,
            tool_config,
            capability,
            viewer_ctx,
            system_notifications,
        );
        if result.is_err() {
            hunk_cancel.cancel();
        }
        result
    }
    /// Create a session that reuses an existing hunk tracker (already rooted at
    /// `cwd`) instead of spawning a new one, so the workspace session and the
    /// agent share a single per-session tracker. `tool_config: None` uses the default.
    pub fn create_session_with_tracker(
        &self,
        session_id: impl Into<String>,
        cwd: std::path::PathBuf,
        hunk_tracker: HunkTrackerHandle,
        tool_config: Option<kigi_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        self.create_session_with_tracker_and_viewer_ctx(
            session_id,
            cwd,
            hunk_tracker,
            tool_config,
            capability,
            None,
            false,
        )
    }
    /// Variant of [`create_session_with_tracker`](Self::create_session_with_tracker)
    /// that carries a session-bind viewer context.
    pub fn create_session_with_tracker_and_viewer_ctx(
        &self,
        session_id: impl Into<String>,
        cwd: std::path::PathBuf,
        hunk_tracker: HunkTrackerHandle,
        tool_config: Option<kigi_tools::registry::types::ToolServerConfig>,
        capability: CapabilityMode,
        viewer_ctx: Option<kigi_tool_runtime::WorkspaceViewerContext>,
        system_notifications: bool,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return Err(WorkspaceError::EmptyAgentId);
        }
        let mut sessions = self.shared.sessions.write();
        if self.shared.activity_tracker.is_draining() {
            return Err(WorkspaceError::ShuttingDown);
        }
        if sessions.contains_key(&session_id) {
            return Err(WorkspaceError::SessionAlreadyExists(session_id));
        }
        let session_env = Arc::new(std::collections::HashMap::new());
        let config = tool_config.unwrap_or_else(|| self.shared.default_tool_config.clone());
        let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
        let system_notify_channel = system_notifications
            .then(kigi_tools::notification::types::ToolNotificationHandle::channel);
        let system_notify_handle = system_notify_channel.as_ref().map(|(h, _)| h.clone());
        let (effective, toolset, terminal_backend) = resolve_session_toolset(
            config,
            capability,
            &mcp_snapshot,
            cwd.clone(),
            session_env.clone(),
            &session_id,
            self.shared.session_factory.as_ref(),
            Some(self.shared.local_registry.clone()),
            self.shared.lsp.clone(),
            viewer_ctx.clone(),
            system_notify_handle,
        )?;
        let session = Arc::new(WorkspaceSession::new(
            session_id.clone(),
            cwd,
            session_env,
            capability,
            0,
            u32::MAX,
            Arc::new(effective),
            toolset,
            terminal_backend,
            hunk_tracker,
            viewer_ctx,
            system_notifications,
            system_notify_channel,
        ));
        tracing::info!(session_id = % session_id, "create_session: new session created");
        sessions.insert(session_id, session.clone());
        record_toolset_swap(
            &self.shared.activity_tracker,
            "create",
            session.session_id(),
        );
        Ok(session)
    }
    pub async fn on_before_turn(
        &self,
        session_id: &str,
        payload: &kigi_tool_protocol::turn_hook::BeforeTurnPayload,
    ) {
        self.sync_session_yolo_mode(session_id, payload.yolo_mode);
        self.on_turn_boundary(
            session_id,
            crate::session::checkpoint::TurnBoundary::turn_start(payload.turn_number),
        )
        .await;
        tracing::debug!(
            session = % session_id, turn = payload.turn_number, model = % payload
            .model_id, "workspace: before_turn processed"
        );
        self.shared
            .session_event_writer(session_id)
            .emit(Event::TurnStarted {
                session_id: session_id.to_owned(),
                turn_number: payload.turn_number,
                model_id: payload.model_id.clone(),
                yolo_mode: payload.yolo_mode,
                conversation_message_count: payload.conversation_message_count,
                session_relationship: decode_session_relationship(&payload.session_relationship),
                schema_version: payload.schema_version.clone(),
                redirect_kind: None,
            });
    }
    /// Fire-and-forget `after_turn` hook path (legacy shells / local mode):
    /// turn-end work, no ack. New shells use the request/response path
    /// ([`Self::compute_turn_injections`]) instead.
    pub async fn on_after_turn(
        &self,
        session_id: &str,
        payload: &kigi_tool_protocol::turn_hook::AfterTurnPayload,
    ) {
        self.process_after_turn(session_id, payload).await;
    }
    async fn process_after_turn(
        &self,
        session_id: &str,
        payload: &kigi_tool_protocol::turn_hook::AfterTurnPayload,
    ) {
        self.on_turn_boundary(
            session_id,
            crate::session::checkpoint::TurnBoundary::turn_end(
                payload.turn_number,
                payload.duration_ms,
                payload.outcome,
            ),
        )
        .await;
        tracing::debug!(
            session = % session_id, turn = payload.turn_number, outcome = ? payload
            .outcome, "workspace: after_turn processed"
        );
        self.shared
            .session_event_writer(session_id)
            .emit(Event::TurnEnded {
                outcome: turn_outcome_label(payload.outcome),
                cancellation_category: decode_cancellation_category(
                    payload.cancellation_category.as_deref(),
                ),
                cancellation_context: payload.cancellation_context.clone(),
            });
    }
    /// Answer a request/response `turn_hook` (sampler/shell → workspace).
    ///
    /// Both phases run the same turn-boundary work as their fire-and-forget
    /// hook counterparts (the server-side sampler signals turns ONLY through
    /// this request channel): `Before` drives [`Self::on_before_turn`]
    /// (including the YOLO-state sync) and answers with a no-op reply
    /// (injections are not computed yet); `After` runs the turn-end work and
    /// acks `Skipped` — there is no artifact pipeline in this build.
    ///
    /// Each phase must be signalled through exactly ONE channel per client —
    /// fire-and-forget hook or request — otherwise its work runs twice.
    pub async fn compute_turn_injections(
        &self,
        session_id: &str,
        request: &kigi_tool_protocol::turn_hook::TurnHookRequest,
    ) -> kigi_tool_protocol::turn_hook::HookReply {
        use kigi_tool_protocol::turn_hook::{HookReply, TurnHookRequest};
        match request {
            TurnHookRequest::Before(payload) => {
                self.on_before_turn(session_id, payload).await;
                HookReply::default()
            }
            TurnHookRequest::After(payload) => {
                self.process_after_turn(session_id, payload).await;
                tracing::debug!(
                    session_id = % session_id, turn_number = payload.turn_number,
                    "after_turn ack returned on hook reply"
                );
                HookReply {
                    after_turn_ack: Some(AfterTurnAckPayload {
                        turn_number: payload.turn_number,
                        status: AfterTurnAckStatus::Skipped,
                        error_message: Some("no_upload_queue".to_owned()),
                        artifact_count: 0,
                    }),
                    ..HookReply::default()
                }
            }
            _ => HookReply::default(),
        }
    }
    /// Sync a before-turn hook's YOLO state into the session, emitting
    /// `YoloToggled` on transitions. No-op for unknown sessions.
    fn sync_session_yolo_mode(&self, session_id: &str, yolo_mode: bool) {
        let Some(session) = self.session(session_id) else {
            return;
        };
        let was = session.yolo_mode();
        if was != yolo_mode {
            tracing::info!(
                session = % session_id, from = was, to = yolo_mode,
                "workspace: yolo_mode changed via before-turn hook"
            );
            session.set_yolo_mode(yolo_mode);
            self.on_yolo_toggled(session_id, yolo_mode);
        }
    }
    /// Bookkeeping for a cancelled in-flight tool call: marks it as
    /// completed in the activity tracker. Does **not** abort execution
    /// of the tool — that requires `CancellationToken` plumbing (future work).
    pub fn cancel_tool_call(&self, session_id: &str, call_id: &str) {
        self.shared.activity_tracker.tool_call_completed(
            call_id,
            Some(session_id),
            kigi_file_utils::events::ToolOutcome::Cancelled,
        );
        tracing::info!(% session_id, % call_id, "cancel_tool_call: marked as completed");
    }
    /// Cancel all in-flight tool calls for a session. Called when a
    /// session-wide Cancel hook arrives (no specific `call_id`).
    pub fn cancel_all_tool_calls(&self, session_id: &str) {
        let count = self
            .shared
            .activity_tracker
            .cancel_all_session_calls(session_id);
        tracing::info!(
            % session_id, count, "cancel_all_tool_calls: marked all as completed"
        );
    }
    /// Clean up workspace state for a session that has ended.
    /// Does **not** drop the session — that is handled by the server's
    /// `unbind_session` lifecycle.
    pub fn on_session_ended(&self, session_id: &str) {
        self.shared.activity_tracker.session_ended(session_id);
        self.shared.session_event_writers.remove(session_id);
        tracing::info!(% session_id, "session_ended cleanup completed");
    }
    /// Record a YOLO / always-approve mode toggle into the session's
    /// `events.jsonl`. These volatile-config mutations are shell-owned; this is
    /// the workspace-side emission entry point invoked by the server/shell forwarding
    /// layer when it observes a `SetYoloMode` command for a bound session. A no-op
    /// when events recording is disabled.
    pub fn on_yolo_toggled(&self, session_id: &str, enabled: bool) {
        self.shared
            .session_event_writer(session_id)
            .emit(Event::YoloToggled { enabled });
        tracing::debug!(% session_id, enabled, "workspace: yolo toggle recorded");
    }
    /// Record an MCP server enable/disable toggle into the session's
    /// `events.jsonl`. Like [`on_yolo_toggled`](Self::on_yolo_toggled), this is
    /// the workspace-side emission point for a shell-owned mutation; the server/shell
    /// forwarding layer calls it when it observes an MCP toggle for a bound
    /// session. A no-op when events recording is disabled.
    pub fn on_mcp_server_toggled(&self, session_id: &str, server_name: &str, enabled: bool) {
        self.shared
            .session_event_writer(session_id)
            .emit(Event::McpServerToggled {
                server_name: server_name.to_owned(),
                enabled,
            });
        tracing::debug!(
            % session_id, % server_name, enabled, "workspace: mcp toggle recorded"
        );
    }
    /// Returns a cloned snapshot of the hook registry, disconnected
    /// from the workspace's live state.
    ///
    /// The registry is loaded once at workspace construction from the
    /// global and project sources in `WorkspaceConfig`; mid-session
    /// reloads (e.g. plugin hook appending) mutate the live registry
    /// in place via the `RwLock` on `WorkspaceShared`. The returned
    /// clone is not affected by subsequent mutations.
    pub fn hook_registry(&self) -> kigi_hooks::discovery::HookRegistry {
        self.shared.hook_registry.read().clone()
    }
    /// Non-fatal errors from the initial hook discovery pass at
    /// workspace construction time.
    ///
    /// Empty when all hook files parsed cleanly. Not updated on
    /// mid-session hook mutations (e.g. plugin hook appending).
    pub fn hook_load_errors(&self) -> &[kigi_hooks::error::HookError] {
        &self.shared.hook_load_errors
    }
    /// Canonicalize the workspace root directory.
    /// Called once per batch and passed to `resolve_service_path` for each file.
    pub(crate) async fn canonical_root(&self) -> WorkspaceResult<PathBuf> {
        Self::canonicalize_root_dir(&self.root_cwd()?).await
    }
    /// Canonicalize a confinement root directory.
    async fn canonicalize_root_dir(root: &std::path::Path) -> WorkspaceResult<PathBuf> {
        #[allow(clippy::disallowed_methods)]
        let canonical = tokio::fs::canonicalize(root).await.map_err(|e| {
            WorkspaceError::Internal(format!("failed to canonicalize workspace root: {e}"))
        })?;
        Ok(dunce::simplified(&canonical).to_path_buf())
    }
    /// Resolve a caller-provided path safely. Accepts a path relative to the
    /// workspace root, or an absolute path that resolves within the root;
    /// either form is confined to the root (paths that escape are rejected).
    /// Two-layer defense: textual normalization + symlink containment.
    ///
    /// # TOCTOU caveat
    /// The symlink check is point-in-time. If a symlink is created between
    /// resolution and I/O, containment is not guaranteed. Defense-in-depth
    /// (e.g., `O_NOFOLLOW`, mount namespaces) would be needed for hostile
    /// workspace environments, which is out of scope for this service-level API.
    pub(crate) async fn resolve_service_path(
        &self,
        req_path: &str,
        canonical_root: &std::path::Path,
    ) -> WorkspaceResult<PathBuf> {
        let root = self.root_cwd()?;
        Self::resolve_path_within_root(req_path, &root, canonical_root).await
    }
    /// Core of [`Self::resolve_service_path`], parameterized over the
    /// confinement root (see [`Self::confine_to_root`]).
    async fn resolve_path_within_root(
        req_path: &str,
        root: &std::path::Path,
        canonical_root: &std::path::Path,
    ) -> WorkspaceResult<PathBuf> {
        use std::path::{Component, Path};
        if req_path.is_empty() {
            return Err(WorkspaceError::Internal("empty path not allowed".into()));
        }
        let path = Path::new(req_path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let mut components = Vec::new();
        for component in joined.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !components.is_empty()
                        && !matches!(components.last(), Some(Component::RootDir))
                    {
                        components.pop();
                    }
                }
                c => components.push(c),
            }
        }
        let normalized: PathBuf = components.into_iter().collect();
        if !normalized.starts_with(root) && !normalized.starts_with(canonical_root) {
            return Err(WorkspaceError::Internal(format!(
                "path escapes workspace root: {req_path}"
            )));
        }
        const MAX_SYMLINK_HOPS: usize = 40;
        let mut symlink_hops = 0usize;
        let mut check_path = normalized.clone();
        loop {
            #[allow(clippy::disallowed_methods)]
            match tokio::fs::canonicalize(&check_path).await {
                Ok(canonical) => {
                    let canonical = dunce::simplified(&canonical).to_path_buf();
                    if !canonical.starts_with(canonical_root) {
                        return Err(WorkspaceError::Internal(format!(
                            "path resolves outside workspace root (symlink escape): {req_path}"
                        )));
                    }
                    break;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    if let Ok(md) = tokio::fs::symlink_metadata(&check_path).await
                        && md.file_type().is_symlink()
                    {
                        if symlink_hops >= MAX_SYMLINK_HOPS {
                            return Err(WorkspaceError::Internal(format!(
                                "path resolves outside workspace root (unresolved symlink chain): {req_path}"
                            )));
                        }
                        let Ok(target) = tokio::fs::read_link(&check_path).await else {
                            return Err(WorkspaceError::Internal(format!(
                                "failed to resolve symlink for containment: {req_path}"
                            )));
                        };
                        symlink_hops += 1;
                        check_path = if target.is_absolute() {
                            target
                        } else {
                            check_path
                                .parent()
                                .map(|p| p.join(&target))
                                .unwrap_or(target)
                        };
                        continue;
                    }
                    match check_path.parent() {
                        Some(parent) if parent != check_path => {
                            check_path = parent.to_path_buf();
                        }
                        _ => {
                            tracing::warn!(
                                "symlink containment: parent chain exhausted without canonicalize for {req_path}"
                            );
                            break;
                        }
                    }
                }
                Err(e) => {
                    return Err(WorkspaceError::Internal(format!(
                        "failed to verify path containment: {e}"
                    )));
                }
            }
        }
        Ok(normalized)
    }
    /// Confine `path` to the workspace root (reject `..`, absolute-outside-root,
    /// symlink escapes) when confinement is enabled. Returns the resolved path and
    /// an optional walk root: `Some(root)` confines a `list`, `None` leaves it
    /// unconfined. Off by default (see
    /// [`WorkspaceConfig::confine_fs_to_workspace_root`](crate::config::WorkspaceConfig::confine_fs_to_workspace_root)):
    /// the absolute `path` is returned as-is, following out-of-root symlinks.
    pub async fn confine_to_workspace_root(
        &self,
        path: &std::path::Path,
    ) -> WorkspaceResult<(PathBuf, Option<PathBuf>)> {
        if !self.shared.confine_fs_to_workspace_root {
            return Ok((path.to_path_buf(), None));
        }
        let path_str = path.to_str().ok_or_else(|| {
            WorkspaceError::Internal(format!("non-UTF-8 path: {}", path.display()))
        })?;
        let canonical_root = self.canonical_root().await?;
        let confined = self.resolve_service_path(path_str, &canonical_root).await?;
        Ok((confined, Some(canonical_root)))
    }
    /// Like [`Self::confine_to_workspace_root`] but against an alternative trusted
    /// root (e.g. a worktree session cwd). Same gate; unconfined by default.
    pub async fn confine_to_root(
        &self,
        path: &std::path::Path,
        root: &std::path::Path,
    ) -> WorkspaceResult<(PathBuf, Option<PathBuf>)> {
        if !self.shared.confine_fs_to_workspace_root {
            return Ok((path.to_path_buf(), None));
        }
        let path_str = path.to_str().ok_or_else(|| {
            WorkspaceError::Internal(format!("non-UTF-8 path: {}", path.display()))
        })?;
        let canonical_root = Self::canonicalize_root_dir(root).await?;
        let confined = Self::resolve_path_within_root(path_str, root, &canonical_root).await?;
        Ok((confined, Some(canonical_root)))
    }
    /// Open a fuzzy file search index rooted at the workspace cwd.
    pub async fn fuzzy_open(
        &self,
        root: Option<&std::path::Path>,
        request_id: Option<String>,
        hidden: bool,
        session_id: Option<String>,
        target_client_id: crate::file_system::TargetClientId,
    ) -> String {
        let search_root = root.unwrap_or(&self.shared.root_cwd);
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.open(
            search_root,
            request_id,
            hidden,
            session_id,
            target_client_id,
        )
    }
    /// Routing info (session id + target client) stored for a search at open
    /// time, read by the notification driver to address status updates.
    pub async fn fuzzy_routing(
        &self,
        search_id: &str,
    ) -> (Option<String>, crate::file_system::TargetClientId) {
        let manager = self.shared.fuzzy_searches.lock().await;
        (
            manager.get_session_id(search_id),
            manager.get_target_client_id(search_id),
        )
    }
    /// Run one poll tick for an active fuzzy search. Returns the next batch of
    /// results (paths absolutized against the search root) or a signal to keep
    /// polling / stop. Drives the `kigi/search/fuzzy/status` notification loop.
    pub async fn fuzzy_poll(
        &self,
        search_id: &str,
        min_generation: usize,
        has_query: bool,
        query_version: usize,
        limit: usize,
    ) -> crate::file_system::FuzzyPollOutcome {
        use crate::file_system::FuzzyPollOutcome;
        let mut manager = self.shared.fuzzy_searches.lock().await;
        if !manager.is_current_query(search_id, query_version) {
            return FuzzyPollOutcome::Stale;
        }
        let root = manager.get_root(search_id);
        match manager.get_results_filtered(search_id, min_generation, has_query) {
            None => {
                if manager.get_results(search_id).is_none() {
                    FuzzyPollOutcome::Closed
                } else {
                    FuzzyPollOutcome::Pending
                }
            }
            Some(mut data) => {
                data.matches.truncate(limit);
                if let Some(root) = &root {
                    for m in &mut data.matches {
                        let path_str = m.path.to_string();
                        if !path_str.starts_with('/') {
                            m.path = root.join(&path_str).to_string_lossy().into_owned().into();
                        }
                    }
                }
                FuzzyPollOutcome::Update(data)
            }
        }
    }
    /// Update the query for an active fuzzy search.
    /// Returns (min_generation, has_query, query_version) if the search exists.
    pub async fn fuzzy_change(
        &self,
        search_id: &str,
        query: &str,
        dirs_only: bool,
    ) -> Option<(usize, bool, usize)> {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.change(search_id, query, dirs_only)
    }
    /// Get fuzzy search results.
    pub async fn fuzzy_get_results(
        &self,
        search_id: &str,
    ) -> Option<crate::file_system::FuzzySearchData> {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.get_results(search_id)
    }
    /// Close a fuzzy search.
    pub async fn fuzzy_close(&self, search_id: &str) -> bool {
        let mut manager = self.shared.fuzzy_searches.lock().await;
        manager.close(search_id)
    }
    /// Install the sink used to deliver workspace-originated ext-notifications
    /// to the client (gateway in local mode, hub in proxy mode).
    pub fn set_client_ext_sink(&self, sink: crate::session::ClientExtSink) {
        self.shared.client_ext_sink.store(Arc::new(Some(sink)));
    }
    /// Whether a client ext-notification sink has been installed.
    pub fn has_client_ext_sink(&self) -> bool {
        self.shared.client_ext_sink.load().is_some()
    }
    /// Deliver an ext-notification to the client via the installed sink.
    /// No-op when no sink is set.
    pub fn emit_client_ext(&self, method: String, params: serde_json::Value) {
        if let Some(sink) = self.shared.client_ext_sink.load_full().as_ref() {
            sink(method, params);
        }
    }
    /// Drive the `kigi/search/fuzzy/status` stream for an active search: poll
    /// until done / closed / superseded, emitting each new result batch to the
    /// client through the ext-notification sink. Co-located with the manager so
    /// it polls in-process in both local and proxy mode.
    pub async fn run_fuzzy_notifications(
        &self,
        search_id: String,
        min_generation: usize,
        has_query: bool,
        query_version: usize,
        limit: usize,
    ) {
        use crate::file_system::FuzzyPollOutcome;
        use std::time::Duration;
        use tokio::time::interval;
        let (session_id, target_client_id) = self.fuzzy_routing(&search_id).await;
        let context_id = session_id.unwrap_or_else(|| "agent".to_string());
        let mut poll_interval = interval(Duration::from_millis(25));
        let mut last_generation: Option<usize> = None;
        let max_polls = 400;
        poll_interval.tick().await;
        for _ in 0..max_polls {
            poll_interval.tick().await;
            let data = match self
                .fuzzy_poll(&search_id, min_generation, has_query, query_version, limit)
                .await
            {
                FuzzyPollOutcome::Stale | FuzzyPollOutcome::Closed => break,
                FuzzyPollOutcome::Pending => continue,
                FuzzyPollOutcome::Update(data) => data,
            };
            if last_generation == Some(data.generation) {
                if data.done {
                    break;
                }
                continue;
            }
            last_generation = Some(data.generation);
            let mut params = serde_json::json!(
                { "sessionId" : context_id.as_str(), "searchId" : search_id.as_str(),
                "matches" : serde_json::to_value(& data.matches).unwrap_or_default(),
                "total" : data.total, "done" : data.done, "generation" : data.generation,
                }
            );
            if !target_client_id.is_none() {
                params["_meta"] = serde_json::json!(
                    { "targetClientId" : serde_json::to_value(& target_client_id)
                    .unwrap_or_default(), }
                );
            }
            self.emit_client_ext("kigi/search/fuzzy/status".to_string(), params);
            if data.done {
                break;
            }
        }
    }
    /// Run a content search (ripgrep) and return results.
    /// Run a streaming content (ripgrep) search rooted at `cwd`, emitting each
    /// batch as `kigi/search/content/status` via the client sink, and returning
    /// the final result. Co-located with the sink so it streams in both modes.
    pub async fn run_content_search(
        &self,
        cwd: std::path::PathBuf,
        context_id: String,
        params: crate::file_system::ContentSearchParams,
    ) -> crate::error::WorkspaceResult<crate::file_system::ContentSearchData> {
        let handle = self.clone();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        crate::file_system::content_search_streaming(&cwd, &params, cancel, move |batch| {
            let params = serde_json::json!(
                { "sessionId" : context_id.as_str(), "files" :
                serde_json::to_value(& batch.files).unwrap_or_default(),
                "totalMatches" : batch.total_matches, "totalFiles" : batch
                .total_files, "done" : batch.done, "truncated" : batch.truncated,
                }
            );
            handle.emit_client_ext("kigi/search/content/status".to_string(), params);
        })
        .await
        .map_err(|e| WorkspaceError::Internal(e.to_string()))
    }
    pub fn get_or_create_codebase_index(
        &self,
        cwd: std::path::PathBuf,
    ) -> (Arc<kigi_codebase_graph::IndexManagerHandle>, bool) {
        self.shared.codebase_indexes.lock().get_or_create(cwd)
    }
    pub fn get_codebase_index(
        &self,
        cwd: &std::path::Path,
    ) -> Option<Arc<kigi_codebase_graph::IndexManagerHandle>> {
        self.shared.codebase_indexes.lock().get(cwd)
    }
    fn spawn_codebase_index_event_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        let root_cwd = self.shared.root_cwd.clone();
        let index_root =
            crate::session::git::find_git_root_from_path(&root_cwd).unwrap_or(root_cwd);
        tokio::spawn(async move {
            let mut rx = shared.events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(kigi_workspace_types::WorkspaceEvent::FsChanged { ref path, kind }) => {
                        if let Some(idx) = shared.codebase_indexes.lock().get(&index_root) {
                            let event =
                                crate::fs_notify::ws_event_to_codebase_graph_event(path, kind);
                            if let Err(e) = idx.send_event(event) {
                                tracing::debug!(
                                    error = % e, "codebase graph: fs event forward failed"
                                );
                            }
                        }
                    }
                    Ok(kigi_workspace_types::WorkspaceEvent::GitHeadChanged { .. }) => {
                        let idx_opt = shared.codebase_indexes.lock().get(&index_root);
                        if let Some(idx) = idx_opt {
                            crate::fs_notify::refresh_codebase_graph_after_head_change(
                                &idx,
                                &index_root,
                                &shared.events,
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "codebase index event forwarder lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!("codebase index event forwarder exited");
        })
    }
    /// Look up an existing session.
    pub fn session(&self, session_id: &str) -> Option<Arc<WorkspaceSession>> {
        self.shared.sessions.read().get(session_id).cloned()
    }
    /// IDs of all sessions currently bound to this workspace.
    pub fn session_ids(&self) -> Vec<String> {
        self.shared.sessions.read().keys().cloned().collect()
    }
    /// Fork a new subagent session. Clones (not references) the parent's
    /// tool config and env. Enforces capability subset and fork budget.
    ///
    /// Forks go through the same post-creation setup as hub-bound sessions
    /// ([`Self::finalize_session_setup`]): each fork gets its own browser
    /// service rather than sharing the parent's tabs.
    pub async fn fork_session(
        &self,
        config: AgentSessionConfig,
    ) -> WorkspaceResult<Arc<WorkspaceSession>> {
        if config.agent_id.is_empty() {
            return Err(WorkspaceError::EmptyAgentId);
        }
        let parent_id = config.parent_session_id.clone().ok_or_else(|| {
            WorkspaceError::ParentSessionNotFound(
                "fork_session requires an explicit parent_session_id".into(),
            )
        })?;
        let parent = self
            .shared
            .sessions
            .read()
            .get(&parent_id)
            .cloned()
            .ok_or_else(|| WorkspaceError::ParentSessionNotFound(parent_id.clone()))?;
        if !config.capability_mode.is_subset_of(parent.capability_mode) {
            return Err(WorkspaceError::CapabilityWidening {
                parent: parent.capability_mode,
                child: config.capability_mode,
            });
        }
        if parent.fork_budget == 0 {
            return Err(WorkspaceError::MaxDepthExceeded { parent: parent_id });
        }
        let new_depth = parent.depth.saturating_add(1);
        let new_fork_budget = parent.fork_budget.saturating_sub(1).min(config.max_depth);
        let baseline = config
            .tool_config
            .clone()
            .unwrap_or_else(|| (*parent.effective_tool_config()).clone());
        let cwd = config
            .cwd_override
            .clone()
            .unwrap_or_else(|| parent.cwd.clone());
        let mut env: std::collections::HashMap<String, String> = (**parent.session_env()).clone();
        env.extend(config.extra_env.clone());
        let session_env = Arc::new(env);
        let mcp_snapshot = self.shared.mcp_tools_snapshot.load_full();
        let inherited_viewer_ctx = parent.viewer_ctx().cloned();
        let (effective, toolset, terminal_backend) = resolve_session_toolset(
            baseline,
            config.capability_mode,
            &mcp_snapshot,
            cwd.clone(),
            session_env.clone(),
            &config.agent_id,
            self.shared.session_factory.as_ref(),
            Some(self.shared.local_registry.clone()),
            self.shared.lsp.clone(),
            inherited_viewer_ctx.clone(),
            None,
        )?;
        let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let hunk_cancel = tokio_util::sync::CancellationToken::new();
        let hunk_tracker = HunkTrackerActor::spawn(
            config.agent_id.clone(),
            cwd.clone(),
            hunk_event_tx,
            TrackingMode::AllDirty,
            hunk_cancel,
        );
        let session = Arc::new(WorkspaceSession::new(
            config.agent_id.clone(),
            cwd,
            session_env,
            config.capability_mode,
            new_depth,
            new_fork_budget,
            Arc::new(effective),
            toolset,
            terminal_backend,
            hunk_tracker,
            inherited_viewer_ctx,
            false,
            None,
        ));
        {
            let mut sessions = self.shared.sessions.write();
            if self.shared.activity_tracker.is_draining() {
                return Err(WorkspaceError::ShuttingDown);
            }
            if sessions.contains_key(&config.agent_id) {
                return Err(WorkspaceError::SessionAlreadyExists(config.agent_id));
            }
            sessions.insert(config.agent_id.clone(), session.clone());
        }
        record_toolset_swap(&self.shared.activity_tracker, "fork", session.session_id());
        Ok(session)
    }
    /// Remove a session.
    pub fn drop_session(&self, caller_session_id: &str, session_id: &str) -> WorkspaceResult<()> {
        if caller_session_id != session_id {
            return Err(WorkspaceError::Unauthorized {
                caller: caller_session_id.to_owned(),
                target: session_id.to_owned(),
            });
        }
        let mut sessions = self.shared.sessions.write();
        let Some(session) = sessions.remove(session_id) else {
            return Err(WorkspaceError::SessionNotFound(session_id.to_owned()));
        };
        drop(sessions);
        session.abort_system_notify_forwarder();
        session.shutdown_terminal_backend();
        Ok(())
    }
    /// Re-resolve every session's toolset against `new_snapshot` and
    /// emit one `WorkspaceEvent::ToolsChanged` per session.
    pub fn on_mcp_snapshot_changed(
        &self,
        new_snapshot: Vec<kigi_tools::registry::types::ToolConfig>,
    ) -> usize {
        self.shared.mcp_tools_snapshot.store(Arc::new(new_snapshot));
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                self.shared
                    .re_resolve_all_sessions("mcp_snapshot_changed", true),
            )
        })
    }
}
/// Whether per-session `events.jsonl` recording is enabled
/// (`KIGI_WORKSPACE_EVENTS_ENABLED=true`). Any other value — including unset —
/// keeps the legacy behaviour: [`WorkspaceShared::session_event_writer`] hands
/// back [`EventWriter::noop()`](kigi_file_utils::events::EventWriter::noop)
/// and no `events.jsonl` is ever opened.
fn events_enabled() -> bool {
    std::env::var("KIGI_WORKSPACE_EVENTS_ENABLED").as_deref() == Ok("true")
}
/// Single source of truth for mapping a turn-hook outcome to the `events.jsonl`
/// [`TurnOutcomeLabel`]. Kept as one `match` so the two enums cannot drift and
/// the mapping is never duplicated across call sites.
fn turn_outcome_label(outcome: kigi_tool_protocol::turn_hook::TurnHookOutcome) -> TurnOutcomeLabel {
    use kigi_tool_protocol::turn_hook::TurnHookOutcome;
    match outcome {
        TurnHookOutcome::Completed => TurnOutcomeLabel::Completed,
        TurnHookOutcome::Cancelled => TurnOutcomeLabel::Cancelled,
        TurnHookOutcome::Error => TurnOutcomeLabel::Error,
        _ => TurnOutcomeLabel::Error,
    }
}
/// Decode the wire `session_relationship` string into the `events.jsonl`
/// enum. Unknown values map to the safe default `Primary`; the snake_case
/// forms are pinned by `session_relationship_wire_forms_round_trip`.
fn decode_session_relationship(s: &str) -> SessionRelationship {
    match s {
        "subagent" => SessionRelationship::Subagent,
        _ => SessionRelationship::Primary,
    }
}
/// Decode the bare snake_case `cancellation_category` string into the
/// `events.jsonl` enum; unrecognised values decode to `None` rather than
/// failing the whole `TurnEnded` emission.
fn decode_cancellation_category(s: Option<&str>) -> Option<CancellationCategory> {
    s.and_then(|s| {
        serde_json::from_value::<CancellationCategory>(serde_json::Value::String(s.to_owned())).ok()
    })
}
/// Per-process ephemeral workspace home for handles constructed without an
/// explicit home (tests, local mode). Never the real kigi home —
/// only [`connect_local_workspace`] resolves `$KIGI_WORKSPACE_HOME` — so the
/// default path can never collide with a real workspace's state dir.
fn ephemeral_workspace_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kigi-workspace-ephemeral-{}", std::process::id()))
}
/// Resolve `workspace_rewind_all_outcomes` from `KIGI_WORKSPACE_REWIND_ALL_OUTCOMES` (default off).
fn rewind_all_outcomes_from_env() -> bool {
    kigi_config::env_bool("KIGI_WORKSPACE_REWIND_ALL_OUTCOMES").unwrap_or(false)
}
impl WorkspaceHandle {
    /// Minimal handle for local mode (no hub). Requires Tokio runtime.
    pub fn new_minimal(
        cwd: std::path::PathBuf,
        project_lsp_trusted: bool,
    ) -> WorkspaceResult<Self> {
        use crate::session::tool_config::WorkspaceSessionContextFactory;
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: kigi_tools::registry::types::ToolServerConfig {
                tools: vec![],
                behavior_preset: None,
            },
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: Arc::new(WorkspaceSessionContextFactory::new()),
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted,
            confine_fs_to_workspace_root: false,
        };
        Self::build(
            config,
            ephemeral_workspace_home(),
            events_enabled(),
            rewind_all_outcomes_from_env(),
        )
    }
}
#[cfg(any(test, feature = "test-support"))]
impl WorkspaceHandle {
    fn test_config(
        root_cwd: std::path::PathBuf,
        factory: std::sync::Arc<
            crate::session::tool_config::test_support::TestSessionContextFactory,
        >,
    ) -> crate::config::WorkspaceConfig {
        use crate::config::{DEFAULT_EVENT_BUFFER_CAPACITY, WorkspaceConfig};
        use crate::session::tool_config::test_support::baseline_config;
        WorkspaceConfig {
            root_cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        }
    }
    /// Test handle backed by a temp dir. Zero sessions; `TempDir` kept alive via `Arc`.
    pub fn for_test() -> Self {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let factory = std::sync::Arc::new(TestSessionContextFactory::new());
        let root_cwd = factory.temp.path().to_path_buf();
        Self::new(Self::test_config(root_cwd, factory))
            .expect("test workspace handle construction must succeed")
    }
    /// Like [`Self::for_test`] but rooted at `root` (must exist on disk).
    pub fn for_test_in(root: &std::path::Path) -> Self {
        use crate::session::tool_config::test_support::TestSessionContextFactory;
        let factory = std::sync::Arc::new(TestSessionContextFactory::new());
        Self::new(Self::test_config(root.to_path_buf(), factory))
            .expect("test workspace handle construction must succeed")
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::capability::CapabilityMode;
    use crate::config::{AgentSessionConfig, DEFAULT_EVENT_BUFFER_CAPACITY, WorkspaceConfig};
    use crate::error::WorkspaceError;
    use crate::session::tool_config::resolve_session_toolset;
    use crate::session::tool_config::test_support::{
        TestSessionContextFactory, baseline_config, tc,
    };
    use kigi_tools::registry::types::ToolServerConfig;
    use kigi_tools::types::tool::ToolKind;
    use kigi_workspace_types::WorkspaceEvent;
    use std::sync::Arc;
    /// Create a test workspace handle with a "main" session pre-created.
    pub(crate) fn make_handle() -> WorkspaceHandle {
        make_handle_with_rewind_all_outcomes(false)
    }
    /// [`make_handle`] with fs confinement on.
    pub(crate) fn make_confining_handle() -> WorkspaceHandle {
        make_handle_inner(false, Default::default(), true)
    }
    /// [`make_handle`] with an explicit `workspace_rewind_all_outcomes` value.
    pub(crate) fn make_handle_with_rewind_all_outcomes(enabled: bool) -> WorkspaceHandle {
        make_handle_inner(enabled, Default::default(), false)
    }
    fn make_handle_inner(
        rewind_all_outcomes: bool,
        status_config: crate::StatusConfig,
        confine_fs_to_workspace_root: bool,
    ) -> WorkspaceHandle {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config,
            project_lsp_trusted: true,
            confine_fs_to_workspace_root,
        };
        let handle = WorkspaceHandle::build(
            config,
            ephemeral_workspace_home(),
            false,
            rewind_all_outcomes,
        )
        .expect("handle construction should succeed");
        handle
            .create_session("main")
            .expect("create main session should succeed");
        handle
    }
    #[test]
    fn rewind_outcome_label_maps_each_variant() {
        assert_eq!(
            rewind_outcome_label(TurnHookOutcome::Completed),
            "completed"
        );
        assert_eq!(
            rewind_outcome_label(TurnHookOutcome::Cancelled),
            "cancelled"
        );
        assert_eq!(rewind_outcome_label(TurnHookOutcome::Error), "error");
    }
    #[test]
    fn rewind_domain_and_result_labels_are_stable() {
        assert_eq!(RewindDomain::Fs.as_str(), "fs");
        assert_eq!(RewindDomain::Hunk.as_str(), "hunk");
        assert_eq!(RewindDomain::Git.as_str(), "git");
        assert_eq!(rewind_result_label(true), "success");
        assert_eq!(rewind_result_label(false), "failure");
    }
    fn session_tool_names(session: &Arc<crate::session::WorkspaceSession>) -> Vec<String> {
        session
            .toolset()
            .tool_definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect()
    }
    async fn toolset_terminal(
        toolset: &Arc<kigi_tools::registry::types::FinalizedToolset>,
    ) -> Arc<dyn kigi_tools::computer::types::TerminalBackend> {
        let res = toolset.resources.lock().await;
        res.get::<kigi_tools::types::resources::Terminal>()
            .map(|t| t.0.clone())
            .expect("toolset must carry a Terminal resource")
    }
    fn orphaned_swap_count() -> u64 {
        WORKSPACE_TERMINAL_BACKEND_ORPHANED_TOTAL
            .with_label_values(&["swap"])
            .get()
    }
    fn explicit_cfg(name_override: &str) -> ToolServerConfig {
        let mut renamed = tc("Kigi:read_file", Some(ToolKind::Read));
        renamed.name_override = Some(name_override.to_owned());
        ToolServerConfig {
            tools: vec![renamed],
            behavior_preset: None,
        }
    }
    /// Background-capable toolset (execute + task-output + kill), the shape
    /// the restart-recovery and RPC-survival tests resolve.
    pub(crate) fn background_capable_cfg() -> ToolServerConfig {
        ToolServerConfig {
            tools: vec![
                tc("Kigi:read_file", Some(ToolKind::Read)),
                tc("Kigi:run_terminal_cmd", Some(ToolKind::Execute)),
                tc("Kigi:get_task_output", Some(ToolKind::BackgroundTaskAction)),
                tc("Kigi:kill_task", Some(ToolKind::KillTaskAction)),
            ],
            behavior_preset: None,
        }
    }
    /// A minimal bash-kind [`TerminalRunRequest`] for `command`, writing
    /// output under `out_dir`.
    ///
    /// [`TerminalRunRequest`]: kigi_tools::computer::types::TerminalRunRequest
    pub(crate) fn terminal_run_request(
        command: &str,
        out_dir: &std::path::Path,
        tool_call_id: &str,
    ) -> kigi_tools::computer::types::TerminalRunRequest {
        kigi_tools::computer::types::TerminalRunRequest {
            command: command.to_string(),
            working_directory: out_dir.to_path_buf(),
            env: std::collections::HashMap::new(),
            timeout: std::time::Duration::from_secs(60),
            output_byte_limit: 4096,
            output_file: out_dir.join(format!("{tool_call_id}.out")),
            notification_handle: kigi_tools::notification::ToolNotificationHandle::noop(),
            tool_call_id: tool_call_id.to_string(),
            display_command: None,
            auto_background_on_timeout: false,
            foreground_block_budget: None,
            kind: kigi_tools::computer::types::TaskKind::Bash,
            owner_session_id: None,
        }
    }
    /// Start a `sleep 30` background task on `session`'s owned backend and
    /// return its handle. Shared by the swap-survival, rebind-survival, and
    /// restart tests.
    pub(crate) async fn start_background_sleep(
        session: &Arc<crate::session::WorkspaceSession>,
        out_dir: &std::path::Path,
        tool_call_id: &str,
    ) -> kigi_tools::computer::types::BackgroundHandle {
        session
            .terminal_backend()
            .run_background(terminal_run_request("sleep 30", out_dir, tool_call_id))
            .await
            .expect("start background task")
    }
    /// A snapshot-driven rebuild must rebuild the toolset AROUND the
    /// session-owned terminal backend, not a fresh one — that identity is
    /// what keeps background tasks alive across the swap.
    #[tokio::test]
    async fn re_resolve_all_sessions_preserves_session_terminal_backend() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let session = handle.session("main").expect("main session exists");
        let backend = session.terminal_backend().clone();
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session, out_dir.path(), "snapshot-bg").await;
        handle
            .shared
            .mcp_tools_snapshot
            .store(Arc::new(vec![tc("Kigi:read_file", Some(ToolKind::Read))]));
        let rebuilt = handle
            .shared
            .re_resolve_all_sessions("mcp_snapshot_changed", true)
            .await;
        assert!(rebuilt >= 1, "the main session must be rebuilt");
        let session = handle.session("main").expect("main session still exists");
        assert!(
            Arc::ptr_eq(&backend, session.terminal_backend()),
            "the session-owned backend must survive a snapshot rebuild"
        );
        let new_terminal = toolset_terminal(&session.toolset()).await;
        assert!(
            Arc::ptr_eq(&backend, &new_terminal),
            "the rebuilt toolset must reference the session-owned backend"
        );
        assert!(
            !new_terminal
                .get_task(&bg.task_id)
                .await
                .expect("the task table must survive the snapshot rebuild")
                .completed,
            "the task's process must still be running after the rebuild"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the orphaned-backend tripwire must stay 0"
        );
        new_terminal.kill_task(&bg.task_id).await;
    }
    /// A local-bound session (external toolset installed via
    /// `bind_local_session`: the toolset keeps the shell's backend, the
    /// session-owned backend is an idle decoy) must be SKIPPED by
    /// snapshot-driven rebuilds — rebuilding around the decoy would detach
    /// tools from the shell's live task table — and must not fire the
    /// orphan tripwire (the mismatch is the local-bind contract).
    #[tokio::test]
    async fn local_bound_session_skips_snapshot_rebuild() {
        let orphaned_before = orphaned_swap_count();
        let handle = make_handle();
        let donor = handle
            .create_session_with_config(
                "donor",
                None,
                Some(explicit_cfg("read_donor")),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create donor session");
        let local = handle
            .create_session_with_config(
                "local",
                None,
                Some(explicit_cfg("read_local")),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create local session");
        let external_toolset = donor.toolset();
        local.replace(local.effective_tool_config(), external_toolset.clone());
        assert!(
            !local.toolset_terminal_is_session_owned().await,
            "precondition: the installed toolset's Terminal must be external"
        );
        handle
            .shared
            .mcp_tools_snapshot
            .store(Arc::new(vec![tc("Kigi:read_file", Some(ToolKind::Read))]));
        handle
            .shared
            .re_resolve_all_sessions("mcp_snapshot_changed", true)
            .await;
        let local = handle.session("local").expect("local session still exists");
        assert!(
            Arc::ptr_eq(&local.toolset(), &external_toolset),
            "the local-bound session's toolset must be untouched by the rebuild"
        );
        assert!(
            Arc::ptr_eq(
                &toolset_terminal(&local.toolset()).await,
                donor.terminal_backend()
            ),
            "the external (shell) backend must still ride the toolset"
        );
        assert_eq!(
            orphaned_swap_count(),
            orphaned_before,
            "the skip must not fire the orphaned-backend tripwire"
        );
    }
    /// Test factory whose sessions own a PERSISTENT-shell backend (the
    /// production factory shape). The plain [`TestSessionContextFactory`]
    /// builds a non-persistent backend, which tracks no shell cwd — hence
    /// this wrapper for the shell-state-survival test.
    struct PersistentShellFactory {
        inner: TestSessionContextFactory,
    }
    impl crate::config::SessionContextFactory for PersistentShellFactory {
        fn build_session_context(
            &self,
            session_id: &str,
            cwd: std::path::PathBuf,
            session_env: Arc<std::collections::HashMap<String, String>>,
            backend: Arc<dyn kigi_tools::computer::types::TerminalBackend>,
        ) -> kigi_tools::registry::types::SessionContext {
            self.inner
                .build_session_context(session_id, cwd, session_env, backend)
        }
        fn build_terminal_backend(&self) -> crate::config::SessionTerminalBackend {
            crate::config::SessionTerminalBackend::local(
                kigi_tools::computer::local::LocalTerminalBackend::with_persistent_shell(),
            )
        }
        fn registry_builder(&self) -> kigi_tools::registry::types::ToolRegistryBuilder {
            self.inner.registry_builder()
        }
    }
    /// [`make_handle`] shape around a [`PersistentShellFactory`]; no
    /// pre-created session.
    fn make_persistent_shell_handle() -> WorkspaceHandle {
        let factory = Arc::new(PersistentShellFactory {
            inner: TestSessionContextFactory::new(),
        });
        let root_cwd = factory.inner.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        WorkspaceHandle::build(config, ephemeral_workspace_home(), false, false)
            .expect("handle construction should succeed")
    }
    /// The persistent shell's state (a model-issued `cd`) survives a
    /// `Reresolved` toolset swap, because the shell lives inside the
    /// session-owned backend — the isolation-matrix #3 "persistent-shell
    /// cwd preserved" sub-assert, on the production backend shape
    /// (`with_persistent_shell`). Unix-only, like the persistent shell.
    #[cfg(unix)]
    #[tokio::test]
    async fn reresolved_swap_preserves_persistent_shell_cwd() {
        let handle = make_persistent_shell_handle();
        let root = handle.root_cwd().expect("root cwd");
        let cfg_a = explicit_cfg("read_a");
        let session = handle
            .create_session_with_config(
                "shell-swap",
                None,
                Some(cfg_a.clone()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        session.set_bind_tool_config_fingerprint(serde_json::to_value(&cfg_a).ok());
        std::fs::create_dir_all(root.join("swap_kept_dir")).expect("create subdir");
        let result = session
            .terminal_backend()
            .run(terminal_run_request("cd swap_kept_dir", &root, "shell-cd"))
            .await
            .expect("cd through the persistent shell");
        assert_eq!(
            result.exit_code,
            Some(0),
            "cd must succeed: {}",
            result.combined_output
        );
        let cwd_before = session
            .terminal_backend()
            .get_shell_cwd()
            .await
            .expect("the persistent shell must track a cwd after a command");
        assert_eq!(
            cwd_before.file_name().and_then(|n| n.to_str()),
            Some("swap_kept_dir"),
            "the shell must have entered the subdir: {}",
            cwd_before.display()
        );
        handle
            .shared
            .mcp_tools_snapshot
            .store(Arc::new(vec![tc("Kigi:read_file", Some(ToolKind::Read))]));
        let rebuilt = handle
            .shared
            .re_resolve_all_sessions("mcp_snapshot_changed", true)
            .await;
        assert!(rebuilt >= 1, "the session must be rebuilt");
        let rebound = handle.session("shell-swap").expect("session still exists");
        let cwd_after = toolset_terminal(&rebound.toolset())
            .await
            .get_shell_cwd()
            .await
            .expect("the swapped-in toolset's terminal must still track the shell cwd");
        assert_eq!(
            cwd_after, cwd_before,
            "the persistent shell's cwd must survive the toolset swap"
        );
    }
    /// Each fork owns its own fresh backend: fork teardown kills only the
    /// fork's tasks, never the parent's.
    #[tokio::test]
    async fn fork_session_owns_distinct_terminal_backend() {
        let handle = make_handle();
        let parent = handle.session("main").expect("main session exists");
        let fork = handle
            .fork_session(fork_cfg_with(
                "fork-backend",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork succeeds");
        assert!(
            !Arc::ptr_eq(parent.terminal_backend(), fork.terminal_backend()),
            "a fork must own its own backend, not share the parent's"
        );
        assert!(
            Arc::ptr_eq(
                fork.terminal_backend(),
                &toolset_terminal(&fork.toolset()).await
            ),
            "the fork's toolset must reference the fork-owned backend"
        );
    }
    /// Poll `backend` with a trivial command until its actor refuses it —
    /// proving an explicit shutdown, since callers still hold live `Arc`s.
    /// Shared by the `drop_session` and hub-evict teardown tests.
    pub(crate) async fn assert_backend_stops(
        backend: &Arc<dyn kigi_tools::computer::types::TerminalBackend>,
    ) {
        let out_dir = tempfile::tempdir().expect("temp dir");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let request = terminal_run_request("true", out_dir.path(), "probe");
            if backend.run(request).await.is_err() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "backend actor must stop after an explicit shutdown even with live Arcs"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    /// `drop_session` shuts the backend down explicitly: the actor stops even
    /// while other `Arc`s to the backend are still alive (teardown must not
    /// depend on the last toolset `Arc` dropping).
    #[tokio::test]
    async fn drop_session_shuts_down_terminal_backend_explicitly() {
        let handle = make_handle();
        let session = handle
            .create_session_with_config("doomed", None, None, CapabilityMode::All, None, false)
            .expect("create session");
        let retained_backend = session.terminal_backend().clone();
        let retained_toolset = session.toolset();
        drop(session);
        handle.drop_session("doomed", "doomed").expect("drop");
        assert_backend_stops(&retained_backend).await;
        drop(retained_toolset);
    }
    /// Isolation matrix #5: a workspace process restart loses tasks (they are
    /// process state — physics), and what's pinned here is the recovery UX:
    /// the same session id recreates cleanly on the fresh process, the task
    /// table starts empty (loss is visible, not silent), and `get_task_output`
    /// for the lost id returns the informative not-found message.
    #[tokio::test]
    async fn restarted_workspace_recreates_session_and_reports_lost_task() {
        let handle_a = make_handle();
        let session_a = handle_a
            .create_session_with_config(
                "reborn",
                None,
                Some(background_capable_cfg()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");
        let out_dir = tempfile::tempdir().expect("temp dir");
        let bg = start_background_sleep(&session_a, out_dir.path(), "restart-bg").await;
        assert!(
            session_a
                .terminal_backend()
                .get_task(&bg.task_id)
                .await
                .is_some(),
            "precondition: the task exists in the first process"
        );
        let handle_b = make_handle();
        let session_b = handle_b
            .create_session_with_config(
                "reborn",
                None,
                Some(background_capable_cfg()),
                CapabilityMode::All,
                None,
                false,
            )
            .expect("the session must recreate cleanly after a restart");
        assert!(
            session_b.terminal_backend().list_tasks().await.is_empty(),
            "precondition: a fresh handle must start with an empty task table"
        );
        let result = session_b
            .toolset()
            .call(
                "get_task_output",
                serde_json::json!({ "task_ids" : [bg.task_id.clone()] }),
                "restart-probe",
                None,
            )
            .await
            .expect("get_task_output must answer, not error");
        let kigi_tools::types::output::ToolOutput::TaskOutput(
            kigi_tool_types::TaskOutputOutput::TaskNotFound(msg),
        ) = &result.output
        else {
            panic!("expected TaskNotFound, got: {:?}", result.output);
        };
        assert!(
            msg.contains(&format!("Task {} not found", bg.task_id)),
            "the message must name the lost task id: {msg}"
        );
        assert!(
            msg.contains("No background tasks or subagents exist in this session"),
            "the message must say the restarted session has no tasks: {msg}"
        );
        session_a.terminal_backend().kill_task(&bg.task_id).await;
    }
    /// The typed helpers feed the registry and the targeted counters advance.
    /// Counters are monotonic, so `after > before` is robust despite the
    /// process-global registry and parallel tests (capture, restore, canary).
    #[test]
    fn rewind_metric_helpers_record_observable_effects() {
        let capture_labels = [
            RewindDomain::Git.as_str(),
            rewind_outcome_label(TurnHookOutcome::Cancelled),
        ];
        let restore_labels = [RewindDomain::Fs.as_str(), rewind_result_label(true)];
        let canary_label = [rewind_outcome_label(TurnHookOutcome::Error)];
        let capture_before = REWIND_CHECKPOINT_CAPTURE_TOTAL
            .with_label_values(&capture_labels)
            .get();
        let restore_before = REWIND_RESTORE_TOTAL
            .with_label_values(&restore_labels)
            .get();
        let canary_before = REWIND_NON_COMPLETED_FINALIZE_TOTAL
            .with_label_values(&canary_label)
            .get();
        record_rewind_capture(RewindDomain::Git, TurnHookOutcome::Cancelled);
        observe_rewind_capture_duration(RewindDomain::Hunk, 0.002);
        record_rewind_restore(RewindDomain::Fs, true);
        record_rewind_restore(RewindDomain::Git, false);
        record_fs_finalize(TurnHookOutcome::Completed, 0.001);
        record_non_completed_finalize_canary(TurnHookOutcome::Error);
        assert!(
            REWIND_CHECKPOINT_CAPTURE_TOTAL
                .with_label_values(&capture_labels)
                .get()
                > capture_before,
            "capture counter must advance"
        );
        assert!(
            REWIND_RESTORE_TOTAL
                .with_label_values(&restore_labels)
                .get()
                > restore_before,
            "restore counter must advance"
        );
        assert!(
            REWIND_NON_COMPLETED_FINALIZE_TOTAL
                .with_label_values(&canary_label)
                .get()
                > canary_before,
            "canary counter must advance"
        );
    }
    /// The client ext-notification sink is invoked with the emitted method +
    /// params, and is no-op until installed.
    #[tokio::test]
    async fn client_ext_sink_receives_emitted_notification() {
        let handle = make_handle();
        assert!(!handle.has_client_ext_sink());
        handle.emit_client_ext("kigi/noop".to_string(), serde_json::json!({}));
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink_captured = captured.clone();
        handle.set_client_ext_sink(Arc::new(move |method, params| {
            sink_captured.lock().push((method, params));
        }));
        assert!(handle.has_client_ext_sink());
        handle.emit_client_ext(
            "kigi/search/fuzzy/status".to_string(),
            serde_json::json!({ "a" : 1 }),
        );
        let got = captured.lock();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "kigi/search/fuzzy/status");
        assert_eq!(got[0].1, serde_json::json!({ "a" : 1 }));
    }
    /// End-to-end local streaming: open + change a fuzzy search over real files,
    /// run the notification driver, and assert a correctly-shaped
    /// `kigi/search/fuzzy/status` is delivered through the sink with the match.
    #[tokio::test]
    async fn fuzzy_change_streams_status_through_sink() {
        use crate::file_system::TargetClientId;
        let handle = make_handle();
        let cwd = handle.root_cwd().unwrap();
        std::fs::write(cwd.join("alpha_widget.rs"), b"").unwrap();
        std::fs::write(cwd.join("beta_gadget.rs"), b"").unwrap();
        let captured = Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let sink_captured = captured.clone();
        handle.set_client_ext_sink(Arc::new(move |method, params| {
            if method == "kigi/search/fuzzy/status" {
                sink_captured.lock().push(params);
            }
        }));
        let search_id = handle
            .fuzzy_open(
                Some(cwd.as_path()),
                None,
                false,
                Some("sess-1".into()),
                TargetClientId::None,
            )
            .await;
        let (min_gen, has_query, query_version) = handle
            .fuzzy_change(&search_id, "alpha_widget", false)
            .await
            .expect("search should exist");
        handle
            .run_fuzzy_notifications(search_id.clone(), min_gen, has_query, query_version, 50)
            .await;
        let got = captured.lock();
        assert!(
            !got.is_empty(),
            "expected at least one fuzzy status notification"
        );
        let last = got.last().unwrap();
        assert_eq!(last["sessionId"], "sess-1");
        assert_eq!(last["searchId"], serde_json::json!(search_id));
        let matches = last["matches"].as_array().expect("matches array");
        assert!(
            matches.iter().any(|m| m["path"]
                .as_str()
                .is_some_and(|p| p.contains("alpha_widget"))),
            "expected alpha_widget in matches, got: {last}"
        );
    }
    /// Like [`make_handle`] but with `events_enabled = true` and a known
    /// `workspace_home` (returned `TempDir`) so tests can read the per-session
    /// `events.jsonl`. Bypasses the env flag via the private `build` seam so the
    /// assertion never races a sibling test's process environment.
    pub(crate) fn make_handle_with_events() -> (WorkspaceHandle, tempfile::TempDir) {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let home = tempfile::tempdir().unwrap();
        let handle = WorkspaceHandle::build(config, home.path().to_path_buf(), true, false)
            .expect("handle construction should succeed");
        (handle, home)
    }
    /// Full wiring: a turn with a tool call, the volatile-config toggles, and a
    /// representative `Mcp*` event all land in the per-session `events.jsonl`
    /// with truthful field content.
    #[tokio::test]
    async fn events_jsonl_captures_turn_tool_toggle_and_mcp_variants() {
        use kigi_file_utils::events::ToolOutcome;
        use kigi_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let (handle, home) = make_handle_with_events();
        let sid = "sess-int";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 7,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 5,
                    session_relationship: "subagent".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("c1", "read_file", Some(sid));
        tracker.tool_call_completed("c1", Some(sid), ToolOutcome::Success);
        handle.on_yolo_toggled(sid, true);
        handle.on_mcp_server_toggled(sid, "linear", false);
        handle.shared().session_event_writer(sid).emit(
            kigi_file_utils::events::Event::McpToolCallStarted {
                server_name: "linear".into(),
                tool_name: "list_issues".into(),
                call_id: "mcp-1".into(),
                timeout_sec: 30,
            },
        );
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 7,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 1234,
                    tool_call_count: 1,
                    model_id: "kigi-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                },
            )
            .await;
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let events: Vec<serde_json::Value> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by_type = |t: &str| {
            events
                .iter()
                .find(|e| e["type"] == t)
                .unwrap_or_else(|| panic!("{t} event missing from events.jsonl"))
        };
        let ts = by_type("turn_started");
        assert_eq!(ts["session_id"], sid);
        assert_eq!(ts["turn_number"], 7);
        assert_eq!(ts["model_id"], "kigi-4");
        assert_eq!(ts["yolo_mode"], false);
        assert_eq!(ts["conversation_message_count"], 5);
        assert_eq!(ts["session_relationship"], "subagent");
        assert_eq!(ts["schema_version"], "1.0");
        assert_eq!(by_type("tool_started")["tool_name"], "read_file");
        let tc = by_type("tool_completed");
        assert_eq!(tc["tool_name"], "read_file");
        assert_eq!(tc["outcome"], "success");
        assert_eq!(by_type("yolo_toggled")["enabled"], true);
        let mcp_toggle = by_type("mcp_server_toggled");
        assert_eq!(mcp_toggle["server_name"], "linear");
        assert_eq!(mcp_toggle["enabled"], false);
        let mcp_call = by_type("mcp_tool_call_started");
        assert_eq!(mcp_call["server_name"], "linear");
        assert_eq!(mcp_call["tool_name"], "list_issues");
        assert_eq!(by_type("turn_ended")["outcome"], "completed");
        let pos = |t: &str| events.iter().position(|e| e["type"] == t).unwrap();
        assert!(
            pos("turn_started") < pos("tool_started"),
            "turn_started must precede tool_started"
        );
        assert!(
            pos("tool_completed") < pos("turn_ended"),
            "tool_completed must precede turn_ended"
        );
    }
    /// Both before-turn hook delivery styles sync YOLO state into the session.
    #[tokio::test]
    async fn before_turn_hooks_sync_session_yolo_mode() {
        use kigi_tool_protocol::turn_hook::{BeforeTurnPayload, TurnHookRequest};
        let handle = make_handle();
        let session = handle.session("main").expect("main session");
        assert!(!session.yolo_mode(), "fail-closed default");
        handle
            .on_before_turn(
                "main",
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: true,
                    ..Default::default()
                },
            )
            .await;
        assert!(session.yolo_mode(), "on_before_turn must sync yolo on");
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 2,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: false,
                    ..Default::default()
                }),
            )
            .await;
        assert_eq!(
            reply,
            kigi_tool_protocol::turn_hook::HookReply::default(),
            "reply stays a behavior-neutral no-op"
        );
        assert!(
            !session.yolo_mode(),
            "compute_turn_injections must sync yolo off"
        );
        handle
            .compute_turn_injections(
                "never-bound",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: true,
                    ..Default::default()
                }),
            )
            .await;
    }
    /// YOLO transitions emit `yolo_toggled` in events.jsonl; repeats don't.
    #[tokio::test]
    async fn before_turn_yolo_transition_emits_yolo_toggled_event() {
        use kigi_tool_protocol::turn_hook::BeforeTurnPayload;
        let (handle, home) = make_handle_with_events();
        let sid = "sess-yolo";
        let _session = handle
            .create_session_with_config(sid, None, None, CapabilityMode::All, None, false)
            .expect("create session");
        for (turn, yolo) in [(1, true), (2, true), (3, false)] {
            handle
                .on_before_turn(
                    sid,
                    &BeforeTurnPayload {
                        turn_number: turn,
                        model_id: "kigi-4".to_owned(),
                        yolo_mode: yolo,
                        ..Default::default()
                    },
                )
                .await;
        }
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let toggles: Vec<bool> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "yolo_toggled")
            .map(|e| e["enabled"].as_bool().unwrap())
            .collect();
        assert_eq!(
            toggles,
            vec![true, false],
            "exactly one toggle per transition (turn 2 repeats true → no re-emit)"
        );
        let turn_yolo: Vec<bool> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "turn_started")
            .map(|e| e["yolo_mode"].as_bool().unwrap())
            .collect();
        assert_eq!(
            turn_yolo,
            vec![true, true, false],
            "turn_started must carry the per-turn yolo state"
        );
    }
    /// Flag-off preservation: `WorkspaceHandle::new` resolves `events_enabled`
    /// from the (unset) env var, so the whole emission path must stay a noop —
    /// no session writers cached, no `sessions/` dir created.
    #[tokio::test]
    async fn events_disabled_keeps_noop_and_writes_nothing() {
        use kigi_file_utils::events::ToolOutcome;
        use kigi_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let handle = make_handle();
        assert!(
            !handle.shared().events_enabled,
            "test precondition: events must be disabled"
        );
        let sid = "main";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("c1", "read_file", Some(sid));
        tracker.tool_call_completed("c1", Some(sid), ToolOutcome::Success);
        handle.on_yolo_toggled(sid, true);
        handle.on_mcp_server_toggled(sid, "linear", true);
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 1,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 1,
                    tool_call_count: 1,
                    model_id: "kigi-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                },
            )
            .await;
        assert!(
            handle.shared().session_event_writers.is_empty(),
            "flag-off must not cache any session writer (EventWriter::noop preserved)"
        );
        let sessions_dir = handle.shared().workspace_home().join("sessions");
        assert!(
            !sessions_dir.exists(),
            "flag-off must not create the sessions dir or any events.jsonl"
        );
    }
    /// `on_session_ended` must evict the session's `events.jsonl` writer from the
    /// shared map (releasing the open file descriptor) without losing any events
    /// already written to disk.
    #[tokio::test]
    async fn session_end_evicts_event_writer_without_data_loss() {
        use kigi_tool_protocol::turn_hook::BeforeTurnPayload;
        let (handle, home) = make_handle_with_events();
        let sid = "sess-evict";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 1,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        assert!(
            handle.shared().session_event_writers.contains_key(sid),
            "writer must be cached after the turn opens it"
        );
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(
            before.contains("turn_started"),
            "TurnStarted must be persisted before eviction"
        );
        handle.on_session_ended(sid);
        assert!(
            !handle.shared().session_event_writers.contains_key(sid),
            "writer must be evicted from the map on session end (fd released)"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "evicting the writer must not lose already-written events"
        );
    }
    /// The single `TurnHookOutcome → TurnOutcomeLabel` mapping used by
    /// `on_after_turn` must be exhaustive and stable.
    #[test]
    fn turn_outcome_label_maps_every_variant() {
        use kigi_file_utils::events::TurnOutcomeLabel;
        use kigi_tool_protocol::turn_hook::TurnHookOutcome;
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Completed),
            TurnOutcomeLabel::Completed
        ));
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Cancelled),
            TurnOutcomeLabel::Cancelled
        ));
        assert!(matches!(
            turn_outcome_label(TurnHookOutcome::Error),
            TurnOutcomeLabel::Error
        ));
    }
    pub(crate) fn fork_cfg_with(
        agent_id: &str,
        capability: CapabilityMode,
        tool_config: Option<ToolServerConfig>,
        parent: Option<&str>,
    ) -> AgentSessionConfig {
        let mut c = AgentSessionConfig::new(agent_id);
        c.capability_mode = capability;
        c.tool_config = tool_config;
        c.parent_session_id = parent.map(|p| p.to_owned());
        c
    }
    /// `WorkspaceHandle::new` (the test/default path, not `connect_local_workspace`)
    /// must use an ephemeral temp `workspace_home` — never the real
    /// `$KIGI_WORKSPACE_HOME` — so default construction can never collide with
    /// a real workspace's state dir.
    #[tokio::test]
    async fn new_defaults_to_ephemeral_home() {
        let handle = make_handle();
        let shared = handle.shared();
        let home = shared.workspace_home();
        assert!(
            home.starts_with(std::env::temp_dir()),
            "default workspace_home must live under the temp dir, got {}",
            home.display()
        );
        assert!(
            home.starts_with(std::env::temp_dir()),
            "default construction must use an ephemeral temp home, got {}",
            home.display()
        );
    }
    #[tokio::test]
    async fn fork_session_inherits_parent_tool_config_when_none() {
        let handle = make_handle();
        let parent = handle.session("main").expect("main session present");
        let parent_baseline = parent.effective_tool_config();
        let parent_ids: Vec<String> = parent_baseline.tools.iter().map(|t| t.id.clone()).collect();
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let child_baseline = child.effective_tool_config();
        let child_ids: Vec<String> = child_baseline.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(child_ids, parent_ids);
        let new_parent_baseline = ToolServerConfig {
            tools: vec![tc("Kigi:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let factory = handle.shared.session_factory.clone();
        let mcp_snapshot = handle.shared.mcp_tools_snapshot.load_full();
        let (eff, ts, _backend) = resolve_session_toolset(
            new_parent_baseline,
            parent.capability_mode(),
            &mcp_snapshot,
            parent.cwd().to_path_buf(),
            parent.session_env().clone(),
            "main",
            factory.as_ref(),
            None,
            None,
            None,
            None,
        )
        .expect("re-resolve should succeed");
        parent.replace(Arc::new(eff), ts);
        let child_after: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            child_after, child_ids,
            "child baseline must not change when parent is mutated"
        );
    }
    #[tokio::test]
    async fn fork_session_uses_explicit_tool_config_when_provided() {
        let handle = make_handle();
        let custom = ToolServerConfig {
            tools: vec![
                tc("Kigi:read_file", Some(ToolKind::Read)),
                tc("Kigi:list_dir", Some(ToolKind::ListDir)),
            ],
            behavior_preset: None,
        };
        let child = handle
            .fork_session(fork_cfg_with(
                "explicit",
                CapabilityMode::ReadWrite,
                Some(custom.clone()),
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let baseline_ids: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let custom_ids: Vec<String> = custom.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(baseline_ids, custom_ids);
    }
    #[tokio::test]
    async fn fork_session_uses_main_session_when_parent_session_id_is_none() {
        let handle = make_handle();
        let marker_config = ToolServerConfig {
            tools: vec![tc("Kigi:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let main = handle.session("main").expect("main present");
        let factory = handle.shared.session_factory.clone();
        let mcp_snapshot = handle.shared.mcp_tools_snapshot.load_full();
        let (eff, ts, _backend) = resolve_session_toolset(
            marker_config,
            main.capability_mode(),
            &mcp_snapshot,
            main.cwd().to_path_buf(),
            main.session_env().clone(),
            "main",
            factory.as_ref(),
            None,
            None,
            None,
            None,
        )
        .expect("re-resolve should succeed");
        main.replace(Arc::new(eff), ts);
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let baseline_ids: Vec<String> = child
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(baseline_ids, vec!["Kigi:read_file".to_string()]);
    }
    #[tokio::test]
    async fn fork_session_uses_named_parent_when_parent_session_id_is_set() {
        let handle = make_handle();
        let custom = ToolServerConfig {
            tools: vec![tc("Kigi:read_file", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        handle
            .fork_session(fork_cfg_with(
                "intermediate",
                CapabilityMode::ReadWrite,
                Some(custom.clone()),
                Some("main"),
            ))
            .await
            .expect("intermediate fork should succeed");
        let leaf = handle
            .fork_session(fork_cfg_with(
                "leaf",
                CapabilityMode::ReadWrite,
                None,
                Some("intermediate"),
            ))
            .await
            .expect("leaf fork should succeed");
        let baseline_ids: Vec<String> = leaf
            .effective_tool_config()
            .tools
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let custom_ids: Vec<String> = custom.tools.iter().map(|t| t.id.clone()).collect();
        assert_eq!(baseline_ids, custom_ids);
    }
    #[test]
    fn fork_session_concurrent_same_id_only_one_winner() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();
        let handle = Arc::new(make_handle());
        let mut handles = vec![];
        for _ in 0..16 {
            let h = handle.clone();
            let g = rt.handle().clone();
            handles.push(std::thread::spawn(move || {
                g.block_on(h.fork_session({
                    let mut c = AgentSessionConfig::new("racer");
                    c.parent_session_id = Some("main".into());
                    c
                }))
            }));
        }
        let mut wins = 0;
        let mut losses = 0;
        for jh in handles {
            let res = jh.join().expect("thread panic");
            match res {
                Ok(_) => wins += 1,
                Err(WorkspaceError::SessionAlreadyExists(id)) => {
                    assert_eq!(id, "racer");
                    losses += 1;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!(wins, 1, "exactly one fork must succeed");
        assert_eq!(losses, 15, "the other 15 must see SessionAlreadyExists");
    }
    #[tokio::test]
    async fn fork_session_empty_agent_id_rejected() {
        let handle = make_handle();
        let err = handle
            .fork_session({
                let mut c = AgentSessionConfig::new("");
                c.parent_session_id = Some("main".into());
                c
            })
            .await
            .expect_err("empty agent_id must error");
        assert!(matches!(err, WorkspaceError::EmptyAgentId), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_capability_widening_rejected() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "ro",
                CapabilityMode::ReadOnly,
                None,
                Some("main"),
            ))
            .await
            .expect("readonly fork ok");
        let err = handle
            .fork_session(fork_cfg_with(
                "widen",
                CapabilityMode::All,
                None,
                Some("ro"),
            ))
            .await
            .expect_err("widening must error");
        assert!(
            matches!(
                err,
                WorkspaceError::CapabilityWidening {
                    parent: CapabilityMode::ReadOnly,
                    child: CapabilityMode::All
                }
            ),
            "got {err:?}"
        );
    }
    /// A fork that races a terminal drain must be rejected by the same
    /// shutdown gate as `create_session`, so it can't repopulate the session
    /// map while the shared upload queue is being flushed/closed.
    #[tokio::test]
    async fn fork_session_rejected_while_draining() {
        let handle = make_handle();
        handle.activity_tracker().set_draining();
        let err = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect_err("fork must be rejected while draining");
        assert!(matches!(err, WorkspaceError::ShuttingDown), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_capability_widening_readwrite_to_execute_rejected() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "rw",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("rw fork ok");
        let err = handle
            .fork_session(fork_cfg_with(
                "exe",
                CapabilityMode::Execute,
                None,
                Some("rw"),
            ))
            .await
            .expect_err("incomparable widen must error");
        assert!(matches!(err, WorkspaceError::CapabilityWidening { .. }));
    }
    #[tokio::test]
    async fn fork_session_max_depth_rejected_when_budget_zero() {
        let handle = make_handle();
        let mut cfg = AgentSessionConfig::new("budgeted");
        cfg.parent_session_id = Some("main".into());
        cfg.max_depth = 0;
        let child = handle.fork_session(cfg).await.expect("budgeted fork ok");
        assert_eq!(child.fork_budget(), 0);
        let err = handle
            .fork_session(fork_cfg_with(
                "grandchild",
                CapabilityMode::ReadWrite,
                None,
                Some("budgeted"),
            ))
            .await
            .expect_err("further fork must error");
        assert!(matches!(err, WorkspaceError::MaxDepthExceeded { .. }));
    }
    #[tokio::test]
    async fn fork_session_parent_session_not_found_errors() {
        let handle = make_handle();
        let mut cfg = AgentSessionConfig::new("orphan");
        cfg.parent_session_id = Some("ghost".into());
        let err = handle
            .fork_session(cfg)
            .await
            .expect_err("missing parent must error");
        match err {
            WorkspaceError::ParentSessionNotFound(id) => assert_eq!(id, "ghost"),
            other => panic!("unexpected: {other:?}"),
        }
    }
    #[tokio::test]
    async fn fork_session_finalize_error_propagated() {
        let handle = make_handle();
        let bad = ToolServerConfig {
            tools: vec![tc("DoesNotExist:nope", Some(ToolKind::Read))],
            behavior_preset: None,
        };
        let cfg = fork_cfg_with("bogus", CapabilityMode::ReadOnly, Some(bad), Some("main"));
        let err = handle
            .fork_session(cfg)
            .await
            .expect_err("bogus id must error");
        assert!(matches!(err, WorkspaceError::Finalize(_)), "got {err:?}");
    }
    #[tokio::test]
    async fn fork_session_extra_env_layered_on_parent() {
        let handle = make_handle();
        let mut intermediate_cfg = AgentSessionConfig::new("parent_env");
        intermediate_cfg
            .extra_env
            .insert("INHERITED".into(), "from_parent".into());
        intermediate_cfg
            .extra_env
            .insert("OVERRIDDEN".into(), "old_value".into());
        intermediate_cfg.parent_session_id = Some("main".into());
        let parent = handle
            .fork_session(intermediate_cfg)
            .await
            .expect("parent ok");
        assert_eq!(
            parent.session_env().get("INHERITED").map(String::as_str),
            Some("from_parent")
        );
        let mut child_cfg = AgentSessionConfig::new("child_env");
        child_cfg.parent_session_id = Some("parent_env".into());
        child_cfg
            .extra_env
            .insert("OVERRIDDEN".into(), "new_value".into());
        child_cfg
            .extra_env
            .insert("CHILD_ONLY".into(), "yes".into());
        let child = handle.fork_session(child_cfg).await.expect("child ok");
        assert_eq!(
            child.session_env().get("INHERITED").map(String::as_str),
            Some("from_parent"),
            "parent var must be inherited"
        );
        assert_eq!(
            child.session_env().get("OVERRIDDEN").map(String::as_str),
            Some("new_value"),
            "extra_env must override parent var"
        );
        assert_eq!(
            child.session_env().get("CHILD_ONLY").map(String::as_str),
            Some("yes"),
            "extra_env must add new var"
        );
    }
    #[tokio::test]
    async fn fork_session_cwd_override_used_when_set() {
        let handle = make_handle();
        let alt = std::env::temp_dir().join("kigi-workspace-test-cwd-override");
        std::fs::create_dir_all(&alt).expect("create alt cwd");
        let mut cfg = AgentSessionConfig::new("cwdchild");
        cfg.cwd_override = Some(alt.clone());
        cfg.parent_session_id = Some("main".into());
        let child = handle.fork_session(cfg).await.expect("ok");
        assert_eq!(child.cwd(), alt);
    }
    #[tokio::test]
    async fn fork_session_inheritance_arc_distinct() {
        let handle = make_handle();
        let main = handle.session("main").expect("main");
        let child = handle
            .fork_session({
                let mut c = AgentSessionConfig::new("kid");
                c.parent_session_id = Some("main".into());
                c
            })
            .await
            .expect("ok");
        assert!(
            !Arc::ptr_eq(
                &main.effective_tool_config(),
                &child.effective_tool_config()
            ),
            "child must hold its own Arc<ToolServerConfig>"
        );
        assert!(
            !Arc::ptr_eq(&main.toolset(), &child.toolset()),
            "child must hold its own Arc<FinalizedToolset>"
        );
    }
    #[tokio::test]
    async fn fork_session_empty_baseline_tools_succeeds() {
        let handle = make_handle();
        let empty = ToolServerConfig {
            tools: vec![],
            behavior_preset: None,
        };
        let child = handle
            .fork_session(fork_cfg_with(
                "empty",
                CapabilityMode::ReadOnly,
                Some(empty),
                Some("main"),
            ))
            .await
            .expect("empty tool set is valid");
        assert!(child.toolset().tool_definitions().is_empty());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_mcp_snapshot_changed_emits_per_session_events_and_rebuilds() {
        let handle = make_handle();
        handle
            .fork_session(fork_cfg_with(
                "subA",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("subA ok");
        handle
            .fork_session(fork_cfg_with(
                "subB",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("subB ok");
        let mut rx = handle.shared.events.subscribe();
        let mcp_tool = tc("Kigi:read_file", Some(ToolKind::Read));
        let rebuilt = handle.on_mcp_snapshot_changed(vec![mcp_tool]);
        assert_eq!(rebuilt, 3, "main + 2 subagents");
        let mut got: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event arrives")
                .expect("not closed");
            match ev {
                WorkspaceEvent::ToolsChanged { session_id } => {
                    got.insert(session_id);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            got,
            ["main".to_string(), "subA".to_string(), "subB".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<String>>()
        );
    }
    #[tokio::test]
    async fn shared_accessors_round_trip() {
        let handle = make_handle();
        assert!(handle.shared().root_cwd().to_str().is_some());
        assert!(!handle.shared().respect_gitignore());
        assert!(handle.shared().memory_config().is_none());
        assert!(handle.shared().mcp_tools_snapshot().is_empty());
        assert!(!handle.shared().default_tool_config().tools.is_empty());
    }
    #[tokio::test]
    async fn hook_registry_empty_when_no_sources() {
        let handle = make_handle();
        let registry = handle.hook_registry();
        assert!(registry.is_empty(), "no sources => empty registry");
        assert!(
            handle.hook_load_errors().is_empty(),
            "no sources => no errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_loads_from_settings_file() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let settings_path = cwd.join("claude_settings.json");
        std::fs::write(
            &settings_path,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo ok"}]}]}}"#,
        )
        .expect("write settings");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(settings_path)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert!(!registry.is_empty(), "settings file should yield hooks");
        assert!(handle.hook_load_errors().is_empty());
    }
    #[tokio::test]
    async fn hook_registry_loads_from_directory() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let hooks_dir = cwd.join("hooks");
        std::fs::create_dir_all(&hooks_dir).expect("mkdir");
        std::fs::write(
            hooks_dir.join("my_hook.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .expect("write hook file");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![HookSourceConfig::Directory(hooks_dir)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert!(!registry.is_empty(), "directory source should yield hooks");
    }
    #[tokio::test]
    async fn hook_registry_snapshot_is_disconnected() {
        let handle = make_handle();
        let snap1 = handle.hook_registry();
        assert!(snap1.is_empty());
        {
            let spec = kigi_hooks::config::HookSpec {
                name: "injected".into(),
                event: kigi_hooks::event::HookEventName::SessionStart,
                handler_type: "command".into(),
                configured_matcher: None,
                matcher: None,
                enabled: true,
                command: Some("echo injected".into()),
                command_raw: Some("echo injected".into()),
                url: None,
                url_raw: None,
                timeout_ms: 10_000,
                source_dir: std::path::PathBuf::from("/tmp"),
                extra_env: std::collections::HashMap::new(),
            };
            handle.shared.hook_registry.write().append_specs(vec![spec]);
        }
        assert!(snap1.is_empty(), "snapshot must not see live mutations");
        let snap2 = handle.hook_registry();
        assert!(!snap2.is_empty(), "fresh snapshot must see mutation");
    }
    #[tokio::test]
    async fn hook_load_errors_reported_for_bad_file() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let bad_path = cwd.join("bad_settings.json");
        std::fs::write(&bad_path, "NOT VALID JSON").expect("write bad file");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(bad_path)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("construction must still succeed");
        assert!(
            !handle.hook_load_errors().is_empty(),
            "bad JSON must produce load errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_global_and_project_sources_merge() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let global_settings = cwd.join("global.json");
        std::fs::write(
                &global_settings,
                r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo global"}]}]}}"#,
            )
            .expect("write");
        let project_settings = cwd.join("project.json");
        std::fs::write(
            &project_settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo project"}]}]}}"#,
        )
        .expect("write");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(global_settings)],
            hook_project_sources: vec![HookSourceConfig::SettingsFile(project_settings)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        let registry = handle.hook_registry();
        assert_eq!(registry.len(), 2, "both sources must contribute hooks");
    }
    #[tokio::test]
    async fn hook_registry_missing_source_is_non_fatal() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let missing = cwd.join("does_not_exist.json");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![HookSourceConfig::SettingsFile(missing)],
            hook_project_sources: vec![],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("must not panic on missing source");
        assert!(handle.hook_registry().is_empty());
        assert!(
            handle.hook_load_errors().is_empty(),
            "missing file should not produce errors"
        );
    }
    #[tokio::test]
    async fn hook_registry_empty_directory_yields_empty_registry() {
        let factory = Arc::new(TestSessionContextFactory::new());
        let cwd = factory.temp.path().to_path_buf();
        let empty_dir = cwd.join("empty_hooks");
        std::fs::create_dir_all(&empty_dir).expect("mkdir");
        let config = WorkspaceConfig {
            root_cwd: cwd,
            default_tool_config: baseline_config(),
            respect_gitignore: false,
            memory_config: None,
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            session_factory: factory,
            hook_global_sources: vec![],
            hook_project_sources: vec![HookSourceConfig::Directory(empty_dir)],
            skills_config: Default::default(),
            plugin_discovery_config: Default::default(),
            status_config: Default::default(),
            project_lsp_trusted: true,
            confine_fs_to_workspace_root: false,
        };
        let handle = WorkspaceHandle::new(config).expect("ok");
        assert!(handle.hook_registry().is_empty());
        assert!(handle.hook_load_errors().is_empty());
    }
    #[tokio::test]
    async fn codebase_index_forwarder_abort_releases_shared() {
        let handle = make_handle();
        tokio::task::yield_now().await;
        let before = Arc::strong_count(handle.shared());
        let task = handle.spawn_codebase_index_event_forwarder();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(Arc::strong_count(handle.shared()) > before);
        task.abort();
        let _ = task.await;
        assert_eq!(
            Arc::strong_count(handle.shared()),
            before,
            "abort must drop the forwarder's WorkspaceShared ref"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_normal() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("src/main.rs", &canonical_root)
            .await
            .expect("normal path should resolve");
        assert_eq!(resolved, root.join("src/main.rs"));
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_empty() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("", &canonical_root)
            .await
            .expect_err("empty path must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("empty path"),
            "error should mention empty path: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_absolute_outside_root() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("/etc/passwd", &canonical_root)
            .await
            .expect_err("absolute path outside root must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("escapes workspace root"),
            "error should mention escape: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_accepts_absolute_within_root() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let rel = handle
            .resolve_service_path("src/main.rs", &canonical_root)
            .await
            .expect("relative path should resolve");
        let abs_input = root.join("src/main.rs");
        let abs = handle
            .resolve_service_path(abs_input.to_str().expect("utf-8 path"), &canonical_root)
            .await
            .expect("absolute path within root should resolve");
        assert_eq!(abs, rel);
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_escape() {
        let handle = make_handle();
        let canonical_root = handle.canonical_root().await.unwrap();
        let err = handle
            .resolve_service_path("../../etc/passwd", &canonical_root)
            .await
            .expect_err("escape path must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("path escapes workspace root"),
            "error should mention escape: {msg}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_allows_dotdot_within_root() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("src/../lib.rs", &canonical_root)
            .await
            .expect("dotdot within root should resolve");
        assert_eq!(resolved, root.join("lib.rs"));
    }
    #[tokio::test]
    async fn resolve_service_path_rejects_symlink_escape() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").expect("write secret");
        let link_path = root.join("escape_link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link_path).expect("create symlink");
        #[cfg(not(unix))]
        {
            return;
        }
        let err = handle
            .resolve_service_path("escape_link/secret.txt", &canonical_root)
            .await
            .expect_err("symlink escape must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink escape"),
            "error should mention symlink escape: {msg}"
        );
    }
    /// A *dangling* leaf symlink (target missing, outside root) must be rejected:
    /// `canonicalize` fails NotFound, so the leaf is resolved via `read_link`.
    #[tokio::test]
    #[cfg(unix)]
    async fn resolve_service_path_rejects_dangling_symlink_escape() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        std::os::unix::fs::symlink(outside.path().join("new.txt"), root.join("lnk"))
            .expect("create symlink");
        let err = handle
            .resolve_service_path("lnk", &canonical_root)
            .await
            .expect_err("dangling symlink escape must be rejected");
        assert!(
            format!("{err}").contains("symlink escape"),
            "error should mention symlink escape: {err}"
        );
    }
    /// A multi-hop chain of dangling in-root links ending outside the root must
    /// be followed and rejected (not fall through the ancestor walk).
    #[tokio::test]
    #[cfg(unix)]
    async fn resolve_service_path_rejects_dangling_symlink_chain() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let outside = tempfile::tempdir().expect("outside");
        for i in 0..3 {
            std::os::unix::fs::symlink(
                root.join(format!("lnk{}", i + 1)),
                root.join(format!("lnk{i}")),
            )
            .expect("chain link");
        }
        std::os::unix::fs::symlink(outside.path().join("x"), root.join("lnk3")).expect("tail link");
        let err = handle
            .resolve_service_path("lnk0", &canonical_root)
            .await
            .expect_err("dangling symlink chain escaping root must be rejected");
        assert!(
            format!("{err}").contains("symlink escape")
                || format!("{err}").contains("unresolved symlink chain"),
            "unexpected error: {err}"
        );
    }
    #[tokio::test]
    async fn resolve_service_path_nested_subdir() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("a/b/c/d.txt", &canonical_root)
            .await
            .expect("deeply nested path should resolve");
        assert_eq!(resolved, root.join("a/b/c/d.txt"));
    }
    #[tokio::test]
    async fn resolve_service_path_dot_current_dir() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let canonical_root = handle.canonical_root().await.unwrap();
        let resolved = handle
            .resolve_service_path("./src/./main.rs", &canonical_root)
            .await
            .expect("dot segments should be stripped");
        assert_eq!(resolved, root.join("src/main.rs"));
    }
    #[tokio::test]
    async fn confine_to_root_accepts_path_within_alternative_root() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let alt_root = alt.path().to_path_buf();
        let target = alt_root.join("src/foo.rs");
        let (confined, _canonical) = handle
            .confine_to_root(&target, &alt_root)
            .await
            .expect("path within the alternative root should resolve");
        assert_eq!(confined, target);
        handle
            .confine_to_workspace_root(&target)
            .await
            .expect_err("path outside the workspace root must be rejected");
    }
    #[tokio::test]
    async fn confine_to_root_rejects_dotdot_escape() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let err = handle
            .confine_to_root(std::path::Path::new("../../etc/passwd"), alt.path())
            .await
            .expect_err("dotdot escape from the alternative root must be rejected");
        assert!(
            format!("{err}").contains("path escapes workspace root"),
            "error should mention escape: {err}"
        );
    }
    #[tokio::test]
    async fn confine_to_root_rejects_absolute_path_outside_root() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let err = handle
            .confine_to_root(std::path::Path::new("/etc/passwd"), alt.path())
            .await
            .expect_err("absolute path outside the alternative root must be rejected");
        assert!(
            format!("{err}").contains("escapes workspace root"),
            "error should mention escape: {err}"
        );
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn confine_to_root_rejects_symlink_escape() {
        let handle = make_confining_handle();
        let alt = tempfile::tempdir().expect("create alt root");
        let outside = tempfile::tempdir().expect("create outside dir");
        std::fs::write(outside.path().join("secret.txt"), "top secret").expect("write secret");
        std::os::unix::fs::symlink(outside.path(), alt.path().join("escape_link"))
            .expect("create symlink");
        let err = handle
            .confine_to_root(&alt.path().join("escape_link/secret.txt"), alt.path())
            .await
            .expect_err("symlink escaping the alternative root must be rejected");
        assert!(
            format!("{err}").contains("symlink escape"),
            "error should mention symlink escape: {err}"
        );
    }
    /// Off by default: an out-of-root absolute path is passed through, not rejected.
    #[tokio::test]
    async fn confine_to_workspace_root_unconfined_by_default_allows_escape() {
        let handle = make_handle();
        let outside = tempfile::tempdir().expect("create outside dir");
        let target = outside.path().join("secret.txt");
        let (resolved, walk_root) = handle
            .confine_to_workspace_root(&target)
            .await
            .expect("unconfined resolution must not reject an outside path");
        assert_eq!(resolved, target, "path is passed through unchanged");
        assert!(
            walk_root.is_none(),
            "no confining walk root when confinement is off"
        );
    }
    /// Off by default: a symlink escaping the root is followed, not rejected.
    #[tokio::test]
    #[cfg(unix)]
    async fn confine_to_workspace_root_unconfined_by_default_follows_symlink() {
        let handle = make_handle();
        let root = handle.root_cwd().unwrap();
        let outside = tempfile::tempdir().expect("create outside dir");
        std::fs::write(outside.path().join("secret.txt"), "ok").expect("write secret");
        std::os::unix::fs::symlink(outside.path(), root.join("escape_link"))
            .expect("create symlink");
        let link_path = root.join("escape_link/secret.txt");
        let (resolved, walk_root) = handle
            .confine_to_workspace_root(&link_path)
            .await
            .expect("unconfined resolution must follow a symlink out of the root");
        assert_eq!(resolved, link_path);
        assert!(walk_root.is_none());
    }
    #[tokio::test]
    async fn per_session_hunk_tracker_isolation() {
        let handle = make_handle();
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        child.hunk_tracker().record_agent_write(
            std::path::PathBuf::from("/tmp/test-file.rs"),
            "fn main() {}".to_string(),
            0,
            None,
        );
        let child_hunks = child.hunk_tracker().get_all_hunks().await;
        assert!(
            !child_hunks.is_empty(),
            "child session should have tracked hunks"
        );
        let main = handle.session("main").expect("main session present");
        let main_hunks = main.hunk_tracker().get_all_hunks().await;
        assert!(
            main_hunks.is_empty(),
            "main session hunk tracker must be isolated from child: got {} hunks",
            main_hunks.len()
        );
    }
    #[tokio::test]
    async fn cancel_tool_call_marks_call_completed() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        tracker.tool_call_started("call-1", "read_file", Some("main"));
        assert_eq!(tracker.snapshot().active_tool_calls, 1);
        handle.cancel_tool_call("main", "call-1");
        assert_eq!(
            tracker.snapshot().active_tool_calls,
            0,
            "cancel_tool_call should mark the call as completed"
        );
    }
    #[tokio::test]
    async fn cancel_tool_call_unknown_id_is_noop() {
        let handle = make_handle();
        handle.cancel_tool_call("main", "never-started");
        assert_eq!(handle.activity_tracker().snapshot().active_tool_calls, 0);
    }
    #[tokio::test]
    async fn on_session_ended_clears_turn_active() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        tracker.turn_started("main", 1);
        assert!(tracker.is_turn_active("main"));
        handle.on_session_ended("main");
        assert!(
            !tracker.is_turn_active("main"),
            "on_session_ended should clear turn_active"
        );
    }
    #[tokio::test]
    async fn on_session_ended_unknown_session_is_noop() {
        let handle = make_handle();
        let tracker = handle.activity_tracker();
        let sessions_before = tracker.known_sessions();
        handle.on_session_ended("nonexistent");
        assert_eq!(
            tracker.known_sessions(),
            sessions_before,
            "on_session_ended must not create a new session entry"
        );
    }
    #[tokio::test]
    async fn fork_session_inherits_viewer_ctx_from_parent() {
        let handle = make_handle();
        handle.drop_session("main", "main").expect("drop main");
        let parent = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                kigi_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(kigi_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: true,
                }),
                false,
            )
            .expect("create parent");
        assert!(parent.viewer_ctx().is_some());
        let child = handle
            .fork_session(fork_cfg_with(
                "child",
                CapabilityMode::ReadWrite,
                None,
                Some("main"),
            ))
            .await
            .expect("fork should succeed");
        let inherited = child.viewer_ctx().expect("child inherits viewer_ctx");
        assert!(
            inherited.stream_tool_progress,
            "child must inherit the parent's stream_tool_progress flag"
        );
    }
    /// Dropping and rebinding a session with the same ID surfaces the
    /// new `viewer_ctx` (kill-switch for mid-session staleness).
    #[tokio::test]
    async fn drop_then_rebind_session_replaces_viewer_ctx_value() {
        let handle = make_handle();
        handle.drop_session("main", "main").expect("drop main");
        let s1 = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                kigi_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(kigi_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: true,
                }),
                false,
            )
            .expect("first bind");
        assert_eq!(s1.viewer_ctx().map(|c| c.stream_tool_progress), Some(true));
        handle.drop_session("main", "main").expect("drop");
        let s2 = handle
            .create_session_with_tracker_and_viewer_ctx(
                "main",
                handle.root_cwd().unwrap(),
                kigi_hunk_tracker::HunkTrackerHandle::noop(),
                None,
                CapabilityMode::All,
                Some(kigi_tool_runtime::WorkspaceViewerContext {
                    stream_tool_progress: false,
                }),
                false,
            )
            .expect("second bind");
        assert_eq!(
            s2.viewer_ctx().map(|c| c.stream_tool_progress),
            Some(false),
            "rebind must surface the new viewer_ctx value"
        );
    }
    /// The hand-written decode `match` must not drift from the enum's
    /// serde snake_case forms.
    #[test]
    fn session_relationship_wire_forms_round_trip() {
        for variant in [SessionRelationship::Primary, SessionRelationship::Subagent] {
            let wire = serde_json::to_value(variant).unwrap();
            let wire = wire.as_str().unwrap();
            let decoded = decode_session_relationship(wire);
            assert_eq!(
                serde_json::to_value(decoded).unwrap().as_str(),
                Some(wire),
                "{variant:?} must round-trip through decode_session_relationship"
            );
        }
        assert!(matches!(
            decode_session_relationship("nonsense"),
            SessionRelationship::Primary
        ));
    }
    /// The workspace decodes the bare snake_case `cancellation_category` string
    /// back into the enum; unknown / absent values decode to `None`.
    #[test]
    fn cancellation_category_decode_round_trips() {
        assert_eq!(
            decode_cancellation_category(Some("hook_denied")),
            Some(CancellationCategory::HookDenied),
        );
        assert_eq!(
            decode_cancellation_category(Some("permission_rejected")),
            Some(CancellationCategory::PermissionRejected),
        );
        assert_eq!(decode_cancellation_category(Some("not_a_category")), None);
        assert_eq!(decode_cancellation_category(None), None);
    }
    /// The request/response `After` turn hook performs the turn-end work and
    /// returns the ack on the reply: always a truthful `Skipped` with the
    /// `no_upload_queue` diagnostic — there is no artifact pipeline.
    #[tokio::test]
    async fn compute_turn_injections_after_returns_skipped_ack() {
        use kigi_tool_protocol::turn_hook::{AfterTurnPayload, TurnHookOutcome, TurnHookRequest};
        let handle = make_handle();
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::After(AfterTurnPayload {
                    turn_number: 3,
                    outcome: TurnHookOutcome::Completed,
                    duration_ms: 10,
                    tool_call_count: 0,
                    model_id: "kigi-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                }),
            )
            .await;
        let ack = reply
            .after_turn_ack
            .expect("After reply must carry the ack");
        assert_eq!(ack.turn_number, 3);
        assert_eq!(ack.status, AfterTurnAckStatus::Skipped);
        assert_eq!(ack.error_message.as_deref(), Some("no_upload_queue"));
        assert_eq!(ack.artifact_count, 0);
        assert!(reply.injections.is_empty());
    }
    /// A `Before` request answers with a no-op reply (no ack) while driving
    /// the same turn-start work as the fire-and-forget hook — the request
    /// channel is the only turn signal the server-side sampler sends.
    #[tokio::test]
    async fn compute_turn_injections_before_runs_turn_start_and_replies_noop() {
        use kigi_tool_protocol::turn_hook::{BeforeTurnPayload, HookReply, TurnHookRequest};
        let handle = make_handle();
        let reply = handle
            .compute_turn_injections(
                "main",
                &TurnHookRequest::Before(BeforeTurnPayload {
                    turn_number: 9,
                    ..BeforeTurnPayload::default()
                }),
            )
            .await;
        assert_eq!(reply, HookReply::default());
        assert!(
            handle
                .activity_tracker()
                .known_sessions()
                .iter()
                .any(|s| s == "main"),
            "Before request must drive on_before_turn (activity tracking)"
        );
    }
    /// The extended after-turn cancellation pair is decoded into the
    /// `TurnEnded` line: the category string becomes the enum's snake_case form
    /// and the context object passes through verbatim.
    #[tokio::test]
    async fn after_turn_decodes_cancellation_fields_into_events_jsonl() {
        use kigi_tool_protocol::turn_hook::{AfterTurnPayload, BeforeTurnPayload, TurnHookOutcome};
        let (handle, home) = make_handle_with_events();
        let sid = "sess-cancel";
        handle
            .on_before_turn(
                sid,
                &BeforeTurnPayload {
                    turn_number: 2,
                    model_id: "kigi-4".to_owned(),
                    yolo_mode: false,
                    conversation_message_count: 0,
                    session_relationship: "primary".to_owned(),
                    schema_version: "1.0".to_owned(),
                },
            )
            .await;
        handle
            .on_after_turn(
                sid,
                &AfterTurnPayload {
                    turn_number: 2,
                    outcome: TurnHookOutcome::Cancelled,
                    duration_ms: 10,
                    tool_call_count: 0,
                    model_id: "kigi-4".to_owned(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: Some("permission_rejected".to_owned()),
                    cancellation_context: Some(serde_json::json!({ "recovery" : false })),
                },
            )
            .await;
        let path = home.path().join("sessions").join(sid).join("events.jsonl");
        let text = std::fs::read_to_string(&path).expect("events.jsonl must exist");
        let ended = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|e| e["type"] == "turn_ended")
            .expect("turn_ended must be present");
        assert_eq!(ended["outcome"], "cancelled");
        assert_eq!(ended["cancellation_category"], "permission_rejected");
        assert_eq!(
            ended["cancellation_context"],
            serde_json::json!({ "recovery" : false })
        );
    }
}
