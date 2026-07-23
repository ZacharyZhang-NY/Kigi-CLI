//! Adapter that owns a finalized `ToolRegistry` and exposes tool dispatch,
//! definitions and tracker state to the session layer.

use std::sync::Arc;

use crate::computer::types::KillOutcome;
use crate::computer::types::TaskKind;
use crate::computer::types::TerminalBackend;
use crate::registry::types::{
    FinalizedToolset, SessionContext, ToolRegistryBuilder, ToolServerConfig,
};
use crate::types::TaskSnapshot;
use crate::types::ToolInput;
use crate::types::agents_md_tracker::AgentsMdTracker;
use crate::types::definition::ToolDefinition;
use crate::types::output::{ToolOutput, ToolRunResult};
use crate::types::resources::{OwnerSessionId, State, Terminal};
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::ToolKind;

#[derive(Debug)]
pub struct ToolBridgeResult {
    /// Clean tool output — for JSON serialization, ACP conversion, hunk tracking.
    pub output: ToolOutput,
    /// Same output with system reminders appended, for the model prompt.
    pub prompt_text: String,
}

impl From<ToolRunResult> for ToolBridgeResult {
    fn from(result: ToolRunResult) -> Self {
        Self {
            output: result.output,
            prompt_text: result.prompt_text,
        }
    }
}

/// All tool state lives in `Resources` on the registry — there is no separate
/// `ToolState`.
///
/// # Cancellation Safety
///
/// `terminal` is held here as well as in the registry: while a bash command
/// runs, `call()` holds the registry lock, and `kill_foreground_commands()`
/// must reach the terminal without blocking on it.
#[derive(Clone)]
pub struct ToolBridge {
    registry: Arc<FinalizedToolset>,
    terminal: Option<Arc<dyn TerminalBackend>>,
}

