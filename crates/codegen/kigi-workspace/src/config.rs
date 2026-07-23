//! Workspace and session configuration types.
use crate::capability::CapabilityMode;
use kigi_tools::registry::types::{SessionContext, ToolRegistryBuilder, ToolServerConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
/// Default capacity for the workspace event broadcast channel.
pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 64;
/// A session-lifetime terminal backend paired with its explicit shutdown hook.
///
/// The backend (background-task registry + persistent shell) is owned by the
/// [`WorkspaceSession`](crate::session::WorkspaceSession) and injected into
/// every toolset re-resolve for that session, so background tasks and shell
/// state survive toolset swaps. The shutdown hook fires the backend's cancel
/// token — killing every child process group and stopping the actor — so
/// `drop_session`/evict teardown is an explicit act rather than a side effect
/// of the last `Arc` drop.
#[derive(Clone)]
pub struct SessionTerminalBackend {
    backend: Arc<dyn kigi_tools::computer::types::TerminalBackend>,
    shutdown: Arc<dyn Fn() + Send + Sync>,
}
impl SessionTerminalBackend {
    /// Pair an already-erased `backend` with its shutdown hook.
    ///
    /// Extension point for [`SessionContextFactory`] implementors whose
    /// backend is not a `LocalTerminalBackend` (the fields are private, so
    /// this is the only way to satisfy `build_terminal_backend` for other
    /// backend types); in-repo factories use [`Self::local`].
    pub fn new(
        backend: Arc<dyn kigi_tools::computer::types::TerminalBackend>,
        shutdown: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self { backend, shutdown }
    }
    /// Wrap a [`LocalTerminalBackend`], wiring the shutdown hook to its
    /// cancel token.
    ///
    /// [`LocalTerminalBackend`]: kigi_tools::computer::local::LocalTerminalBackend
    pub fn local(backend: kigi_tools::computer::local::LocalTerminalBackend) -> Self {
        let canceller = backend.clone();
        Self {
            backend: Arc::new(backend),
            shutdown: Arc::new(move || canceller.cancel()),
        }
    }
    /// The type-erased backend, as injected into toolset resolves.
    pub fn backend(&self) -> &Arc<dyn kigi_tools::computer::types::TerminalBackend> {
        &self.backend
    }
    /// Explicitly shut the backend down: kills all of its child process
    /// groups and stops its actor.
    pub fn shutdown(&self) {
        (self.shutdown)();
    }
}
impl std::fmt::Debug for SessionTerminalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTerminalBackend")
            .finish_non_exhaustive()
    }
}
/// Pluggable producer of [`SessionContext`] / [`ToolRegistryBuilder`]
/// for each session.
///
/// The workspace itself doesn't know how to construct the tool runtime
/// (terminal backend, file system, persistence path, MCP client config,
/// notification handle, ...) -- those come from the embedder (TUI, SDK,
/// or remote sampler). The embedder hands us a factory at
/// `WorkspaceHandle::new` time and we call it on every session
/// resolution.
pub trait SessionContextFactory: Send + Sync {
    /// Build a fresh [`SessionContext`] for the given session, around the
    /// given terminal `backend` (constructing one here would waste an actor
    /// per resolve — the pipeline rebuilds toolsets around the session-owned
    /// backend, so the caller always supplies it).
    fn build_session_context(
        &self,
        session_id: &str,
        cwd: PathBuf,
        session_env: Arc<HashMap<String, String>>,
        backend: Arc<dyn kigi_tools::computer::types::TerminalBackend>,
    ) -> SessionContext;
    /// Build the session-lifetime terminal backend for a new session.
    /// Called once per session create/fork; toolset re-resolves reuse the
    /// session's stored backend instead of building another.
    fn build_terminal_backend(&self) -> SessionTerminalBackend;
    /// Build a fresh [`ToolRegistryBuilder`] with the workspace's
    /// full set of registered tools.
    fn registry_builder(&self) -> ToolRegistryBuilder;
    fn known_tool_ids(&self) -> Arc<std::collections::HashSet<String>> {
        Arc::new(self.registry_builder().known_tool_ids())
    }
}
/// Placeholder for the cross-session memory backend config.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MemoryConfig {}
/// Top-level config required to construct a [`crate::handle::WorkspaceHandle`].
///
/// `#[non_exhaustive]` so future fields are non-breaking.
#[non_exhaustive]
pub struct WorkspaceConfig {
    /// Workspace root directory.
    pub root_cwd: PathBuf,
    /// Baseline tool config for the main session.
    pub default_tool_config: ToolServerConfig,
    /// Whether session-scoped fs operations should respect `.gitignore`.
    pub respect_gitignore: bool,
    /// Optional cross-session memory config.
    pub memory_config: Option<MemoryConfig>,
    /// Capacity of the workspace event broadcast channel.
    pub event_buffer_capacity: usize,
    /// Pluggable [`SessionContext`] / [`ToolRegistryBuilder`] producer.
    pub session_factory: Arc<dyn SessionContextFactory>,
    /// Global hook sources (e.g. `~/.claude/settings.json`, `~/.kigi/hooks/`).
    pub hook_global_sources: Vec<HookSourceConfig>,
    /// Project-scoped hook sources (e.g. `<project>/.kigi/hooks/`).
    pub hook_project_sources: Vec<HookSourceConfig>,
    /// Skill discovery configuration: additional skill paths and
    /// path-prefix ignore list. Stored on `WorkspaceShared` for
    /// `discover_skills` calls. Defaults to empty (no extra paths,
    /// no ignores).
    pub skills_config: crate::discovery::SkillsConfig,
    /// Plugin discovery configuration: CLI plugin dirs, config paths,
    /// and disabled/enabled lists. Stored on `WorkspaceShared` for
    /// `discover_plugins` calls. Defaults to empty.
    pub plugin_discovery_config: crate::discovery::PluginDiscoveryConfig,
    /// Runtime-tunable timing/threshold config for the tool server.
    pub status_config: crate::status_config::StatusConfig,
    /// Folder-trust verdict for repo-local (project-scoped) LSP servers from
    /// `<cwd>/.kigi/lsp.json`: `false` drops them at load, `true` keeps them. The
    /// shell caller resolves the verdict and threads it in; callers without a
    /// folder-trust decision pass `true`.
    pub project_lsp_trusted: bool,
    /// Confine `kigi/fs/*` / `workspace.fs_*` resolution to the workspace root
    /// (reject `..`, absolute-outside-root, symlink escapes). Default `false`
    /// (unconfined) — set to `true` only by the workspace server on a remote
    /// sandbox, where the root is a real tenant boundary.
    pub confine_fs_to_workspace_root: bool,
}
/// Configuration for spawning a subagent session within a workspace.
#[derive(Clone)]
#[non_exhaustive]
pub struct AgentSessionConfig {
    /// Unique agent session id. Must be non-empty.
    pub agent_id: String,
    /// Filesystem isolation strategy.
    pub isolation: IsolationMode,
    /// Capability mode applied to this session's toolset.
    pub capability_mode: CapabilityMode,
    /// Per-fork tool override. `None` inherits from parent.
    pub tool_config: Option<ToolServerConfig>,
    /// Maximum recursion depth for subagent nesting.
    pub max_depth: u32,
    /// Working directory override. `None` inherits the parent's `cwd`.
    pub cwd_override: Option<PathBuf>,
    /// Extra env vars to layer on top of the parent's `session_env`.
    pub extra_env: HashMap<String, String>,
    /// Parent session to inherit from. Required.
    pub parent_session_id: Option<String>,
}
impl AgentSessionConfig {
    /// Construct a config with the supplied `agent_id` and otherwise
    /// minimal/permissive defaults.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            isolation: IsolationMode::None,
            capability_mode: CapabilityMode::ReadWrite,
            tool_config: None,
            max_depth: u32::MAX,
            cwd_override: None,
            extra_env: HashMap::new(),
            parent_session_id: None,
        }
    }
}
/// WARNING: `tool_config` is deliberately redacted from `Debug` output
/// because `ToolServerConfig.tools[*].params` may contain credentials.
impl std::fmt::Debug for AgentSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSessionConfig")
            .field("agent_id", &self.agent_id)
            .field("isolation", &self.isolation)
            .field("capability_mode", &self.capability_mode)
            .field(
                "tool_config",
                if self.tool_config.is_some() {
                    &"Some(<redacted>)"
                } else {
                    &"None"
                },
            )
            .field("max_depth", &self.max_depth)
            .field("cwd_override", &self.cwd_override)
            .field("extra_env", &self.extra_env)
            .field("parent_session_id", &self.parent_session_id)
            .finish()
    }
}
/// A single hook source: either a JSON settings file or a directory of
/// `*.json` hook files. Maps 1:1 to [`kigi_hooks::discovery::HookSource`]
/// but uses owned `PathBuf` so the config struct is `'static`.
#[derive(Debug, Clone)]
pub enum HookSourceConfig {
    /// A single JSON settings file (e.g. `~/.claude/settings.json`).
    SettingsFile(PathBuf),
    /// A directory of `*.json` hook files (e.g. `~/.kigi/hooks/`).
    Directory(PathBuf),
}
/// Filesystem isolation strategy for a forked session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationMode {
    /// No isolation: subagent shares the parent's working tree.
    #[default]
    None,
    /// Run the subagent in a copy-on-write git worktree.
    Worktree,
    /// Run the subagent inside a sandbox/container.
    Sandbox,
}
