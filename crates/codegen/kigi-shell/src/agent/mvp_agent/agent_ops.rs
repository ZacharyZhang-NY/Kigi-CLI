#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! Inherent [`MvpAgent`] helpers (MCP/clients/gateway, settings/models, session ops, spawn).
//! Co-located child of `mvp_agent` (`use super::*`).
use super::*;
impl MvpAgent {
    pub(super) fn resolve_image_description_model(&self) -> String {
        self.cfg
            .borrow()
            .image_description_model
            .as_deref()
            .unwrap_or(crate::models::default_image_description_model())
            .to_owned()
    }
    fn resolve_session_summary_model(&self) -> String {
        self.cfg
            .borrow()
            .session_summary_model
            .as_deref()
            .unwrap_or(crate::models::default_session_summary_model())
            .to_owned()
    }
    pub(super) fn build_summary_client(
        &self,
        primary: &SamplingConfig,
    ) -> Result<(OaiCompatClient, String), acp::Error> {
        let slug = self.resolve_session_summary_model();
        // Resolve the aux token by the summary model's OWN platform: a grok
        // (oauth-platform) summary model draws its pooled grok token or `None`
        // — NEVER the primary Kimi session token (which `resolve_credentials`
        // would otherwise stamp onto an api.x.ai request). A first-party /
        // non-oauth summary model still gets the primary (byte-identical).
        let session_key = crate::auth::oauth_registry::session_key_for_model(
            &crate::util::kigi_home::kigi_home(),
            &slug,
            Some(&self.auth_manager),
        );
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let alpha_test_key = self.cfg.borrow().endpoints.alpha_test_key.clone();
        let config = match crate::agent::config::resolve_aux_model_sampling_config(
            &slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            alpha_test_key,
        ) {
            Some(mut cfg) => {
                cfg.attribution_callback = primary.attribution_callback.clone();
                cfg.bearer_resolver = primary.bearer_resolver.clone();
                cfg.max_retries = primary.max_retries;
                cfg
            }
            None => {
                let mut fallback = primary.clone();
                fallback.model = slug;
                fallback
            }
        };
        let model = config.model.clone();
        let client = OaiCompatClient::new(config).map_err(map_sampling_err_to_acp)?;
        Ok((client, model))
    }
    /// `true` for session-based ACP auth methods.
    fn is_session_based_auth(&self) -> bool {
        self.auth_method_id
            .load()
            .as_deref()
            .is_some_and(crate::agent::auth_method::is_session_based_method)
    }
    /// Publish the current ACP auth method into the shared live handle so every
    /// running session's per-turn auth gate observes it on its next turn.
    pub(super) fn set_auth_method(&self, id: acp::AuthMethodId) {
        self.auth_method_id.store(Some(std::sync::Arc::new(id)));
    }
    /// Return auth for sync config construction.
    pub(super) fn current_or_buffered_auth(&self) -> Option<crate::auth::KimiAuth> {
        self.auth_manager
            .current()
            .or_else(|| {
                if self.is_session_based_auth() {
                    let auth = self.auth_manager.expired_auth();
                    if auth.is_some() {
                        kigi_log::unified_log::info(
                            "auth buffered token fallback",
                            None,
                            None,
                        );
                    }
                    auth
                } else {
                    None
                }
            })
    }
    /// Resolve the launch dir's project-scope trust verdict ONCE and return it
    /// with its path.
    ///
    /// Memoizes the single [`folder_trust::resolve_launch_dir_trust`] gather (see
    /// it for the dedup + TOCTOU contract) so the two one-shot init helpers
    /// (`ensure_plugin_registry` and `ensure_local_workspace_ops`) share it
    /// instead of each re-scanning. They share a single point-in-time verdict
    /// rather than two independent re-scans; the sub-millisecond, startup-only
    /// window between them is intentional (the cross-session TOCTOU re-scan is
    /// preserved per the contract).
    fn prime_launch_dir_trust(&self) -> (&std::path::Path, bool) {
        let trust = *self
            .launch_dir_trust
            .get_or_init(|| {
                let remote_settings = self.cfg.borrow().remote_settings.clone();
                folder_trust::resolve_launch_dir_trust(
                    &self.launch_cwd,
                    remote_settings.as_ref(),
                )
            });
        (&self.launch_cwd, trust)
    }
    /// Resolve folder trust and load launch-dir MCP configs after `initialize`
    /// returns. The walks are synchronous and expensive in large monorepos; they
    /// must not block the ACP response (kigi-desktop sends `initialize` immediately).
    pub(super) fn spawn_initialize_launch_mcp_setup(&self) {
        let cwd = self.launch_cwd.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        let gateway = self.gateway.clone();
        let agent_mcp_state = self.agent_mcp_state.clone();
        tokio::task::spawn_local(async move {
            let local_mcp_servers = match tokio::task::spawn_blocking(move || {
                    let local = crate::util::config::load_mcp_servers(&cwd, &compat);
                    folder_trust::resolve_and_record(
                        &cwd,
                        remote_settings.as_ref(),
                        false,
                    );
                    folder_trust::filter_untrusted_project_mcp(&cwd, local)
                })
                .await
            {
                Ok(servers) => servers,
                Err(e) => {
                    tracing::warn!(error = % e, "initialize MCP setup task failed");
                    return;
                }
            };
            if !local_mcp_servers.is_empty() {
                agent_mcp_state.lock().await.update_configs(local_mcp_servers.clone());
            }
            crate::extensions::mcp::notify_servers_updated(
                    &gateway,
                    &local_mcp_servers,
                )
                .await;
        });
    }
    pub fn agent_mcp_state(
        &self,
    ) -> std::sync::Arc<tokio::sync::Mutex<crate::session::mcp_servers::McpState>> {
        self.agent_mcp_state.clone()
    }
    /// Build the launch-dir plugin registry snapshot on first use.
    ///
    /// Boot-time discovery was deferred past ACP `initialize` (the cwd→git-root
    /// plus user/marketplace walks stalled kigi-desktop's first `initialize`),
    /// leaving `plugin_registry_handle` empty. That shared snapshot still backs
    /// the launch-dir plugin MCP/LSP merges read in `resolve_mcp_servers` and
    /// the session LSP build, so populate it lazily — off the `initialize`
    /// critical path — on the first session-creating call. Runs the discovery
    /// walk once; per-session `build_for_cwd` still re-resolves project-scoped
    /// plugins for each session's own cwd.
    pub(super) fn ensure_plugin_registry(&self) {
        if self.plugin_registry_initialized.replace(true) {
            return;
        }
        let (cwd, trusted) = self.prime_launch_dir_trust();
        let mut plugins = self.cfg.borrow().plugins.clone();
        plugins.merge_claude_enabled_plugins(Some(cwd));
        let disk_config = plugins.to_discovery_config();
        let count = self
            .plugin_registry_handle
            .reload(Some(cwd), &disk_config, trusted, false);
        tracing::debug!(
            plugin_count = count, "lazily populated plugin registry snapshot"
        );
    }
    /// Merge on-disk/plugin MCP servers with client servers.
    pub(super) fn resolve_mcp_servers(
        &self,
        client_servers: Vec<acp::McpServer>,
        cwd: &std::path::Path,
    ) -> Vec<acp::McpServer> {
        self.ensure_plugin_registry();
        crate::session::managed_mcp::merge_managed_mcp_servers(
            client_servers,
            cwd,
            self.plugin_registry_handle.snapshot().as_deref(),
            &self.cfg.borrow().compat_resolved,
        )
    }
    /// Set the memory configuration (called from TUI after config resolution).
    pub fn set_memory_config(&mut self, config: crate::config::MemoryConfig) {
        self.memory_config = if config.enabled { Some(config) } else { None };
    }
    /// Adopt the leader's [`AgentActivity`] so the auto-update checker sees
    /// the agent's live view of running turns/subagents and can flush
    /// sessions at shutdown.
    ///
    /// Must be called right after construction: entries registered on the
    /// constructor-created default instance are NOT migrated.
    pub fn set_activity(&mut self, activity: crate::agent::activity::AgentActivity) {
        self.subagent_coordinator
            .borrow_mut()
            .set_running_gauge(activity.subagent_gauge());
        self.activity = activity;
    }
    /// Install the channel that fans new session cwds into the leader's
    /// `ConfigFileWatcher::watch_path`. Called once after
    /// the watcher is constructed in `agent/app.rs`. In simple /
    /// non-leader mode the channel is never wired and
    /// `notify_session_cwd_for_watch` is a no-op.
    pub fn set_config_watcher_path_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
    ) {
        self.config_watcher_path_tx = Some(tx);
    }
    /// Best-effort fan-out of a new session's `cwd` to the leader's
    /// `ConfigFileWatcher` for dynamic non-recursive registration
    /// No-op if the channel was never installed
    /// (`set_config_watcher_path_tx` was not called — simple mode,
    /// tests) or if the receiver has been dropped. Watcher errors are
    /// logged inside the spawned task and do NOT propagate here.
    pub(crate) fn notify_session_cwd_for_watch(&self, cwd: &std::path::Path) {
        if let Some(tx) = self.config_watcher_path_tx.as_ref()
            && tx.send(cwd.to_path_buf()).is_err()
        {
            tracing::debug!(
                cwd = % cwd.display(),
                "config watcher path channel closed; session cwd not registered"
            );
        }
    }
    /// Feedback endpoint base when this is a subscription (OAuth) session —
    /// the Kimi Code feedback endpoint only takes the OAuth Bearer, so
    /// API-key-only setups get `None` (they are pointed at the issue
    /// tracker instead; kimi-cli slash.py parity).
    fn feedback_base_url(&self) -> Option<String> {
        let has_session = self
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth());
        has_session.then(|| self.cfg.borrow().endpoints.resolve_feedback_base_url())
    }
    /// Build a `FeedbackClient` for subscription sessions.
    pub(crate) fn feedback_client(&self) -> Option<FeedbackClient> {
        Some(FeedbackClient::new(
            self.feedback_base_url()?,
            self.auth_manager.clone(),
        ))
    }
    /// Build a `RegistryConfig` if the feature is enabled (for passing to persistence actor).
    pub(super) fn build_registry_config(
        &self,
    ) -> Option<crate::session::RegistryConfig> {
        let remote = self
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.session_registry_enabled);
        if !self.session_registry_local.or(remote).unwrap_or(false) {
            return None;
        }
        let auth = self.auth_manager.current_or_expired()?;
        if !auth.is_session_auth() {
            return None;
        }
        let key = auth.key.clone();
        let cfg = self.cfg.borrow();
        Some(crate::session::RegistryConfig {
            base_url: cfg.endpoints.proxy_url(),
            user_token: key,
            deployment_key: cfg.endpoints.deployment_key.clone(),
            alpha_test_key: cfg.endpoints.alpha_test_key.clone(),
        })
    }
    /// Build a `SessionRegistryClient` if the feature is enabled.
    /// Delegates to `build_registry_config()` for the enabled check + config.
    pub(crate) fn session_registry_client(
        &self,
    ) -> Option<crate::agent::session_registry_client::SessionRegistryClient> {
        let cfg = self.build_registry_config()?;
        Some(
            crate::agent::session_registry_client::SessionRegistryClient::new(
                    cfg.base_url,
                    cfg.user_token,
                )
                .with_deployment_key(cfg.deployment_key)
                .with_alpha_test_key(cfg.alpha_test_key)
                .with_auth(self.auth_manager.clone()),
        )
    }
    /// Pre-session command availability snapshot.
    ///
    /// Used by the `kigi/commands/list` ext method and the
    /// `InitializeResponse._meta` path (`builtin_commands()`), both of
    /// which fire before any session exists. The eventual agent's toolset
    /// is unknown (depends on the model the user picks), so we fail-closed
    /// for runtime/tool-dependent gates (`/flush`, `/loop`, `/memory`,
    /// …) and let the session-scoped `available_commands_update` in
    /// `acp_session.rs` fill in the real per-model gating as soon as a
    /// session starts.
    ///
    /// Exception: `/goal` is gated on the `resolve_goal()` feature flag
    /// (a config/managed-settings switch known at initialize time) plus
    /// the `update_goal` tool, which is part of the default coding-agent
    /// toolset. So when the flag is on we advertise `/goal` pre-session;
    /// otherwise it wouldn't appear in the slash menu until after the
    /// first user turn created a session.
    pub(crate) fn command_availability(
        &self,
    ) -> crate::session::slash_commands::CommandAvailability {
        let goal = self.cfg.borrow().resolve_goal().value;
        crate::session::slash_commands::CommandAvailability {
            goal,
            // Same convention as /goal: the flag is known at initialize
            // time, so advertise pre-session; the in-session path
            // re-checks the live toolset.
            graph: goal && self.cfg.borrow().resolve_graph().value,
            ..crate::session::slash_commands::CommandAvailability::default()
        }
    }
    /// Current client type as set by the most recent `initialize()` call.
    pub(crate) fn client_type(&self) -> ClientType {
        *self.client_type.borrow()
    }
    /// Most recently allocated turn number for `sid`, or `None` if the
    /// session has not started a turn yet.
    pub(crate) fn session_turn_number(&self, sid: &acp::SessionId) -> Option<u64> {
        self.session_turn_numbers.borrow().get(sid).copied()
    }
    /// Return the current KimiAuth credentials, if authenticated and not expired.
    pub(crate) fn current_auth(&self) -> Option<crate::auth::KimiAuth> {
        self.auth_manager.current()
    }
    /// Shared plugin registry handle used by extensions for snapshot/reload.
    pub(crate) fn plugin_registry_handle(
        &self,
    ) -> &kigi_agent::plugins::SharedPluginRegistryHandle {
        &self.plugin_registry_handle
    }
    /// Resolved cli-chat-proxy base for session features (via
    /// `proxy_url`). Not for the deployment-config fetch.
    pub(crate) fn coding_api_base_url(&self) -> String {
        self.cfg.borrow().endpoints.proxy_url()
    }
    pub(crate) fn alpha_test_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.alpha_test_key.clone()
    }
    /// Build the process-lifetime local `WorkspaceOps` on first use.
    ///
    /// Deferred past ACP wiring so `initialize` can respond before folder-trust
    /// scans and `WorkspaceHandle::new_minimal` run (same boot stall as plugin
    /// discovery on kigi-desktop Windows).
    fn ensure_local_workspace_ops(
        &self,
    ) -> Result<kigi_workspace::WorkspaceOps, acp::Error> {
        if let Some(ops) = self.workspace_ops.borrow().clone() {
            return Ok(ops);
        }
        let (cwd, project_lsp_trusted) = self.prime_launch_dir_trust();
        let ops = match kigi_workspace::handle::WorkspaceHandle::new_minimal(
            cwd.to_path_buf(),
            project_lsp_trusted,
        ) {
            Ok(handle) => kigi_workspace::WorkspaceOps::local(handle),
            Err(e) => {
                tracing::error!(error = % e, "failed to create local WorkspaceHandle");
                return Err(
                    acp::Error::internal_error().data("workspace not initialized"),
                );
            }
        };
        *self.workspace_ops.borrow_mut() = Some(ops.clone());
        Ok(ops)
    }
    /// Resolve the workspace ops, returning `Err` if not yet initialized.
    ///
    /// Only `None` before the first lazy local build via
    /// [`Self::ensure_local_workspace_ops`]. Called at the `ext_method`
    /// dispatch boundary and in session spawn; extensions receive the
    /// resolved `&WorkspaceOps` directly.
    pub(crate) fn resolve_workspace_ops(
        &self,
    ) -> Result<kigi_workspace::WorkspaceOps, acp::Error> {
        let ops = self.ensure_local_workspace_ops()?;
        if let Some(handle) = ops.workspace_handle() && !handle.has_client_ext_sink() {
            let gw = self.gateway.clone();
            handle
                .set_client_ext_sink(
                    std::sync::Arc::new(move |method: String, params: serde_json::Value| {
                        if let Ok(raw) = serde_json::value::to_raw_value(&params) {
                            gw.forward_fire_and_forget(
                                acp::ExtNotification::new(method, raw.into()),
                            );
                        }
                    }),
                );
        }
        Ok(ops)
    }
    /// Derive the current `AuthType` from auth method + auth manager state.
    ///
    /// Conceptually, `AuthType` describes *which authentication mechanism this
    /// session uses*, not *whether we currently have a live bearer*. Bearer
    /// liveness is tracked by the auth manager; the mechanism is fixed by
    /// `auth_method_id`.
    ///
    /// Returns `SessionToken` when EITHER:
    ///   - `auth_manager` currently has a live (non-expired) credential, OR
    ///   - the active auth method is session-based (`cached_token`,
    ///     `kimi-code`, `oidc`) -- even if the in-memory token is currently
    ///     expired or missing.
    ///
    /// Returns `ApiKey` only when the auth method is BYOK (`xai.api_key`) or
    ///   no auth method has been selected yet AND no live credential exists.
    ///
    /// The session-based clause is load-bearing: without it, chat_state can get
    /// locked into `auth_type = ApiKey` and skip token refresh on later prompts.
    pub(crate) fn auth_type(&self) -> kigi_chat_state::AuthType {
        if self.auth_manager.current().is_some() || self.is_session_based_auth() {
            kigi_chat_state::AuthType::SessionToken
        } else {
            kigi_chat_state::AuthType::ApiKey
        }
    }
    /// When `cached_token` cannot proceed, prefer non-interactive `xai.api_key`
    /// iff `should_advertise_xai_api_key`; otherwise the interactive device
    /// login.
    pub(super) fn cached_token_fallthrough_method_id(&self) -> acp::AuthMethodId {
        let id = auth_method::method_id_after_cached_token_unavailable(
            auth_method::should_advertise_xai_api_key(self.models_manager.models().values()),
        );
        acp::AuthMethodId::new(id)
    }
    /// Shared exit for missing/expired `cached_token`.
    pub(super) async fn authenticate_after_cached_token_unavailable(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        let method_id = self.cached_token_fallthrough_method_id();
        let meta = arguments.meta;
        tracing::info!(fallback = % method_id.0, "cached_token fallthrough");
        kigi_log::unified_log::warn(
            "auth cached_token fallthrough",
            None,
            Some(serde_json::json!({ "fallback" : method_id.0.as_ref() })),
        );
        acp::Agent::authenticate(
                self,
                acp::AuthenticateRequest::new(method_id).meta(meta),
            )
            .await
    }
    /// `authenticate(<api-key platform id>)`: interactive API-key login from
    /// the welcome picker for any non-OAuth registry platform.
    ///
    /// Reloads the platform keys from disk+env (the TUI persists the pasted
    /// key to auth.json under the platform-id scope immediately before this
    /// call), fails with an actionable error when none is configured,
    /// validates the key against `GET {platform_base}/models`, then marks the
    /// session authenticated exactly like an external API key: publish the
    /// method id (NOT session-based — no token refresh), swap the
    /// freshly-stamped config into the models manager, and trigger the model
    /// sync so the catalog gains the platform's entries. The key itself is
    /// never logged.
    pub(super) async fn authenticate_api_key_platform(
        &self,
        platform: kigi_models::PlatformId,
        method_id: acp::AuthMethodId,
    ) -> Result<AuthenticateResponse, acp::Error> {
        let keys =
            crate::agent::models::PlatformApiKeys::resolve_from_effective_config();
        auth_method::authenticate_platform_api_key(platform, keys.key_for(platform))
            .await
            .inspect_err(|_| {
                emit_login_span(
                    false,
                    method_id.0.as_ref(),
                    None,
                    Some("platform_key_invalid_or_missing"),
                );
            })?;
        // Rebuild the catalog from the on-disk config: the rebuild freshly
        // resolves platform keys (env > auth.json > config), so the key just
        // persisted to auth.json is stamped onto the platform's entries; a
        // parse failure keeps the last-known-good config (`on_auth_changed`
        // below still re-resolves keys from disk itself).
        match crate::config::load_effective_config()
            .map_err(|e| e.to_string())
            .and_then(|raw| crate::agent::config::Config::new_from_toml_cfg(&raw))
        {
            Ok(new_cfg) => self.models_manager.apply_config(new_cfg),
            Err(e) => {
                tracing::warn!(
                    error = % e,
                    "moonshot auth: config reload failed; keeping last-known-good"
                );
            }
        }
        self.set_auth_method(method_id.clone());
        self.models_manager.on_auth_changed().await;
        emit_login_span(true, method_id.0.as_ref(), None, None);
        // Report api-key auth mode so the pager's `apply_auth_meta` treats
        // the session like every other external-API-key login (badge shown,
        // `/usage` hidden).
        let auth_meta = crate::auth::AuthMeta {
            email: None,
            auth_mode: Some("api_key".to_string()),
            show_resolved_model: None,
        };
        let meta = serde_json::to_value(auth_meta)
            .ok()
            .and_then(|v| v.as_object().cloned());
        Ok(AuthenticateResponse::new().meta(meta))
    }
    /// `authenticate(<generic-oauth platform id>)`: interactive device-code
    /// login for a `uses_oauth` platform carrying an `OAuthConfig` (xai-grok).
    ///
    /// Uses a per-provider [`AuthManager`] scoped to the platform's `scope_key`
    /// (NOT the primary Kimi manager) so the minted session is persisted under
    /// its own `auth.json` scope, then triggers a catalog re-sync so the
    /// platform's models appear (their bearer is resolved per-provider at fetch
    /// / sampling time). The tokens are never logged.
    pub(super) async fn authenticate_oauth_platform(
        &self,
        platform: kigi_models::PlatformId,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        let method_id = arguments.method_id.clone();
        let oauth = platform
            .oauth()
            .expect("oauth_platform() guarantees a device-code OAuthConfig");
        let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
        tracing::info!(
            method = method_id.0.as_ref(),
            headless = auth_meta.headless,
            reauth = auth_meta.reauth,
            "auth: generic oauth device login",
        );
        let kigi_home = crate::util::kigi_home::kigi_home();
        let auth_manager =
            std::sync::Arc::new(crate::auth::AuthManager::new_oauth_provider(&kigi_home, oauth));
        auth_manager.configure_refresher();

        let flow_result = if !auth_meta.headless {
            let (url_tx, url_rx) = tokio::sync::oneshot::channel();
            let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
            *self.auth_code_tx.borrow_mut() = Some(code_tx);
            *self.auth_url_rx.borrow_mut() = Some(url_rx);
            let result = crate::auth::run_oauth_provider_flow(
                &auth_manager,
                oauth,
                auth_meta.reauth,
                Some(crate::auth::AuthChannels {
                    url_tx: Some(url_tx),
                    code_rx,
                }),
            )
            .await;
            *self.auth_code_tx.borrow_mut() = None;
            *self.auth_url_rx.borrow_mut() = None;
            result
        } else {
            crate::auth::run_oauth_provider_flow(&auth_manager, oauth, auth_meta.reauth, None).await
        };

        let (_auth, _did_auth) = flow_result.map_err(|e| {
            emit_login_span(false, method_id.0.as_ref(), None, Some("login_flow_failed"));
            let mut err = acp::Error::auth_required();
            err.message = e.to_string();
            err
        })?;

        // Do NOT stamp this token onto the shared sampling_config: it authorizes
        // ONLY this platform's models (api.x.ai/v1), not the primary session.
        // The catalog re-sync below resolves it per-provider.
        self.set_auth_method(method_id.clone());
        self.models_manager.on_auth_changed().await;
        emit_login_span(true, method_id.0.as_ref(), None, None);
        Ok(self.auth_response_with_meta())
    }

    pub(crate) fn deployment_key(&self) -> Option<String> {
        self.cfg.borrow().endpoints.deployment_key.clone()
    }
    /// Re-resolve eagerly-resolved config fields from the local config.
    ///
    /// Called on `/new` session creation so feature flags reflect the latest
    /// on-disk config without requiring a TUI restart. (Formerly this also
    /// re-fetched the xAI proxy's remote settings; that endpoint is gone.)
    ///
    /// In-flight sessions are unaffected — they snapshot config at creation.
    pub(super) async fn refresh_settings_and_reapply(&self) {
        let cwd = std::env::current_dir().ok();
        {
            let mut cfg = self.cfg.borrow_mut();
            crate::util::config::sync_campaign_fields(&mut cfg);
            let raw_config = crate::config::load_effective_config()
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        error = % e, "config reload failed during settings refresh"
                    );
                    toml::Value::Table(toml::map::Map::new())
                });
            cfg.re_resolve_runtime_fields(&raw_config, cwd.as_deref());
        }
        self.emit_settings_update_notification();
    }
    pub(super) async fn send_model_auto_switched(
        &self,
        session_id: &acp::SessionId,
        previous: &acp::ModelId,
        new: &acp::ModelId,
        reason: &str,
    ) {
        let notification = crate::extensions::notification::SessionNotification {
            session_id: session_id.clone(),
            update: crate::extensions::notification::SessionUpdate::ModelAutoSwitched {
                previous_model_id: previous.0.to_string(),
                new_model_id: new.0.to_string(),
                reason: reason.to_string(),
            },
            meta: None,
        };
        if let Ok(params) = serde_json::value::to_raw_value(&notification) {
            let _ = self
                .gateway
                .ext_notification(
                    acp::ExtNotification::new("kigi/session_notification", params.into()),
                )
                .await;
        }
    }
    /// Pure id → entry resolver (the `allowed_models` gate lives in `set_session_model`).
    pub(crate) fn resolve_model_id(
        &self,
        requested: &acp::ModelId,
    ) -> Result<ModelEntry, acp::Error> {
        let requested_str = requested.0.as_ref();
        let models = self.models_manager.models();
        let Some(catalog_key) = resolve_catalog_key(&models, requested) else {
            tracing::debug!(
                requested = % requested_str, model_count = models.len(),
                "resolve_model_id: unknown model id (not in models() by key or .model field)"
            );
            return Err(acp::Error::invalid_params().data("unknown model id"));
        };
        let entry = models
            .get(catalog_key.0.as_ref())
            .expect("resolve_catalog_key returns a key present in models");
        let match_kind = if catalog_key.0.as_ref() == requested_str {
            "map key"
        } else {
            "model field scan"
        };
        tracing::debug!(
            "resolve_model_id: matched by {}: requested={} model={}", match_kind,
            requested_str, entry.info.model
        );
        Ok(entry.clone())
    }
    /// Resolve the SESSION token for `model` by the model's OWN platform — the
    /// single guard against the api_key-channel token leak.
    ///
    /// An oauth-platform model (xai-grok) draws its session token from ITS OWN
    /// process-global pool manager (build-on-demand from the on-disk grok token,
    /// proactively refreshed), INDEPENDENT of the primary `auth_method`; when
    /// that provider has no stored session the token is `None` — NEVER the
    /// primary Kimi key. Every other model (first-party / Kimi) uses the primary
    /// session manager, and only under a session-based auth method —
    /// byte-identical to the pre-fix path. SECURITY: the resolved token is never
    /// logged.
    fn session_token_for_model(&self, model: &ModelEntry) -> Option<crate::auth::KimiAuth> {
        if let Some(oauth) = model
            .info()
            .id
            .as_deref()
            .and_then(kigi_models::parse_managed_model_key)
            .and_then(|(platform, _)| platform.oauth())
        {
            return crate::auth::oauth_registry::global_manager_for(
                &crate::util::kigi_home::kigi_home(),
                oauth,
            )
            .current_or_expired();
        }
        if self.is_session_based_auth() {
            self.auth_manager.current_or_expired()
        } else {
            None
        }
    }
    pub(crate) fn prepare_sampling_config_for_model(
        &self,
        model: &ModelEntry,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        // Resolve the session token by the MODEL's platform, not the primary
        // auth method: an oauth-platform model (xai-grok) uses its OWN
        // pool-backed token (`None` — never the Kimi key — when the user has not
        // logged into that provider), closing the api_key-channel leak where the
        // primary Kimi session token was stamped onto a grok request. A
        // first-party / Kimi model is unchanged. GUARANTEE: a grok model's
        // api_key is its own grok token or `None`, never the primary Kimi key.
        let session = self.session_token_for_model(model);
        let has_session_key = session.is_some();
        let mut credentials = resolve_credentials(
            model,
            session.as_ref().map(|a| a.key.as_str()),
        );
        if !has_session_key && credentials.auth_type == kigi_chat_state::AuthType::ApiKey
            && !model.has_own_credentials() && self.is_session_based_auth()
        {
            tracing::info!(
                model = model.info().model.as_str(),
                "auth: overriding auth_type to SessionToken (session-based auth method)",
            );
            kigi_log::unified_log::info(
                "auth auth_type override to SessionToken",
                None,
                Some(serde_json::json!({ "model" : model.info().model.as_str() })),
            );
            credentials.auth_type = kigi_chat_state::AuthType::SessionToken;
        }
        if !has_session_key && !model.has_own_credentials() {
            tracing::warn!(
                model = model.info().model.as_str(), is_expired = self.auth_manager
                .is_expired(), auth_type = ? credentials.auth_type,
                "auth: prepare_sampling_config has no session key",
            );
            kigi_log::unified_log::warn(
                "auth: prepare_sampling_config has no session key",
                None,
                Some(
                    serde_json::json!(
                        { "model" : model.info().model.as_str(), "is_expired" : self
                        .auth_manager.is_expired(), "auth_type" : format!("{:?}",
                        credentials.auth_type), }
                    ),
                ),
            );
        }
        let alpha_test_key = self.cfg.borrow().endpoints.alpha_test_key.clone();
        let mut config =
            crate::agent::config::sampling_config_for_model(model, credentials, alpha_test_key);
        config.origin_client = origin_client;
        config
    }
    /// Resolve sampling config for a model by ID, falling back to the global
    /// default on resolution failure. This ensures API-key auth routes to
    /// the public API (via resolve_credentials) instead of the global config's
    /// cli-chat-proxy base_url.
    pub(super) fn resolve_sampling_config_for_model(
        &self,
        model_id: &acp::ModelId,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> SamplingConfig {
        if let Ok(model) = self.resolve_model_id(model_id) {
            self.prepare_sampling_config_for_model(&model, origin_client.clone())
        } else {
            let mut c = self.sampling_config.borrow().clone();
            c.origin_client = origin_client;
            c
        }
    }
    /// Resolve `AgentDefinition.model` override for the parent session.
    /// Apply a profile's pinned-model override to the session's sampling config.
    ///
    /// `pinned_model` is resolved once by the caller (shared with harness
    /// inheritance). `None` — no override, or model not in catalog — keeps the
    /// session defaults.
    fn apply_agent_model_override(
        &self,
        pinned_model: Option<&(acp::ModelId, ModelEntry)>,
        default_model_id: acp::ModelId,
        default_sampling: SamplingConfig,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) -> (acp::ModelId, SamplingConfig) {
        let Some((id, model)) = pinned_model else {
            return (default_model_id, default_sampling);
        };
        let new_config = self.prepare_sampling_config_for_model(model, origin_client);
        tracing::info!(
            model = % id.0, "agent profile model override applied to parent session"
        );
        (id.clone(), new_config)
    }
    /// Build deploy-service config. The tool talks directly to the deployer service.
    pub(super) fn prepare_app_builder_deployer_config(
        &self,
    ) -> kigi_tools::implementations::kigi::deploy_app::AppBuilderDeployerConfig {
        use kigi_tools::implementations::kigi::deploy_app::AppBuilderDeployerConfig;
        AppBuilderDeployerConfig::Disabled
    }
    /// Web search config (PRD F5). The Kimi search service exists only on
    /// the Kimi Code subscription channel (`POST {coding_base}/search`,
    /// kimi-cli `auth/platforms.py`), so this is `Enabled` only for OAuth
    /// sessions — API-key-only sessions get `Disabled` and the tool is
    /// absent. The live token is refreshed per request via the session
    /// api-key provider; the config key is just the bootstrap value.
    pub(super) fn prepare_web_search_config(
        &self,
    ) -> kigi_tools::implementations::WebSearchConfig {
        use kigi_tools::implementations::WebSearchConfig;
        if self.cfg.borrow().disable_web_search {
            return WebSearchConfig::Disabled;
        }
        let Some(auth) = self.current_or_buffered_auth().filter(|a| a.is_session_auth()) else {
            tracing::info!("web_search disabled: no Kimi Code OAuth session");
            return WebSearchConfig::Disabled;
        };
        let base = self.cfg.borrow().endpoints.proxy_url();
        WebSearchConfig::Enabled {
            search_url: format!("{}/search", base.trim_end_matches('/')),
            api_key: auth.key,
            extra_headers: indexmap::IndexMap::new(),
        }
    }
    /// Returns `Err` with a user-facing message on invalid config; the caller at
    /// the process boundary prints it and exits.
    pub fn new(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
    ) -> Result<Self, String> {
        let (cfg, models_manager) = crate::agent::init::bootstrap(
            cfg,
            &auth_manager,
            prefetched_models,
        )?;
        Ok(Self::with_models(gateway, &cfg, auth_manager, models_manager))
    }
    /// Prepare the web fetch configuration based on feature flags.
    ///
    /// Enabled gate: `disable_web_search` kill-switch > `KIGI_WEB_FETCH` env >
    /// remote settings `web_fetch_enabled` > default ON (kimi-cli parity:
    /// `FetchURL` is always offered).
    ///
    /// Params resolution (TOML > env > remote settings > default):
    /// - `proxy_endpoint`: `[toolset.web_fetch] proxy_endpoint` > `KIGI_WEB_FETCH_PROXY` > remote settings > None
    /// - `allowed_domains`: `[toolset.web_fetch] allowed_domains` > remote settings > built-in defaults
    /// - `service_url`: TOML/env dev override > `{coding_base}/fetch` on
    ///   OAuth sessions (PRD F5) > None (local pipeline only)
    pub(super) fn prepare_web_fetch_config(
        &self,
    ) -> kigi_tools::implementations::kigi::web_fetch::WebFetchConfig {
        use kigi_tools::implementations::kigi::web_fetch::WebFetchConfig;
        let cfg = self.cfg.borrow();
        if cfg.disable_web_search {
            return WebFetchConfig::Disabled;
        }
        let remote = cfg.remote_settings.as_ref();
        let enabled = cfg.resolve_web_fetch();
        if !enabled.value {
            return WebFetchConfig::Disabled;
        }
        let context_window = Some(self.sampling_config.borrow().context_window);
        let mut params = cfg
            .toolset
            .web_fetch
            .resolve_params(
                remote.and_then(|s| s.web_fetch_proxy.as_deref()),
                remote.and_then(|s| s.web_fetch_allowed_domains.as_deref()),
                context_window,
            );
        if params.allowed_domains.as_ref().is_some_and(Vec::is_empty) {
            tracing::info!("web_fetch disabled: allowed_domains is explicitly empty");
            return WebFetchConfig::Disabled;
        }
        // PRD F5: the Kimi fetch service exists only on the OAuth channel.
        // TOML/env may pin their own service_url (dev override); otherwise
        // OAuth sessions get `{coding_base}/fetch` and API-key sessions
        // stay local-only.
        if params.service_url.is_none()
            && self
                .current_or_buffered_auth()
                .is_some_and(|a| a.is_session_auth())
        {
            params.service_url = Some(format!(
                "{}/fetch",
                cfg.endpoints.proxy_url().trim_end_matches('/')
            ));
        }
        WebFetchConfig::Enabled { params }
    }
    /// Construct from pre-built components. Use when the caller needs the
    /// `ModelsManager` handle externally (e.g. `run_leader` wires it to the
    /// config watcher). Otherwise prefer [`Self::new`].
    pub fn with_models(
        gateway: GatewaySender,
        cfg: &AgentConfig,
        auth_manager: Arc<AuthManager>,
        models_manager: crate::agent::models::ModelsManager,
    ) -> Self {
        models_manager.set_gateway(gateway.clone());
        let sampling_config = models_manager.sampling_config();
        let storage_mode = cfg.storage_mode;
        let default_yolo_mode = cfg.default_yolo_mode;
        let default_auto_mode = cfg.default_auto_mode;
        let config_root = crate::config::load_effective_config().ok();
        let empty_config = toml::Value::Table(toml::map::Map::new());
        let raw = config_root.as_ref().unwrap_or(&empty_config);
        let (worktree_type, wt_source) = crate::util::config::resolve_worktree_type(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let restore_code = crate::util::config::resolve_restore_code(
            raw,
            cfg.remote_settings.as_ref(),
        );
        let session_registry_local = config_root
            .as_ref()
            .and_then(crate::util::config::session_registry_from_toml_opt);
        tracing::info!(
            worktree_type = ? worktree_type, source = wt_source,
            "WORKTREE_CONFIG_SHELL: resolved worktree type at agent startup"
        );
        let (subagent_event_tx, subagent_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let activity = crate::agent::activity::AgentActivity::default();
        let mut subagent_coordinator = crate::agent::subagent::SubagentCoordinator::new();
        subagent_coordinator.set_running_gauge(activity.subagent_gauge());
        let instance = Self {
            sessions: RefCell::new(HashMap::new()),
            activity,
            loading_sessions: RefCell::new(HashMap::new()),
            prompt_intake_locks: RefCell::new(HashMap::new()),
            session_threads: RefCell::new(HashMap::new()),
            resident_roster_titles: RefCell::new(HashMap::new()),
            initialize_request: OnceLock::new(),
            gateway,
            subagent_model_overrides: cfg.subagent_model_overrides.clone(),
            subagent_toggle: cfg.subagent_toggle.clone(),
            subagent_roles: cfg.subagent_roles.clone(),
            subagent_personas: cfg.subagent_personas.clone(),
            launch_cwd: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            launch_dir_trust: std::cell::OnceCell::new(),
            plugin_registry_handle: kigi_agent::plugins::SharedPluginRegistryHandle::new(
                None,
                cfg.plugins.cli_plugin_dirs.clone(),
            ),
            plugin_registry_initialized: std::cell::Cell::new(false),
            persona_io_summaries: cfg
                .subagent_personas
                .iter()
                .map(|(name, p)| p.render_io_summary(name))
                .collect(),
            models_manager,
            cfg: RefCell::new(cfg.clone()),
            auth_method_id: crate::agent::auth_method::new_shared_auth_method_id(None),
            sampling_config: RefCell::new(sampling_config),
            auth_manager,
            auth_code_tx: RefCell::new(None),
            auth_url_rx: RefCell::new(None),
            client_type: RefCell::new(ClientType::default()),
            code_nav_enabled: std::cell::Cell::new(false),
            interactive_trust_client: std::cell::Cell::new(false),
            interactive_trust_prompted: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            storage_mode,
            default_yolo_mode,
            default_auto_mode,
            memory_config: None,
            config_watcher_path_tx: None,
            buffering_settings: RefCell::new(None),
            background_copy_context: BackgroundCopyContext::new(),
            session_turn_numbers: RefCell::new(HashMap::new()),
            codebase_indexes: Arc::new(
                parking_lot::Mutex::new(CodebaseIndexManager::new()),
            ),
            session_index_claims: RefCell::new(HashMap::new()),
            worktree_type,
            restore_code,
            session_registry_local,
            agent_mcp_state: std::sync::Arc::new(
                tokio::sync::Mutex::new(
                    crate::session::mcp_servers::McpState::new(vec![]),
                ),
            ),
            model_unavailable_sessions: RefCell::new(std::collections::HashMap::new()),
            subagent_event_tx,
            subagent_event_rx: RefCell::new(Some(subagent_event_rx)),
            subagent_coordinator: RefCell::new(subagent_coordinator),
            monitor_event_buffer: kigi_tools::implementations::kigi::task::types::MonitorEventBuffer::default(),
            workspace_ops: RefCell::new(None),
            require_gateway_sessions: Rc::new(
                RefCell::new(std::collections::HashSet::new()),
            ),
            session_live_state: RefCell::new(HashMap::new()),
            supervisor_started: std::cell::Cell::new(false),
            #[cfg(test)]
            finalize_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            roster_delta_spy: RefCell::new(Vec::new()),
            #[cfg(test)]
            supervisor_spawn_count: std::cell::Cell::new(0),
        };
        instance.auth_manager.configure_refresher();
        instance
    }
    /// Handle `kigi/internal/evict_sessions` — the leader server tells us a
    /// client disconnected and these sessions lost their IPC owner.
    ///
    /// **This is the no-evict keystone.** A disconnect must
    /// NOT destroy a session. The behavior is now *detach + keep-resident +
    /// idle-unload*:
    ///
    /// - **Sessions with live work stay resident.** We do NOT send `Shutdown`
    ///   and do NOT drop the `SessionHandle`, so the actor, its pending
    ///   permission oneshots, and its `KillOnDrop` tool subprocesses all
    ///   survive. The route/driver detach is groundwork for PR-3 (the
    ///   driver/subscriber maps don't exist yet), so for now we only mark the
    ///   live state.
    /// - **Fully idle sessions are unloaded to disk** to bound memory (the
    ///   `sessions`/`session_threads` maps are uncapped). This preserves the
    ///   legacy unload path — `Shutdown` the actor, drop the `SessionHandle`,
    ///   but KEEP the `SessionThread` so `drain_old_session_thread` can drain it
    ///   on reconnect — and crucially does **not** finalize the cloud replica
    ///   (the session remains resumable via `session/load`).
    ///
    /// The "live work" check is the coarse PR-2 stub (`session_has_live_work`);
    /// the full `SessionActivity` signal lands in PR-4.
    pub(super) async fn handle_evict_sessions(
        &self,
        params: &serde_json::value::RawValue,
    ) {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct EvictParams {
            session_ids: Vec<String>,
        }
        let Ok(p) = serde_json::from_str::<EvictParams>(params.get()) else {
            tracing::warn!("Failed to parse evict_sessions params");
            return;
        };
        if p.session_ids.is_empty() {
            return;
        }
        tracing::info!(
            count = p.session_ids.len(), sessions = ? p.session_ids,
            "Client disconnected; detaching sessions (no-evict keystone)"
        );
        let checks = p
            .session_ids
            .iter()
            .map(|sid| {
                let id = acp::SessionId::new(sid.clone());
                async move {
                    let busy = self.session_has_live_work(&id).await;
                    (id, busy)
                }
            });
        let resolved = futures::future::join_all(checks).await;
        let mut kept_resident: usize = 0;
        let mut unloaded: usize = 0;
        for (id, busy) in resolved {
            if busy {
                self.set_session_live_state(&id, SessionLiveState::Working);
                kept_resident += 1;
                tracing::info!(
                    session_id = % id.0,
                    "kept session resident across client disconnect (live work)"
                );
                continue;
            }
            self.request_session_shutdown(&id);
            if self.sessions.borrow_mut().remove(&id).is_some() {
                self.session_index_claims.borrow_mut().remove(&id);
                self.require_gateway_sessions.borrow_mut().remove(&id);
                self.set_session_live_state(&id, SessionLiveState::Dormant);
                unloaded += 1;
                tracing::debug!(
                    session_id = % id.0, "idle session unloaded to disk on disconnect"
                );
            }
        }
        tracing::info!(kept_resident, unloaded, "client-disconnect detach complete");
        self.sweep_dead_sessions();
    }
    /// Wait for an old session thread to finish before reloading the same session.
    ///
    /// When a client disconnects and a session is *idle*, `handle_evict_sessions`
    /// unloads it: sends `Shutdown`, drops the `SessionHandle`, and keeps the
    /// `SessionThread`. (Sessions with live work stay fully resident and skip
    /// this path.) If the client reconnects and loads the same session, we must
    /// wait for the old actor to finish flushing to disk before replaying
    /// `updates.jsonl`.
    ///
    /// Uses async polling (never blocks the `LocalSet` runtime) with a 5s deadline
    /// to handle slow shutdowns (e.g., embedding API timeouts).
    pub(super) async fn drain_old_session_thread(&self, session_id: &acp::SessionId) {
        let thread = self.session_threads.borrow_mut().remove(session_id);
        let Some(thread) = thread else { return };
        if thread.is_finished() {
            return;
        }
        tracing::info!(
            session_id = % session_id.0,
            "Waiting for old session thread to finish before reload"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if thread.is_finished() {
                tracing::debug!(
                    session_id = % session_id.0, "Old session thread finished cleanly"
                );
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = % session_id.0,
                    "Old session thread still running after 5s — proceeding with replay. \
                     Session data may be incomplete if the old actor is still writing."
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    /// Mark a `session/load` as in flight for `session_id`.
    ///
    /// Returns an RAII guard; while it is alive,
    /// [`Self::wait_for_in_flight_session_load`] blocks racing session-scoped
    /// requests for the same session. Dropping the guard (every exit path of
    /// `load_session`, success or error) removes the marker and wakes all
    /// waiters via watch-channel closure.
    pub(super) fn begin_session_load(
        &self,
        session_id: &acp::SessionId,
    ) -> SessionLoadGuard<'_> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.loading_sessions.borrow_mut().insert(session_id.clone(), rx.clone());
        SessionLoadGuard {
            agent: self,
            session_id: session_id.clone(),
            rx,
            _tx: tx,
        }
    }
    /// Session lookup that tolerates an in-flight `session/load`.
    ///
    /// THE chokepoint for the post-leader-crash error class: every
    /// user-facing session-scoped handler (`prompt`, `set_session_model`,
    /// `set_session_mode`, `interject`, ...) resolves its handle through
    /// this instead of a bare `sessions` lookup, so a request racing the
    /// reconnect-replayed `session/load` waits for the session to land
    /// rather than failing with "unknown session id" / "session not found".
    ///
    /// Returns `None` only when the session is genuinely absent — no load in
    /// flight (or the load failed / timed out), exactly the cases where the
    /// legacy error is correct.
    pub(crate) async fn session_handle_waiting_for_load(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        let existing = self.sessions.borrow().get(session_id).cloned();
        if existing.is_some() {
            return existing;
        }
        self.wait_for_in_flight_session_load(session_id).await;
        self.sessions.borrow().get(session_id).cloned()
    }
    /// If a `session/load` for `session_id` is in flight, wait (bounded) for
    /// it to finish. Returns immediately when no load is in flight.
    ///
    /// This closes the load-vs-request race after a leader restart: clients
    /// replay `session/load` on reconnect, and a `session/prompt` arriving
    /// right behind it must wait for the session to land in `self.sessions`
    /// instead of failing with "unknown session id". The wait wakes when the
    /// load's [`SessionLoadGuard`] drops (success or failure) and re-checks;
    /// a failed load still surfaces the original error to the caller.
    pub(crate) async fn wait_for_in_flight_session_load(
        &self,
        session_id: &acp::SessionId,
    ) {
        const LOAD_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
            60,
        );
        let deadline = tokio::time::Instant::now() + LOAD_WAIT_TIMEOUT;
        loop {
            if self.sessions.borrow().contains_key(session_id) {
                return;
            }
            let rx = self.loading_sessions.borrow().get(session_id).cloned();
            let Some(mut rx) = rx else { return };
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    session_id = % session_id.0,
                    "timed out waiting for in-flight session/load"
                );
                return;
            }
            let _ = tokio::time::timeout(deadline - now, rx.changed()).await;
        }
    }
    /// Returns the default YOLO mode setting for new sessions
    pub fn default_yolo_mode(&self) -> bool {
        self.default_yolo_mode
    }
    /// Returns the storage mode configured for this agent
    pub fn storage_mode(&self) -> StorageMode {
        self.storage_mode
    }
    /// Returns the background copy context for managing background file copy tasks.
    pub fn background_copy_context(&self) -> BackgroundCopyContext {
        self.background_copy_context.clone()
    }
    /// Move a foreground bash command to background.
    /// Routes through the session's tool bridge to unblock the agent loop.
    pub async fn background_foreground_command(
        &self,
        session_id: &str,
        tool_call_id: &str,
    ) -> bool {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.background_foreground_command(tool_call_id).await
        } else {
            false
        }
    }
    /// Kill a background task by task_id.
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub async fn kill_background_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<kigi_tools::types::KillOutcome, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.kill_background_task(task_id).await
        } else {
            Err("session not found".to_string())
        }
    }
    pub async fn delete_scheduled_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<bool, String> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.delete_scheduled_task(task_id).await
        } else {
            Err("session not found".to_string())
        }
    }
    /// Cancel a subagent by id, returning a typed outcome that backs the pager's
    /// `kigi/subagent/cancel`. Active/pending → cancelled (a finish follows);
    /// already-finished → its terminal status; unknown id → `NotFound`.
    pub fn cancel_subagent(
        &self,
        subagent_id: &str,
    ) -> kigi_tools::implementations::kigi::task::types::SubagentCancelOutcome {
        self.subagent_coordinator.borrow_mut().cancel_with_outcome(subagent_id)
    }
    /// List running subagent seeds for a given parent session.
    ///
    /// Synchronously collects seeds from the coordinator, suitable for
    /// async resolution via `resolve_running_list()` after the borrow is
    /// dropped.
    pub(crate) fn list_running_subagents(
        &self,
        parent_session_id: &str,
    ) -> Vec<crate::agent::subagent::RunningSubagentListSeed> {
        self.subagent_coordinator.borrow().list_running_for_parent(parent_session_id)
    }
    /// Return fork provenance metadata for a subagent.
    pub(crate) fn provenance_for_subagent(
        &self,
        subagent_id: &str,
    ) -> crate::agent::subagent::SubagentProvenance {
        self.subagent_coordinator.borrow().provenance_for(subagent_id)
    }
    /// Return `(parent_session_id, child_session_id)` for a subagent.
    pub(crate) fn session_ids_for_subagent(
        &self,
        subagent_id: &str,
    ) -> Option<(String, String)> {
        self.subagent_coordinator.borrow().session_ids_for(subagent_id)
    }
    /// Synchronous lookup of a single subagent by ID.
    ///
    /// Returns `Option<SnapshotLookup>` which must be resolved
    /// asynchronously via `resolve_snapshot()` after the borrow is dropped.
    pub(crate) fn lookup_subagent(
        &self,
        subagent_id: &str,
    ) -> Option<crate::agent::subagent::SnapshotLookup> {
        self.subagent_coordinator.borrow().lookup(subagent_id)
    }
    /// List all background tasks for a session.
    /// Routes through the session's tool bridge to the TerminalBackend.
    pub async fn list_tasks(
        &self,
        session_id: &str,
    ) -> Option<Vec<kigi_tools::types::TaskSnapshot>> {
        let sid = acp::SessionId::new(session_id);
        if let Some(handle) = self.get_session_handle(&sid) {
            handle.list_tasks().await
        } else {
            None
        }
    }
    /// Flush a session's persistence buffer with a 5-second timeout.
    ///
    /// Sends `FlushComplete` to the session actor, which chains through to
    /// `FlushAndAck` on the persistence actor — a true sync barrier that only
    /// resolves after all queued writes (chat messages, updates) hit disk.
    ///
    /// Returns `Ok(())` on success, `Err(reason)` on timeout or channel failure.
    pub(crate) async fn flush_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), &'static str> {
        let cmd_tx = self.sessions.borrow().get(session_id).map(|h| h.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else {
            return Err("session not found");
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if cmd_tx
            .send(SessionCommand::FlushComplete {
                respond_to: tx,
            })
            .is_err()
        {
            return Err("send failed");
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err("channel closed"),
            Err(_) => Err("timeout"),
        }
    }
    /// Get a session's cwd by session_id.
    /// Returns None if the session is not found.
    pub fn get_session_cwd(&self, session_id: &acp::SessionId) -> Option<PathBuf> {
        let sessions = self.sessions.borrow();
        sessions.get(session_id).map(|handle| PathBuf::from(&handle.info.cwd))
    }
    /// Get a session handle by session_id.
    /// Returns None if the session is not found.
    pub fn get_session_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::session::SessionHandle> {
        let sessions = self.sessions.borrow();
        sessions.get(session_id).cloned()
    }
    /// Get hooks list for a session (for `kigi/hooks/list` extension).
    pub async fn list_hooks(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<kigi_hooks_plugins_types::HooksListResponse> {
        let handle = self.get_session_handle(session_id)?;
        handle.get_hooks_list().await
    }
    /// Execute a hooks management action (for `kigi/hooks/action`).
    pub async fn execute_hooks_action(
        &self,
        session_id: &acp::SessionId,
        action: kigi_hooks_plugins_types::HooksAction,
    ) -> Option<kigi_hooks_plugins_types::ActionOutcome> {
        if matches!(action, kigi_hooks_plugins_types::HooksAction::Untrust)
            && let Some(cwd) = self.get_session_cwd(session_id)
        {
            self.interactive_trust_prompted
                .borrow_mut()
                .remove(&kigi_workspace::trust::workspace_key(&cwd));
        }
        let handle = self.get_session_handle(session_id)?;
        handle.execute_hooks_action(action).await
    }
    /// Execute a plugins management action (for `kigi/plugins/action`).
    pub async fn execute_plugins_action(
        &self,
        session_id: &acp::SessionId,
        action: kigi_hooks_plugins_types::PluginsAction,
    ) -> Option<kigi_hooks_plugins_types::ActionOutcome> {
        let is_reload = matches!(action, kigi_hooks_plugins_types::PluginsAction::Reload);
        let handle = self.get_session_handle(session_id)?;
        let outcome = handle.execute_plugins_action(action).await;
        let succeeded = matches!(
            outcome.as_ref().map(| o | & o.status),
            Some(kigi_hooks_plugins_types::OutcomeStatus::Success)
        );
        if is_reload && succeeded {
            self.broadcast_plugin_registry_to_sessions(Some(session_id));
        }
        outcome
    }
    /// Get a snapshot of the shared plugin registry (for `kigi/plugins/list`).
    pub fn plugin_registry_snapshot(
        &self,
    ) -> Option<std::sync::Arc<kigi_agent::plugins::PluginRegistry>> {
        self.plugin_registry_handle.snapshot()
    }
    /// Resolve client version: prefer the value from the initialize request _meta,
    /// fall back to the agent's own version (VERSION_WITH_COMMIT set by the TUI launcher).
    pub(super) fn client_version(&self) -> Option<String> {
        self.initialize_request
            .get()
            .and_then(|req| req.meta.as_ref())
            .and_then(|m| m.get("clientVersion"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.cfg.borrow().client_version.clone())
    }
    pub(super) fn origin_client_info_from_meta(
        &self,
        meta: Option<&acp::Meta>,
    ) -> Option<crate::http::OriginClientInfo> {
        crate::http::merge_origin_client_info(
                crate::http::origin_client_info_from_meta(meta),
                crate::http::origin_client_info_from_meta(
                        self.initialize_request.get().and_then(|req| req.meta.as_ref()),
                    )
                    .map(|mut origin| {
                        if origin.version.is_none() {
                            origin.version = self.client_version();
                        }
                        origin
                    }),
            )
            .map(|mut origin| {
                if origin.version.is_none() {
                    origin.version = self.client_version();
                }
                origin
            })
    }
    /// Returns the model state for a given session (or the agent default).
    ///
    /// When `session_id` is `Some`, looks up the session's per-session model.
    /// Falls back to `current_model_id` (startup default) when no session is
    /// found or `session_id` is `None` (e.g., during `initialize` before any
    /// session exists).
    pub fn model_state(
        &self,
        session_id: Option<&acp::SessionId>,
    ) -> acp::SessionModelState {
        let model_id = lookup_session_model(
            &self.sessions.borrow(),
            session_id,
            &self.models_manager.current_model_id(),
        );
        let mut available_models: Vec<acp::ModelInfo> = self
            .models_manager
            .available()
            .values()
            .cloned()
            .collect();
        let override_effort = session_id
            .and_then(|sid| self.sessions.borrow().get(sid).map(|h| h.reasoning_effort))
            .flatten()
            .or_else(|| self.models_manager.current_reasoning_effort());
        if let Some(override_effort) = override_effort
            && let Some(info) = available_models
                .iter_mut()
                .find(|info| info.model_id == model_id)
            && supports_reasoning_effort_meta(info.meta.as_ref())
        {
            let mut map = info.meta.clone().unwrap_or_default();
            map.insert(
                REASONING_EFFORT_META_KEY.to_string(),
                reasoning_effort_meta_value(override_effort),
            );
            info.meta = Some(map);
        }
        acp::SessionModelState::new(model_id, available_models)
    }
    pub(super) fn session_config_options(
        &self,
        session_id: Option<&acp::SessionId>,
        state: &acp::SessionModelState,
    ) -> Vec<session_config::SessionConfigOption> {
        let model_id = resolve_catalog_key(
                &self.models_manager.models(),
                &state.current_model_id,
            )
            .unwrap_or_else(|| state.current_model_id.clone());
        let supports_effort = self
            .models_manager
            .model_supports_reasoning_effort(model_id.0.as_ref());
        let effort_options: Vec<ReasoningEffortOption> = if supports_effort {
            let options = self
                .models_manager
                .model_reasoning_efforts(model_id.0.as_ref());
            if options.is_empty() {
                session_config::legacy_session_effort_options()
            } else {
                options
            }
        } else {
            Vec::new()
        };
        let current_effort = if supports_effort {
            session_id
                .and_then(|sid| {
                    self.sessions.borrow().get(sid).map(|h| h.reasoning_effort)
                })
                .flatten()
                .or_else(|| self.models_manager.current_reasoning_effort())
                .or_else(|| {
                    self
                        .models_manager
                        .model_default_reasoning_effort(model_id.0.as_ref())
                })
        } else {
            None
        };
        session_config::build_session_config_options(
            &state.available_models,
            &model_id,
            &effort_options,
            current_effort,
        )
    }
    /// Build the `kigi/sessionConfig` and `kigi/sessionDetail` `_meta` values
    /// shared by `new_session` and `load_session`, returned as
    /// `(sessionConfig, sessionDetail)`. Keeping both response paths on this one
    /// builder stops them drifting.
    pub(super) fn session_config_meta(
        &self,
        session_id: &acp::SessionId,
        cwd: String,
        title: Option<String>,
        model_state: &acp::SessionModelState,
    ) -> (serde_json::Value, serde_json::Value) {
        let config_options = self.session_config_options(Some(session_id), model_state);
        let detail = session_config::KigiSessionDetail::build(
            session_id.0.to_string(),
            cwd,
            model_state.current_model_id.0.to_string(),
            title,
        );
        (serde_json::json!({ "options" : config_options }), serde_json::json!(detail))
    }
    /// Seed the global sampling config with login auth when available.
    ///
    /// Only sets the `api_key` if missing. Does NOT resolve `base_url` from
    /// `current_model_id` — that's deferred to session creation time to avoid
    /// cross-client contamination in leader mode (where `current_model_id` is
    /// shared mutable state).
    pub(super) fn seed_client_config_auth_if_available(&self) {
        let mut sampling_config = self.sampling_config.borrow_mut();
        if sampling_config.api_key.is_none() {
            if let Some(auth) = self.auth_manager.current_or_expired() {
                sampling_config.api_key = Some(auth.key);
                tracing::debug!("auth: seed_client_config set auth (SessionToken)");
                kigi_log::unified_log::debug(
                    "auth: seed_client_config set auth (SessionToken)",
                    None,
                    None,
                );
            } else if !self
                .models_manager
                .models()
                .values()
                .any(|m| m.has_own_credentials())
            {
                tracing::warn!(
                    "No credentials found: no login token and no model api_key/env_key"
                );
                kigi_log::unified_log::warn(
                    "No credentials found: no login token and no model api_key/env_key",
                    None,
                    None,
                );
            }
        }
    }
    /// Allocate the next monotonic telemetry turn number for a session.
    ///
    /// Returns the current turn number and advances the counter. The counter is
    /// intentionally monotonic even across rewinds to avoid overwriting older
    /// telemetry docs in cloud storage.
    ///
    /// For sessions sharing a parent's trace counter, call this once with the
    /// **root session ID** and reuse the result so the root's counter does not
    /// advance more than once per logical turn. The cloud storage layout writes to
    /// `{session_id}/turn_{N}/`.
    pub(crate) fn allocate_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        let turn = self.peek_turn_number(session_id);
        self.set_turn_number(session_id, turn.saturating_add(1));
        turn
    }
    /// Read a session's next trace turn number without advancing the counter.
    fn peek_turn_number(&self, session_id: &acp::SessionId) -> u64 {
        self.session_turn_numbers.borrow().get(session_id).copied().unwrap_or(0u64)
    }
    /// Set a session's next trace turn number. The sole writer of the
    /// `session_turn_numbers` counter, shared by `allocate_turn_number` and the
    /// batched harness-sibling allocation so both honor the same storage.
    fn set_turn_number(&self, session_id: &acp::SessionId, next: u64) {
        self.session_turn_numbers.borrow_mut().insert(session_id.clone(), next);
    }
    /// Resolve the agent definition for a session.
    ///
    /// Priority (highest to lowest):
    /// 1. Model `agent_type` if it names a strict harness (codex, …).
    /// 2. `acp_agent_profile` from ACP `_meta.agentProfile` (remote clients).
    /// 3. `agent_profile_path` from CLI `--agent-profile`.
    /// 4. `agent_config` from config.toml `[agent]`.
    /// 5. `KIGI_AGENT` env var.
    /// 6. Built-in default agent.
    ///
    /// `KIGI_AGENT` and an explicit `[agent] name` bypass step 1.
    /// Strict-harness classification is structural — see
    /// [`kigi_agent::config::is_strict_harness_agent_type`].
    ///
    /// Harness inheritance for a profile that pins its own model is applied by
    /// the caller via [`inherited_harness_template`], not here.
    pub fn resolve_agent_definition(
        cwd: &std::path::Path,
        agent_profile_path: Option<&std::path::Path>,
        agent_config: &config::AgentSelectionConfig,
        acp_agent_profile: Option<kigi_agent::AgentDefinition>,
        model_agent_type: Option<&str>,
    ) -> kigi_agent::AgentDefinition {
        use kigi_agent::AgentDefinition;
        let kigi_agent_env_set = std::env::var("KIGI_AGENT")
            .ok()
            .is_some_and(|s| !s.trim().is_empty());
        let config_agent_explicitly_set = agent_config.name.is_some();
        let model_requires_strict_harness = model_agent_type
            .is_some_and(kigi_agent::config::is_strict_harness_agent_type);
        if !kigi_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && let Some(def) = kigi_agent::discovery::by_name_in_cwd(required, cwd)
        {
            tracing::info!(
                agent_name = % def.name, "Using agent definition from model agent_type"
            );
            return def;
        }
        if let Some(def) = acp_agent_profile {
            tracing::info!(
                agent_name = % def.name,
                "Using agent profile from ACP _meta.agentProfile"
            );
            return def;
        }
        if let Some(path) = agent_profile_path {
            match AgentDefinition::from_file(path) {
                Ok(def) => return def,
                Err(e) => {
                    tracing::error!(
                        path = % path.display(), error = % e,
                        "Failed to load agent profile from --agent-profile path"
                    );
                    eprintln!(
                        "error: failed to load agent profile '{}': {}", path.display(), e
                    );
                    crate::instrumentation::finalize_and_exit(1);
                }
            }
        }
        if let Some(ref path) = agent_config.definition {
            match AgentDefinition::from_file(path) {
                Ok(def) => {
                    tracing::info!(
                        agent_name = % def.name, path = % path.display(),
                        "Using agent definition from config.toml [agent] definition"
                    );
                    return def;
                }
                Err(e) => {
                    tracing::warn!(
                        path = % path.display(), error = % e,
                        "Failed to load agent definition from config.toml [agent] definition, \
                         falling through to next source"
                    );
                }
            }
        }
        if let Some(ref name) = agent_config.name {
            tracing::info!(
                agent_name = % name,
                "Resolving agent definition from config.toml [agent] name"
            );
            if let Some(def) = kigi_agent::discovery::by_name_in_cwd(name, cwd) {
                return def;
            }
            tracing::warn!(
                agent_name = % name,
                "Agent '{}' not found via discovery, falling through to next source",
                name
            );
        }
        let agent_name = std::env::var("KIGI_AGENT").ok();
        let resolved = match agent_name.as_deref() {
            Some("browser-use") | Some("browser_use") => AgentDefinition::browser_use(),
            Some("kigi-concise") | Some("kigi_concise") => {
                AgentDefinition::kigi_concise()
            }
            Some(path) if std::path::Path::new(path).is_absolute() => {
                match AgentDefinition::from_file(path) {
                    Ok(def) => def,
                    Err(e) => {
                        tracing::warn!(
                            path = path, error = % e,
                            "Failed to load agent definition from file, falling back to default"
                        );
                        AgentDefinition::kigi_plan()
                    }
                }
            }
            Some(name) => {
                kigi_agent::discovery::by_name_in_cwd(name, cwd)
                    .unwrap_or_else(AgentDefinition::kigi_plan)
            }
            None => AgentDefinition::kigi_plan(),
        };
        if !kigi_agent_env_set && !config_agent_explicitly_set
            && model_requires_strict_harness && let Some(required) = model_agent_type
            && resolved.name != required
        {
            tracing::info!(
                resolved_agent = % resolved.name, model_agent_type = % required,
                "resolve_agent_definition: model requires different agent, re-resolving"
            );
            if let Some(def) = kigi_agent::discovery::by_name_in_cwd(required, cwd) {
                return def;
            }
            tracing::warn!(
                model_agent_type = % required, fallback_agent = % resolved.name,
                "resolve_agent_definition: model agent_type '{}' not found via discovery, \
                 keeping chain-resolved agent",
                required,
            );
        }
        resolved
    }
    /// Extract per-client terminal/fs capabilities from request `_meta`
    /// (injected by the leader). Falls back to the shared `init` OnceCell.
    pub(super) fn resolve_client_io_caps(
        meta: Option<&acp::Meta>,
        init: &acp::InitializeRequest,
    ) -> (bool, bool, bool) {
        let terminal = meta
            .and_then(|m| m.get("clientTerminal"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.terminal);
        let fs_read = meta
            .and_then(|m| m.get("clientFsRead"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.read_text_file);
        let fs_write = meta
            .and_then(|m| m.get("clientFsWrite"))
            .and_then(|v| v.as_bool())
            .unwrap_or(init.client_capabilities.fs.write_text_file);
        (terminal, fs_read, fs_write)
    }
    /// Spawn and register a session actor given a session id and session parameters.
    ///
    /// Parameters are bundled in [`SessionSpawnOptions`] (named fields) rather than
    /// passed positionally: there are too many same-typed args (`bool`s,
    /// `Option<…>`s) for positional calls to be transposition-safe.
    pub(super) async fn spawn_and_register_session(
        &self,
        init: &acp::InitializeRequest,
        spec: SessionSpawnOptions<'_>,
    ) -> Result<(), acp::Error> {
        let SessionSpawnOptions {
            session_info,
            cwd,
            mcp_servers,
            initial_client_mcp_servers,
            mcp_meta_config_map,
            persistence,
            mut chat_history,
            rewind_points_file_path,
            initial_total_tokens,
            origin_client: _origin_client,
            client_code_nav_enabled,
            client_terminal,
            client_fs_read,
            client_fs_write,
            preloaded_envrc,
            persisted_signals,
            persisted_plan_mode,
            persisted_goal_mode,
            persisted_graph_mode,
            persisted_announcement_state,
            session_meta,
            model_agent_type,
            session_model_id,
            session_yolo_mode,
            session_auto_mode,
            prompt_display_cwd,
        } = spec;
        let _timer = crate::instrumentation_timer!("session.spawn_and_register");
        reject_direct_hub_cloud_meta(session_meta)?;
        let spawn_remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(
            cwd.as_path(),
            spawn_remote_settings.as_ref(),
            false,
        );
        let use_acp_fs = client_fs_read && client_fs_write;
        let fs_notify_config = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("kigi/fs_notify"))
            .and_then(|v| {
                use crate::session::{ClientFsConfig, ClientFsMode};
                use kigi_fsnotify::FsConfig;
                if v.as_bool() == Some(true) {
                    return Some(ClientFsConfig::default());
                }
                let obj = v.as_object()?;
                if obj.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
                    return None;
                }
                let mode = if obj.get("index").and_then(|i| i.as_bool()) == Some(true) {
                    ClientFsMode::Index
                } else {
                    ClientFsMode::Events
                };
                let mut fs = FsConfig::default();
                if let Some(ms) = obj.get("debounce_ms").and_then(|v| v.as_u64()) {
                    fs.debounce_ms = ms;
                }
                if let Some(patterns) = obj.get("ignore").and_then(|v| v.as_array()) {
                    fs.ignore_patterns = patterns
                        .iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect();
                }
                Some(ClientFsConfig { fs, mode })
            });
        let fs: Arc<dyn kigi_workspace::file_system::AsyncFileSystem> = if use_acp_fs {
            let mut acp_fs = AcpSessionFs::new(
                cwd.to_path_buf(),
                session_info.id.clone(),
                self.gateway.clone(),
            );
            if let Some(ref display) = prompt_display_cwd {
                acp_fs = acp_fs.with_display_cwd(std::path::PathBuf::from(display));
            }
            Arc::new(acp_fs)
        } else {
            Arc::new(LocalFs::new(cwd.to_path_buf()))
        };
        let gateway_enabled = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        );
        let terminal: std::sync::Arc<dyn crate::terminal::AsyncTerminalRunner> = if client_terminal {
            std::sync::Arc::new(AcpTerminalRunner {
                gateway: self.gateway.clone(),
                session_id: session_info.id.clone(),
            })
        } else {
            let notifier: std::sync::Arc<
                dyn crate::terminal::SessionNotificationSender,
            > = std::sync::Arc::new(
                crate::terminal::GatedNotifier::new(
                    std::sync::Arc::new(self.gateway.clone()),
                    gateway_enabled.clone(),
                ),
            );
            std::sync::Arc::new(TerminalRunner::new(notifier, session_info.id.clone()))
        };
        let load_envrc = self.cfg.borrow().session.load_envrc.unwrap_or(true);
        let startup_hints = init
            .meta
            .as_ref()
            .and_then(|m| m.get("startupHints"))
            .and_then(|v| {
                serde_json::from_value::<crate::session::StartupHints>(v.clone()).ok()
            })
            .unwrap_or_default();
        let hunk_plan = plan_hunk_tracking(
            init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("kigi/hunkTracker"))
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
        );
        let incremental_bash_output = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("kigi/incrementalBashOutput"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let no_color = init
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|m| m.get("kigi/bashOutputNoColor"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hunk_tracking_enabled = hunk_plan.enabled();
        let (hunk_tracker_handle, hunk_event_rx) = match hunk_plan.actor_mode {
            Some(mode) => {
                let cancel = CancellationToken::new();
                let (hunk_event_tx, hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
                let handle = HunkTrackerActor::spawn(
                    session_info.id.0.to_string(),
                    cwd.as_path().to_path_buf(),
                    hunk_event_tx,
                    mode,
                    cancel.clone(),
                );
                (handle, Some((hunk_event_rx, cancel)))
            }
            None => (kigi_hunk_tracker::HunkTrackerHandle::noop(), None),
        };
        let has_xai_auth = self.auth_manager.current().is_some_and(|a| a.is_session_auth());
        let loc_tracking_enabled = hunk_tracking_enabled && has_xai_auth
            && (self
                .cfg
                .borrow()
                .remote_settings
                .as_ref()
                .and_then(|s| s.loc_tracking)
                .unwrap_or(false)
                || std::env::var("KIGI_LOC_TRACKING")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false));
        let (feedback_resolved, feedback_flags) = {
            let cfg = self.cfg.borrow();
            let resolved = cfg.resolve_feedback();
            let flags = crate::session::feedback_manager::FeedbackFlags {
                enabled: resolved.value,
            };
            (resolved, flags)
        };
        tracing::info!(feedback = % feedback_resolved, "resolved feedback feature flag");
        let loc_aggregate_rx = match hunk_event_rx {
            Some((hunk_event_rx, loc_cancel)) if loc_tracking_enabled => {
                let (loc_agg_tx, loc_agg_rx) = tokio::sync::mpsc::unbounded_channel();
                let loc_path = crate::session::persistence::session_dir(&session_info)
                    .join("hunk_records.jsonl");
                let loc_writer = kigi_hunk_tracker::JsonlHunkRecordWriter::new(loc_path);
                let loc_ctx = kigi_hunk_tracker::LocSinkContext {
                    session_id: session_info.id.0.to_string(),
                    agent_id: crate::util::agent_id::agent_id(),
                    user_id: self.auth_manager.current().map(|a| a.user_id.clone()),
                    aggregate_tx: Some(loc_agg_tx),
                };
                tokio::spawn(
                    kigi_hunk_tracker::run_loc_sink(
                        hunk_event_rx,
                        loc_writer,
                        loc_ctx,
                        loc_cancel,
                    ),
                );
                Some(loc_agg_rx)
            }
            _ => None,
        };
        let project_env_trusted = folder_trust::project_scope_allowed(cwd.as_path());
        let mut session_env = kigi_workspace::permission::claude_settings::load_claude_env_with_project(
            cwd.as_path(),
            project_env_trusted,
        );
        let envrc = match preloaded_envrc {
            Some(env) => env,
            None => {
                kigi_workspace::envrc::load_envrc_or_empty_when_trusted(
                    cwd.as_path(),
                    load_envrc && project_env_trusted,
                )
            }
        };
        session_env.extend(envrc);
        if no_color {
            session_env.extend(crate::terminal::no_color_env());
        } else {
            session_env.extend(crate::terminal::color_env());
        }
        let mut tool_ctx = ToolContext::with_preloaded_env(
                cwd.clone(),
                Some(self.gateway.clone()),
                Some(session_info.id.clone()),
                fs,
                terminal,
                hunk_tracker_handle,
                session_env,
            )
            .with_hunk_tracking_enabled(hunk_tracking_enabled);
        let workspace_ops = self
            .resolve_workspace_ops()
            .map_err(|_| {
                acp::Error::internal_error()
                    .data(
                        "Local workspace initialization failed; cannot create session. \
                 Check that a Tokio runtime is available.",
                    )
            })?;
        tool_ctx.subagent_event_tx = Some(self.subagent_event_tx.clone());
        tool_ctx.is_turn_active = Some(
            self.subagent_coordinator.borrow().turn_active_flag(),
        );
        tool_ctx.monitor_event_buffer = Some(self.monitor_event_buffer.clone());
        tool_ctx.subagent_depth = 0;
        tool_ctx.auto_wake_enabled = self.cfg.borrow().auto_wake_enabled;
        let support_permission = self.cfg.borrow().features.support_permission;
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let sampling_config = self
            .resolve_sampling_config_for_model(&session_model_id, origin_client.clone());
        if self.auth_method_id.load().is_none() {
            return Err(acp::Error::auth_required().data("no auth method id provided"));
        }
        let auth_method_id = std::sync::Arc::clone(&self.auth_method_id);
        tracing::info!(
            session_id = % session_info.id.0, ? startup_hints, "startup hints"
        );
        let auto_compact_threshold_percent = {
            let cfg = self.cfg.borrow();
            let models = self.models_manager.models();
            let model = config::find_model_by_id(&models, &session_model_id.0);
            crate::util::config::resolve_auto_compact_threshold_percent(
                &cfg,
                &session_model_id.0,
                model.map(|e| &e.info),
            )
        };
        let system_prompt_label = {
            let cfg = self.cfg.borrow();
            let models = self.models_manager.models();
            let model = config::find_model_by_id(&models, &session_model_id.0);
            crate::util::config::resolve_system_prompt_label(
                &cfg,
                &session_model_id.0,
                model.map(|e| &e.info),
            )
        };
        let compaction_mode = self.cfg.borrow().resolve_compaction_mode();
        let compaction_verbatim_input = self
            .cfg
            .borrow()
            .resolve_compaction_verbatim_input();
        let two_pass_enabled = self.cfg.borrow().is_two_pass_compaction_enabled();
        let auto_update = self.cfg.borrow().cli.auto_update;
        let client_type = *self.client_type.borrow();
        let buffering_settings = self.buffering_settings.borrow().clone();
        let feedback_base_url = self.feedback_base_url();
        tracing::info!(
            session_id = % session_info.id.0, feedback_url = ? feedback_base_url,
            "Initializing feedback manager for session"
        );
        let skills = self.cfg.borrow().skills.clone();
        let compat = self.cfg.borrow().compat_resolved;
        let acp_agent_profile = parse_agent_profile_from_meta(session_meta);
        let session_default_agent_profile = acp_agent_profile
            .as_ref()
            .map(|d| d.name.clone());
        let mut agent_definition = {
            let cfg = self.cfg.borrow();
            Self::resolve_agent_definition(
                cwd.as_path(),
                cfg.agent_profile_path.as_deref(),
                &cfg.agent,
                acp_agent_profile,
                model_agent_type,
            )
        };
        {
            let cfg = self.cfg.borrow();
            let overrides = &cfg.cli_agent_overrides;
            overrides.apply_to_definition(&mut agent_definition);
            if overrides.has_definition_overrides() {
                tracing::debug!(
                    agent = % agent_definition.name, tools = ? overrides.tools,
                    disallowed = ? overrides.disallowed_tools, permission_mode = ?
                    overrides.permission_mode, "cli agent overrides applied"
                );
            }
        }
        let pinned_model: Option<(acp::ModelId, ModelEntry)> = match &agent_definition
            .model
        {
            kigi_agent::config::ModelOverride::Override(id) => {
                let mid = acp::ModelId::new(Arc::from(id.as_str()));
                match self.resolve_model_id(&mid) {
                    Ok(entry) => Some((mid, entry)),
                    Err(_) => {
                        tracing::warn!(
                            agent = % agent_definition.name, model = % id,
                            "agent profile model not in catalog, keeping session default"
                        );
                        None
                    }
                }
            }
            kigi_agent::config::ModelOverride::Inherit => None,
        };
        if let Some(template) = inherited_harness_template(
            &agent_definition.user_message_template,
            pinned_model.as_ref().map(|(_, e)| e.info().agent_type.as_str()),
            cwd.as_path(),
        ) {
            tracing::info!(
                agent = % agent_definition.name,
                "Inheriting harness wire-format from the profile model's agent_type"
            );
            agent_definition.user_message_template = template;
        }
        let (session_model_id, sampling_config) = self
            .apply_agent_model_override(
                pinned_model.as_ref(),
                session_model_id,
                sampling_config,
                origin_client.clone(),
            );
        let max_turns = {
            let cfg = self.cfg.borrow();
            cfg.cli_agent_overrides
                .max_turns
                .or(agent_definition.max_turns)
                .map(|v| v as usize)
        };
        {
            let cfg = self.cfg.borrow();
            let effective = cfg
                .toolset
                .resolve_file_toolset(cfg.remote_settings.as_ref());
            if effective != crate::tools::FileToolset::Standard {
                let file_tools = effective
                    .tool_configs(&cfg.toolset.hashline)
                    .map_err(|e| {
                        acp::Error::invalid_params()
                            .data(format!("invalid [toolset.hashline] config: {e}"))
                    })?;
                agent_definition.override_file_tools(file_tools);
            }
        }
        let lsp_tools_enabled = self.cfg.borrow().resolve_lsp_tools().value;
        if lsp_tools_enabled && tool_ctx.lsp.is_none() {
            let snapshot = self.plugin_registry_handle.snapshot();
            let active: Vec<_> = snapshot
                .iter()
                .flat_map(|reg| reg.active_plugins())
                .collect();
            let (plugin_lsp_paths, plugin_names): (Vec<std::path::PathBuf>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.lsp_config_path.clone().map(|path| (path, p.name.as_str()))
                })
                .unzip();
            let (
                plugin_inline_lsp,
                inline_names,
            ): (Vec<&serde_json::Value>, Vec<&str>) = active
                .iter()
                .filter_map(|p| {
                    p.inline_lsp_servers.as_ref().map(|v| (v, p.name.as_str()))
                })
                .unzip();
            let sourced = kigi_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
                tool_ctx.cwd.as_path(),
                &plugin_lsp_paths,
                &plugin_inline_lsp,
                &plugin_names,
                &inline_names,
            );
            let servers = folder_trust::filter_untrusted_project_lsp(
                tool_ctx.cwd.as_path(),
                sourced,
            );
            tool_ctx.lsp_server_names = servers.keys().cloned().collect();
            if servers.is_empty() {
                let user_path = kigi_tools::util::kigi_home::kigi_home()
                    .join("lsp.json");
                let project_path = tool_ctx.cwd.as_path().join(".kigi").join("lsp.json");
                tracing::warn!(
                    cwd = % tool_ctx.cwd, user_lsp_path = % user_path.display(),
                    project_lsp_path = % project_path.display(),
                    "LSP tools enabled, but no language servers are configured"
                );
            } else {
                use kigi_tools::implementations::lsp::{
                    LspBackend, LspBackendAdapter, LspManager,
                };
                let mgr = std::sync::Arc::new(
                    tokio::sync::Mutex::new(
                        LspManager::new(
                            servers,
                            tool_ctx.cwd.as_path().to_path_buf(),
                            true,
                            kigi_tools::notification::ToolNotificationHandle::noop(),
                        ),
                    ),
                );
                let adapter = std::sync::Arc::new(LspBackendAdapter::new(mgr));
                adapter.ensure_started_background();
                tool_ctx.lsp = Some(adapter as std::sync::Arc<dyn LspBackend>);
            }
        }
        let inference_idle_timeout_secs = {
            let models = self.models_manager.models();
            let cfg = self.cfg.borrow();
            resolve_inference_idle_timeout_secs(
                &models,
                &sampling_config.model,
                cfg.remote_settings.as_ref(),
            )
        };
        let model_max_retries = self
            .models_manager
            .models()
            .values()
            .find(|entry| entry.info.model == sampling_config.model)
            .and_then(|entry| entry.info.max_retries);
        let origin_client = self.origin_client_info_from_meta(init.meta.as_ref());
        let web_search_config = self.prepare_web_search_config();
        let app_builder_deployer_config = self.prepare_app_builder_deployer_config();
        let web_fetch_config = self.prepare_web_fetch_config();
        let write_file_enabled = self.cfg.borrow().resolve_write_file().value;
        let goal_enabled = self.cfg.borrow().resolve_goal().value;
        let graph_enabled = self.cfg.borrow().resolve_graph().value;
        let subagents_enabled = self.cfg.borrow().subagents_enabled;
        let ask_user_question_enabled = parse_ask_user_question_from_meta(session_meta)
            .unwrap_or_else(|| self.cfg.borrow().resolve_ask_user_question().value);
        let client_hooks = crate::extensions::hooks::parse_client_hooks(session_meta);
        let disable_web_search = self.cfg.borrow().disable_web_search;
        let todo_gate = self.cfg.borrow().todo_gate;
        let remote_settings_for_spawn = self.cfg.borrow().remote_settings.clone();
        let laziness_debug_log_for_spawn = self.cfg.borrow().laziness_debug_log.clone();
        let respect_gitignore = self.cfg.borrow().respect_gitignore;
        let path_not_found_hints = self.cfg.borrow().path_not_found_hints;
        let subagent_toggle = self.subagent_toggle.clone();
        let handle_display_cwd = prompt_display_cwd.clone();
        let auth_manager = Some(self.auth_manager.clone());
        let bash_params_json = {
            let cfg = self.cfg.borrow();
            let remote_auto_bg = cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.auto_background_on_timeout);
            let remote_allow_background_operator = cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.allow_background_operator);
            cfg.toolset
                .bash
                .to_bash_params_json(remote_auto_bg, remote_allow_background_operator)
        };
        let ask_user_question_params_json = {
            let cfg = self.cfg.borrow();
            let params = crate::util::config::resolve_ask_user_question_params_from_disk(
                cfg.remote_settings.as_ref(),
            );
            match serde_json::to_value(params) {
                Ok(serde_json::Value::Object(map)) => Some(map),
                _ => None,
            }
        };
        let tool_params_json = crate::session::agent_rebuild::ResolvedToolParamsJson {
            bash: Some(bash_params_json),
            ask_user_question: ask_user_question_params_json,
        };
        let backend_tools_enabled = {
            let cfg = self.cfg.borrow();
            cfg.resolve_backend_tools().value
        };
        let init_meta = self
            .initialize_request
            .get()
            .and_then(|init| init.meta.as_ref());
        if let Some(override_prompt) = system_prompt_override_from_meta(
            session_meta,
            init_meta,
        ) && !chat_history.is_empty() && !startup_hints.preserve_inherited_system
        {
            let changed = replace_or_insert_system_head(
                &mut chat_history,
                override_prompt,
            );
            if changed {
                tracing::info!(
                    session_id = % session_info.id.0, prompt_len = override_prompt.len(),
                    "cold-load: applied systemPromptOverride to loaded head"
                );
            } else {
                tracing::debug!(
                    session_id = % session_info.id.0,
                    "cold-load: systemPromptOverride already matches head, no-op"
                );
            }
        }
        let (mut handle, agent_system_prompt, session_thread) = {
            let _timer = crate::instrumentation_timer!("session.spawn_actor_call");
            let session_key = self.auth_manager.current_or_expired().map(|a| a.key);
            let credentials = kigi_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type(
                    sampling_config.model.as_str(),
                    session_key.as_deref(),
                    self.auth_type(),
                ),
                alpha_test_key: self.alpha_test_key(),
            };
            let attribution_callback: Option<
                kigi_sampler::SharedAttributionCallback,
            > = Some(
                crate::auth::attribution::ShellAttribution::new(
                    self.auth_manager.clone(),
                    Some(session_info.id.0.to_string()),
                ),
            );
            let agent_hook_registry_override = agent_definition
                .hooks
                .as_ref()
                .and_then(|hooks_config| {
                    let hooks_val = hooks_config.as_value();
                    let (specs, errors) = kigi_hooks::config::parse_hooks_from_value_with_dir(
                        &hooks_val,
                        &format!("agent:{}", agent_definition.name),
                        std::path::Path::new(&session_info.cwd),
                    );
                    for e in &errors {
                        tracing::warn!(
                            agent = % agent_definition.name, error = ? e,
                            "agent hook parse error"
                        );
                    }
                    if specs.is_empty() {
                        return None;
                    }
                    let cwd = std::path::Path::new(&session_info.cwd);
                    let hooks_trusted = folder_trust::project_scope_allowed(cwd);
                    let git_root = kigi_workspace::session::git::find_git_root_from_path(
                            cwd,
                        )
                        .ok();
                    let (disk_registry, disk_errors) = crate::util::hooks::discover_hooks(
                        git_root.as_deref(),
                        &compat,
                        hooks_trusted,
                    );
                    for e in &disk_errors {
                        tracing::warn!(error = ? e, "hook loading error");
                    }
                    let mut merged = disk_registry;
                    if folder_trust::agent_inline_hooks_allowed(
                        agent_definition.scope,
                        || hooks_trusted,
                    ) {
                        merged.append_specs(specs);
                    }
                    Some(std::sync::Arc::new(merged))
                });
            let initial_reasoning_effort = chat_history
                .is_empty()
                .then_some(sampling_config.reasoning_effort);
            let _ = persistence
                .tx
                .send(crate::session::persistence::PersistenceMsg::CurrentModel {
                    model_id: session_model_id.clone(),
                    agent_name: Some(agent_definition.name.clone()),
                    reasoning_effort: initial_reasoning_effort,
                });
            let acp_mcp_servers = crate::session::acp_mcp::parse_acp_mcp_servers(
                session_meta,
            );
            let git_head_changed = init
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|m| m.get("kigi/gitHeadChanged"))
                .and_then(|v| v.as_bool());
            let fs_watch_caps = crate::session::fs_watch::FsWatchCapabilities::resolve(crate::session::fs_watch::CapabilityInputs {
                client_notify: fs_notify_config.is_some(),
                hunk_tracking: hunk_plan.enabled(),
                code_nav: client_code_nav_enabled,
                git_head_changed,
            });
            spawn_session_on_thread(
                    session_info.clone(),
                    self.gateway.clone(),
                    sampling_config,
                    credentials,
                    auth_method_id,
                    auth_manager,
                    attribution_callback,
                    tool_ctx,
                    mcp_servers,
                    initial_client_mcp_servers,
                    mcp_meta_config_map,
                    None,
                    acp_mcp_servers,
                    support_permission,
                    auto_update,
                    persistence,
                    chat_history.clone(),
                    rewind_points_file_path,
                    fs_notify_config,
                    initial_total_tokens,
                    startup_hints,
                    client_type,
                    auto_compact_threshold_percent,
                    system_prompt_label,
                    compaction_mode,
                    compaction_verbatim_input,
                    two_pass_enabled,
                    buffering_settings,
                    origin_client.clone(),
                    self.codebase_indexes.clone(),
                    client_code_nav_enabled,
                    fs_watch_caps,
                    feedback_base_url,
                    client_terminal,
                    client_fs_read && client_fs_write,
                    gateway_enabled,
                    agent_definition,
                    session_default_agent_profile,
                    skills,
                    None,
                    compat,
                    incremental_bash_output,
                    persisted_signals,
                    persisted_plan_mode,
                    persisted_goal_mode,
                    persisted_graph_mode,
                    persisted_announcement_state,
                    self.memory_config.clone(),
                    feedback_flags,
                    session_model_id,
                    session_yolo_mode,
                    session_auto_mode,
                    origin_client.as_ref().map(|o| o.product.clone()),
                    inference_idle_timeout_secs,
                    model_max_retries,
                    web_search_config,
                    web_fetch_config,
                    app_builder_deployer_config,
                    write_file_enabled,
                    goal_enabled,
                    graph_enabled,
                    subagents_enabled,
                    ask_user_question_enabled,
                    client_hooks,
                    prompt_display_cwd,
                    subagent_toggle,
                    self.persona_io_summaries.clone(),
                    kigi_agent::prompt::context::PromptAudience::Primary,
                    None,
                    None,
                    disable_web_search,
                    backend_tools_enabled,
                    respect_gitignore,
                    path_not_found_hints,
                    tool_params_json,
                    {
                        let session_cwd = std::path::Path::new(&session_info.cwd);
                        let disk_cfg = crate::config::resolve_effective_plugins_config(
                                session_cwd,
                            )
                            .to_discovery_config();
                        self.plugin_registry_handle
                            .refresh_and_build_for_cwd(
                                session_cwd,
                                &disk_cfg,
                                &parse_session_plugin_dirs(session_meta),
                                folder_trust::project_scope_allowed(session_cwd),
                            )
                    },
                    Some(self.plugin_registry_handle.clone()),
                    self.models_manager.clone(),
                    None,
                    None,
                    Some(
                        Arc::new(
                            crate::auth::manager::SharedAuthKeyProvider(
                                self.auth_manager.clone(),
                            ),
                        ),
                    ),
                    self.resolve_image_description_model(),
                    agent_hook_registry_override,
                    workspace_ops.clone(),
                    {
                        let cfg = self.cfg.borrow();
                        cfg.cli_agent_overrides.permission_rules.clone()
                    },
                    todo_gate,
                    remote_settings_for_spawn,
                    laziness_debug_log_for_spawn,
                    None,
                    None,
                    max_turns,
                    None,
                )
                .await?
        };
        self.session_threads
            .borrow_mut()
            .insert(session_info.id.clone(), session_thread);
        tracing::debug!(
            session_id = % session_info.id.0, "spawn_session_on_thread complete"
        );
        self.set_session_live_state(&session_info.id, SessionLiveState::IdleResident);
        self.ensure_session_supervisor();
        self.push_roster_delta_upserted(&session_info.id);
        if chat_history.is_empty() {
            let _timer = crate::instrumentation_timer!("session.system_prompt_inject");
            let system_prompt = build_spawn_system_prompt(
                session_meta,
                init_meta,
                &agent_system_prompt,
            );
            tracing::debug!(session_id = % session_info.id.0, "built system prompt");
            let _ = handle
                .cmd_tx
                .send(SessionCommand::Initialize {
                    system_prompt,
                });
            tracing::debug!(
                session_id = % session_info.id.0, "enqueued SessionCommand::Initialize"
            );
        }
        let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
        if let Some(mut loc_rx) = loc_aggregate_rx {
            let signals = handle.signals_handle.clone();
            tokio::spawn(async move {
                while let Some(agg) = loc_rx.recv().await {
                    match agg {
                        kigi_hunk_tracker::LocAggregate::LinesChanged {
                            author_type,
                            lines_added,
                            lines_removed,
                            file_path,
                        } => {
                            let is_agent = author_type
                                == kigi_hunk_tracker::AuthorType::Agent;
                            signals
                                .record_loc_change(
                                    is_agent,
                                    lines_added,
                                    lines_removed,
                                    file_path,
                                );
                        }
                        kigi_hunk_tracker::LocAggregate::LinesReverted {
                            lines_added_reverted,
                            lines_removed_reverted,
                        } => {
                            signals
                                .record_loc_revert(
                                    lines_added_reverted,
                                    lines_removed_reverted,
                                );
                        }
                    }
                }
            });
        }
        if handle_display_cwd.is_some() {
            handle.display_cwd = handle_display_cwd;
        }
        let source = if chat_history.is_empty() { "new" } else { "load" };
        let _ = handle
            .cmd_tx
            .send(SessionCommand::DispatchSessionStartHook {
                source: source.to_string(),
            });
        self.notify_session_cwd_for_watch(std::path::Path::new(&session_info.cwd));
        self.activity.register_session(&session_info.id.0, &handle);
        self.sessions.borrow_mut().insert(session_info.id.clone(), handle);
        let cwd_for_maintenance = session_info.cwd.clone();
        tokio::spawn(async move {
            crate::session::prompt_history::truncate_if_needed_async(cwd_for_maintenance)
                .await;
        });
        Ok(())
    }
}