impl ToolBridge {
    pub fn get_builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::new()
    }

    pub async fn finalize_builder(
        builder: ToolRegistryBuilder,
        config: ToolServerConfig,
        ctx: SessionContext,
    ) -> Result<Self, kigi_tool_runtime::ToolError> {
        let finalized_toolset = builder.finalize(config, ctx).map_err(|errs| {
            kigi_tool_runtime::ToolError::invalid_arguments(format!(
                "Requirements unsatisfied: {errs:?}"
            ))
        })?;

        let terminal;
        {
            terminal = finalized_toolset
                .resources
                .lock()
                .await
                .get::<Terminal>()
                .map(|t| t.0.clone());
        }

        Ok(Self {
            registry: Arc::new(finalized_toolset),
            terminal,
        })
    }

    pub async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry.tool_definitions()
    }

    /// Client-facing name of the tool registered with the given `ToolKind`,
    /// for "does this agent have a way to do X?" checks where X is identified
    /// by kind rather than by namespaced id. `None` when no registered tool
    /// has that kind.
    pub async fn tool_for_kind(&self, kind: ToolKind) -> Option<String> {
        self.registry
            .resources
            .lock()
            .await
            .get::<TemplateRenderer>()
            .and_then(|r| r.tool_for_kind(kind).map(str::to_string))
    }

    /// [`ToolKind`] for a registered tool by client-facing name, or `None` for
    /// unknown names. Name matching is exact and case-sensitive.
    pub fn tool_kind(&self, tool_name: &str) -> Option<ToolKind> {
        self.registry.get_tool_metadata(tool_name).map(|m| m.kind())
    }

    pub async fn tool_definitions_builtins_only(&self) -> Vec<ToolDefinition> {
        self.registry.tool_definitions_builtins_only()
    }

    /// The template may mix `${{ tools.by_kind.* }}`, resolved from the
    /// finalized registry, with caller-provided fields such as
    /// `${{ os_name }}` supplied in `placeholders`.
    ///
    /// `None` when the renderer is not yet available.
    pub async fn render_prompt(
        &self,
        template: &str,
        placeholders: &serde_json::Value,
    ) -> Option<String> {
        let registry = &*self.registry;
        let result;
        {
            result = registry
                .resources
                .lock()
                .await
                .get::<TemplateRenderer>()
                .and_then(|r| r.render_with_extra(template, placeholders).ok());
        }
        result
    }

    pub async fn register_mcp_tools<T>(
        &self,
        mcp_name: String,
        tool: T,
        input_schema: Option<serde_json::Value>,
    ) -> Result<(), kigi_tool_runtime::ToolError>
    where
        T: kigi_tool_runtime::Tool
            + crate::types::tool_metadata::ToolMetadata
            + std::fmt::Debug
            + Send
            + Sync
            + 'static,
        T::Output: serde::Serialize,
    {
        self.registry.register_tool(mcp_name, tool, input_schema)?;
        Ok(())
    }

    pub fn unregister_tools_by_prefix(&self, prefix: &str) -> usize {
        self.registry.unregister_tools_by_prefix(prefix)
    }

    pub fn unregister_tool_by_name(&self, name: &str) -> bool {
        self.registry.unregister_tool_by_name(name)
    }

    pub fn toolset(&self) -> Arc<FinalizedToolset> {
        Arc::clone(&self.registry)
    }

    pub async fn call(
        &self,
        client_function_name: &str,
        client_params: serde_json::Value,
        tool_call_id: &str,
    ) -> Result<ToolRunResult, kigi_tool_runtime::ToolError> {
        self.registry
            .call(client_function_name, client_params, tool_call_id, None)
            .await
    }

    pub async fn try_parse(
        &self,
        client_function_name: &str,
        client_params: serde_json::Value,
    ) -> Result<ToolInput, kigi_tool_runtime::ToolError> {
        self.registry
            .try_parse(client_function_name, &client_params)
            .await
    }

    /// `compat` gates which rules dirs and agent filenames runtime discovery
    /// scans; callers default it to all-on.
    pub async fn seed_agents_md(
        &self,
        initial_paths: Vec<std::path::PathBuf>,
        git_root: Option<std::path::PathBuf>,
        initial_chain: Vec<std::path::PathBuf>,
        gitignore: Option<ignore::gitignore::Gitignore>,
        compat: crate::types::compat::CompatConfig,
    ) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<AgentsMdTracker>() {
            tracker.set_compat(compat);
            tracker
                .seed(initial_paths, git_root, initial_chain, gitignore)
                .await;
        }
    }

    /// Must run BEFORE `seed_skill_discovery()` so that `seed()` sees
    /// non-empty `announced_names` and skips the BaselineChange pending.
    pub async fn restore_announced_skill_names(&self, names: std::collections::HashSet<String>) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        let tracker = res.get_or_default::<crate::types::skill_discovery_tracker::SkillManager>();
        tracker.restore_announced_names(names);
    }

    pub async fn get_announced_skill_names(&self) -> std::collections::HashSet<String> {
        let registry = &*self.registry;
        let res = registry.resources.lock().await;
        res.get::<crate::types::skill_discovery_tracker::SkillManager>()
            .map(|t| t.announced_names().clone())
            .unwrap_or_default()
    }

    /// The model-facing skill listing rendered from the current skill set,
    /// for `/context` accounting. `None` when no skill qualifies.
    pub async fn skill_listing_snapshot(
        &self,
    ) -> Option<crate::types::skill_discovery_tracker::SkillListingSnapshot> {
        let registry = &*self.registry;
        let res = registry.resources.lock().await;
        res.get::<crate::types::skill_discovery_tracker::SkillManager>()
            .and_then(|t| t.listing_snapshot())
    }

    /// Must run at session start so the `SkillDiscoveryReminder` can discover
    /// skills in subdirectories.
    ///
    /// `display_cwd`: when set (forked sessions), skill paths in model-visible
    /// announcements are rewritten from the real cwd to this value. Runtime
    /// invocation still uses the real path.
    pub async fn seed_skill_discovery(
        &self,
        cwd: Option<std::path::PathBuf>,
        git_root: Option<std::path::PathBuf>,
        startup_skills: Vec<crate::implementations::skills::types::SkillInfo>,
        display_cwd: Option<String>,
        context_window_tokens: Option<u64>,
        skill_budget_percent: Option<f64>,
        compat: crate::types::compat::CompatConfig,
    ) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        // Listing headers and descriptions must use the client-facing names,
        // which may be randomized per session.
        let renderer = res.get::<TemplateRenderer>();
        let skill_tool_name = renderer.and_then(|r| r.render("${{ tools.by_kind.skill }}").ok());
        let read_tool_name = renderer.and_then(|r| r.render("${{ tools.by_kind.read }}").ok());
        let tracker = res.get_or_default::<crate::types::skill_discovery_tracker::SkillManager>();
        if let Some(name) = skill_tool_name {
            tracker.set_skill_tool_name(name);
        }
        if let Some(name) = read_tool_name {
            tracker.set_read_tool_name(name);
        }
        tracker.set_compat(compat);
        tracker.seed(
            cwd,
            git_root,
            startup_skills,
            display_cwd,
            context_window_tokens,
            skill_budget_percent,
        );
    }

    /// When enabled, `take_pending()` produces `<agent_skill>` XML rows
    /// instead of markdown, matching the startup `<agent_skills>` preamble.
    pub async fn set_skill_listing_xml_format(&self, enabled: bool) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<crate::types::skill_discovery_tracker::SkillManager>()
        {
            tracker.set_xml_format(enabled);
        }
    }

    /// Seed the gitignore filter so `read_file` and `search_replace` refuse
    /// to access gitignored paths (matching `list_dir`/`grep` behavior).
    pub async fn seed_gitignore_filter(
        &self,
        gitignore: ignore::gitignore::Gitignore,
        git_root: std::path::PathBuf,
    ) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        res.insert(crate::types::resources::GitignoreFilter::new(
            gitignore, git_root,
        ));
    }

    pub async fn on_agents_md_compaction(&self) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<AgentsMdTracker>() {
            tracker.on_compaction();
        }
    }

    /// Clears `announced_names` and `checked_dirs` so skills are re-announced
    /// and re-discovered after compaction drops them from the conversation.
    pub async fn on_skill_discovery_compaction(&self) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<crate::types::skill_discovery_tracker::SkillManager>()
        {
            tracker.on_compaction();
        }
    }

    /// Full reset of skill discovery state for /clear: the startup baseline
    /// survives and a pending reconciliation is queued.
    pub async fn on_skill_discovery_clear(&self) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<crate::types::skill_discovery_tracker::SkillManager>()
        {
            tracker.on_clear();
        }
    }

    /// Replace the startup baseline on plugin reload: dynamic discoveries
    /// survive and a pending reconciliation is queued.
    pub async fn update_skill_baseline(
        &self,
        new_skills: Vec<crate::implementations::skills::types::SkillInfo>,
    ) {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        if let Some(tracker) = res.get_mut::<crate::types::skill_discovery_tracker::SkillManager>()
        {
            tracker.update_startup_baseline(new_skills);
        }
    }

    /// Applies a pending change (discovery, baseline update, /clear) by
    /// writing the runtime projection into `AvailableSkills` and handing back
    /// the conversation/UI side-effects for the session to execute:
    /// system-reminder injection, slash command refresh, prompt finalization.
    ///
    /// `None` when nothing is pending.
    pub async fn apply_pending_skill_update(
        &self,
    ) -> Option<crate::types::skill_discovery_tracker::SkillUpdateEffects> {
        let registry = &*self.registry;
        let mut res = registry.resources.lock().await;
        let tracker = res.get_mut::<crate::types::skill_discovery_tracker::SkillManager>()?;
        let (runtime_skills, effects) = tracker.take_pending()?;

        // The runtime projection stays inside Resources; the shell only ever
        // sees the effects returned below.
        res.insert(crate::types::resources::AvailableSkills(runtime_skills));

        Some(effects)
    }

    /// Combined startup + discovered skills with canonical-path and name
    /// dedup applied. Authoritative source for slash command advertisement —
    /// `PromptContext` is NOT used for it.
    pub async fn slash_skills(&self) -> Vec<crate::implementations::skills::types::SkillInfo> {
        let registry = &*self.registry;
        let res = registry.resources.lock().await;
        res.get::<crate::types::skill_discovery_tracker::SkillManager>()
            .map(|m| m.slash_skills())
            .unwrap_or_default()
    }

    pub async fn agents_md_reminded_paths(&self) -> std::collections::HashSet<std::path::PathBuf> {
        let registry = &*self.registry;
        let result;
        {
            result = registry
                .resources
                .lock()
                .await
                .get::<AgentsMdTracker>()
                .map(|t| t.reminded_paths().clone())
                .unwrap_or_default();
        }
        result
    }

    /// Stable display path for forked sessions. Tools read the inserted
    /// [`DisplayCwd`] through `resolve_model_path` / `display_cwd_or_cwd` to
    /// rewrite model-provided paths and format output paths.
    pub async fn set_display_cwd(&self, display_cwd: std::path::PathBuf) {
        let registry = &*self.registry;
        registry
            .resources
            .lock()
            .await
            .insert(crate::types::resources::DisplayCwd(display_cwd));
    }

    /// Feeds context compaction, which folds task state into summaries.
    pub async fn list_background_tasks(&self) -> Vec<crate::computer::types::TaskSnapshot> {
        if let Some(terminal) = &self.terminal {
            terminal.list_tasks().await
        } else {
            vec![]
        }
    }

    pub async fn kill_foreground_commands(&self) {
        if let Some(terminal) = &self.terminal {
            terminal.kill_foreground_commands().await;
        }
    }

    pub async fn kill_foreground_commands_by_owner(&self, owner_session_id: &str) {
        if let Some(terminal) = &self.terminal {
            terminal
                .kill_foreground_commands_by_owner(owner_session_id)
                .await;
        }
    }

    pub async fn kill_all_background_tasks(&self) {
        if let Some(terminal) = &self.terminal {
            terminal.kill_all_background_tasks().await;
        }
    }

    /// Owner-scoped so subagent teardown spares the tasks of other sessions
    /// sharing the terminal backend.
    pub async fn kill_all_background_tasks_by_owner(&self, owner_session_id: &str) {
        if let Some(terminal) = &self.terminal {
            terminal
                .kill_all_background_tasks_by_owner(owner_session_id)
                .await;
        }
    }

    pub async fn reparent_notifications(
        &self,
        old_owner_session_id: &str,
        new_owner_session_id: &str,
        new_handle: crate::notification::types::ToolNotificationHandle,
    ) {
        if let Some(terminal) = &self.terminal {
            // Weak, so the reparented notifications cannot keep the backend
            // alive past the session that owns this `Arc`.
            let backend_weak = std::sync::Arc::downgrade(terminal);
            terminal
                .reparent_notifications(
                    old_owner_session_id,
                    new_owner_session_id,
                    new_handle,
                    backend_weak,
                )
                .await;
        }
    }

    /// `None` if the resource type has never been inserted. The resource is
    /// cloned so no lock is held once this returns.
    pub async fn read_resource<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.registry.resources.lock().await.get::<T>().cloned()
    }
    pub async fn shared_resources(&self) -> crate::types::resources::SharedResources {
        self.registry.resources.clone()
    }

    pub async fn update_resource<T: Send + Sync + 'static>(&self, resource: T) {
        let _ = self.registry.update_resource(resource).await;
    }

    pub async fn kill_background_task(
        &self,
        task_id: &str,
    ) -> Result<KillOutcome, kigi_tool_runtime::ToolError> {
        if let Some(terminal) = &self.terminal {
            Ok(terminal.kill_task(task_id).await)
        } else {
            Err(kigi_tool_runtime::ToolError::invalid_arguments(format!(
                "Missing Task Id: {task_id}"
            )))
        }
    }

    pub async fn delete_scheduled_task(
        &self,
        task_id: &str,
    ) -> Result<bool, kigi_tool_runtime::ToolError> {
        use crate::implementations::kigi::scheduler::types::{SchedulerCommand, SchedulerHandle};
        let sender = {
            let res = self.registry.resources.lock().await;
            res.get::<SchedulerHandle>()
                .ok_or_else(|| {
                    kigi_tool_runtime::ToolError::custom("missing_resource", "SchedulerHandle")
                })?
                .0
                .clone()
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(SchedulerCommand::Delete {
                id: task_id.to_owned(),
                reply: reply_tx,
            })
            .map_err(|_| {
                kigi_tool_runtime::ToolError::custom("process_manager", "Scheduler actor stopped")
            })?;
        reply_rx.await.map_err(|_| {
            kigi_tool_runtime::ToolError::custom("process_manager", "Scheduler actor dropped reply")
        })
    }

    /// Returns `true` if a matching foreground process was found and unblocked.
    pub async fn background_foreground_command(&self, tool_call_id: &str) -> bool {
        if let Some(terminal) = &self.terminal {
            terminal.background_foreground_command(tool_call_id).await
        } else {
            false
        }
    }

    /// `None` when this bridge has no terminal backend at all, as opposed to
    /// an empty task list.
    pub async fn list_tasks(&self) -> Option<Vec<TaskSnapshot>> {
        if let Some(terminal) = &self.terminal {
            Some(terminal.list_tasks().await)
        } else {
            None
        }
    }

    /// Drain newly-completed bash background tasks not yet reported. Returned
    /// tasks are marked in [`ReportedTaskCompletions`] so
    /// [`TaskCompletionReminder`] does not report them a second time.
    pub async fn drain_between_turn_bash_completions(&self) -> Vec<TaskSnapshot> {
        let tasks = match self.list_tasks().await {
            Some(t) => t,
            None => return Vec::new(),
        };
        let completed: Vec<TaskSnapshot> = tasks
            .into_iter()
            .filter(|t| t.completed && t.kind != TaskKind::Monitor)
            .collect();
        if completed.is_empty() {
            return Vec::new();
        }

        use crate::reminders::task_completion::{ReportedTaskCompletions, task_owned_by_session};

        let mut res = self.registry.resources.lock().await;
        // Subagents share the parent's terminal backend, so `list_tasks()`
        // also returns tasks owned by other sessions; without the owner scope
        // a parent or sibling bash task finishing mid-subagent-turn leaks its
        // "While you were idle, … background task completed"
        // `<system-reminder>` into the subagent's conversation. The owner
        // filter runs before `mark_reported` so the owning session still
        // reports the task on its own next turn.
        let my_owner = res.get::<OwnerSessionId>().map(|o| o.0.clone());
        let state = res.get_or_default::<State<ReportedTaskCompletions>>();
        completed
            .into_iter()
            .filter(|t| task_owned_by_session(t, my_owner.as_deref()))
            .filter(|t| state.mark_reported(&t.task_id))
            .collect()
    }

    /// Minimal bridge with no tools registered. Bypasses
    /// `ToolRegistryBuilder::finalize()`, which `tokio::spawn`s background
    /// tasks, so sync `#[test]` functions without a runtime can call it.
    pub fn for_test() -> Self {
        let toolset = FinalizedToolset::empty_for_test();
        Self {
            registry: Arc::new(toolset),
            terminal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::{BackgroundHandle, TerminalRunRequest, TerminalRunResult};
    use crate::reminders::task_completion::ReportedTaskCompletions;
    use std::time::Duration;

    #[derive(Debug)]
    struct KindFixture {
        kind: ToolKind,
        id: &'static str,
    }

    impl crate::types::tool_metadata::ToolMetadata for KindFixture {
        fn kind(&self) -> ToolKind {
            self.kind
        }
        fn tool_namespace(&self) -> crate::types::tool::ToolNamespace {
            crate::types::tool::ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "kind fixture"
        }
    }

    impl kigi_tool_runtime::Tool for KindFixture {
        type Args = serde_json::Value;
        type Output = String;

        fn id(&self) -> kigi_tool_protocol::ToolId {
            kigi_tool_protocol::ToolId::new(self.id).expect("valid id")
        }
        fn description(
            &self,
            _ctx: &::kigi_tool_runtime::ListToolsContext,
        ) -> kigi_tool_types::ToolDescription {
            kigi_tool_types::ToolDescription::new(self.id, "kind fixture")
        }
        async fn run(
            &self,
            _ctx: kigi_tool_runtime::ToolCallContext,
            _input: serde_json::Value,
        ) -> Result<String, kigi_tool_runtime::ToolError> {
            Ok("ok".into())
        }
    }

    fn register_fixture(toolset: &FinalizedToolset, name: &str, kind: ToolKind, id: &'static str) {
        toolset
            .register_tool(
                name.into(),
                KindFixture { kind, id },
                Some(serde_json::json!({"type": "object", "properties": {}})),
            )
            .unwrap();
    }

    #[test]
    fn tool_kind_returns_registered_kind_per_namespace_and_none_for_unknown() {
        let bridge = ToolBridge::for_test();
        let toolset = bridge.toolset();

        // PascalCase and snake_case in one registry, to exercise the lookup on
        // the literal name strings each namespace ships.
        register_fixture(&toolset, "Write", ToolKind::Write, "fixture_write");
        register_fixture(
            &toolset,
            "StrReplace",
            ToolKind::Edit,
            "fixture_str_replace",
        );
        register_fixture(&toolset, "Delete", ToolKind::Delete, "fixture_delete");
        register_fixture(
            &toolset,
            "run_terminal_cmd",
            ToolKind::Execute,
            "fixture_run_terminal_cmd",
        );

        assert_eq!(bridge.tool_kind("Write"), Some(ToolKind::Write));
        assert_eq!(bridge.tool_kind("StrReplace"), Some(ToolKind::Edit));
        assert_eq!(bridge.tool_kind("Delete"), Some(ToolKind::Delete));
        assert_eq!(
            bridge.tool_kind("run_terminal_cmd"),
            Some(ToolKind::Execute)
        );

        assert_eq!(bridge.tool_kind("not_a_registered_tool"), None);
        assert_eq!(bridge.tool_kind("write"), None);
    }

    #[derive(Debug)]
    struct MockTerminal {
        tasks: Vec<TaskSnapshot>,
    }

    #[async_trait::async_trait]
    impl TerminalBackend for MockTerminal {
        async fn run(
            &self,
            _: TerminalRunRequest,
        ) -> Result<TerminalRunResult, crate::computer::types::ComputerError> {
            unimplemented!()
        }
        async fn run_background(
            &self,
            _: TerminalRunRequest,
        ) -> Result<BackgroundHandle, crate::computer::types::ComputerError> {
            unimplemented!()
        }
        async fn kill_task(&self, _: &str) -> KillOutcome {
            KillOutcome::NotFound
        }
        async fn get_task(&self, _: &str) -> Option<TaskSnapshot> {
            None
        }
        async fn wait_for_completion(&self, _: &str, _: Option<Duration>) -> Option<TaskSnapshot> {
            None
        }
        async fn list_tasks(&self) -> Vec<TaskSnapshot> {
            self.tasks.clone()
        }
    }

    fn completed_task(id: &str, owner: Option<&str>) -> TaskSnapshot {
        TaskSnapshot {
            task_id: id.into(),
            command: "echo test".into(),
            display_command: None,
            cwd: String::new(),
            start_time: std::time::SystemTime::now(),
            end_time: Some(std::time::SystemTime::now()),
            output: String::new(),
            output_file: std::path::PathBuf::new(),
            truncated: false,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind: Default::default(),
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: owner.map(|s| s.to_string()),
        }
    }

    /// Regression: subagents share the parent's terminal backend, so the
    /// between-turn drain must not surface another session's completed bash
    /// task (which leaked as a "While you were idle, 1 background task
    /// completed" `<system-reminder>` into the subagent's conversation).
    #[tokio::test]
    async fn between_turn_bash_completions_scoped_to_owning_session() {
        let toolset = FinalizedToolset::empty_for_test();
        {
            let mut res = toolset.resources.lock().await;
            res.insert(OwnerSessionId("subagent-1".into()));
            res.register_state::<ReportedTaskCompletions>();
        }
        let backend: Arc<dyn TerminalBackend> = Arc::new(MockTerminal {
            tasks: vec![
                completed_task("mine-task", Some("subagent-1")),
                completed_task("parent-task", Some("parent-0")),
                completed_task("unowned-task", None),
            ],
        });
        let bridge = ToolBridge {
            registry: Arc::new(toolset),
            terminal: Some(backend),
        };

        let drained = bridge.drain_between_turn_bash_completions().await;
        let ids: Vec<&str> = drained.iter().map(|t| t.task_id.as_str()).collect();

        assert!(ids.contains(&"mine-task"), "own task must drain: {ids:?}");
        assert!(
            ids.contains(&"unowned-task"),
            "unowned task must drain (backwards compat): {ids:?}"
        );
        assert!(
            !ids.contains(&"parent-task"),
            "another session's task must NOT leak into this session: {ids:?}"
        );
    }
}
