use super::*;

impl SessionActor {
    /// Execute a built-in slash command (e.g. `/compact`, `/yolo`).
    pub(super) async fn execute_builtin_slash_command(
        self: &Arc<Self>,
        action: BuiltinAction,
    ) -> PromptTurnResult {
        match action {
            BuiltinAction::Compact { user_context } => {
                self.run_compact(user_context).await?;
                ok_end_turn(0, None)
            }
            BuiltinAction::SetYolo { enabled } => {
                let was = self.permissions.is_yolo_mode();
                self.permissions.set_yolo_mode(enabled);
                // Report the ACTUAL state, not the request: the manager clamps a
                // requested ON to OFF under the always-approve pin, so `enabled`
                // would mis-report a turn-on (event, telemetry, and the log line)
                // that never happened.
                let actual = self.permissions.is_yolo_mode();
                if let Some(actual) = yolo_toggle_report(was, actual) {
                    self.emit_event(crate::session::events::Event::YoloToggled { enabled: actual });
                    tracing::info_span!(
                        "session.permission_mode_changed",
                        from_mode = crate::session::telemetry::permission_mode_label(was),
                        to_mode = crate::session::telemetry::permission_mode_label(actual),
                        trigger = "slash_command",
                        enabled = actual,
                    )
                    .in_scope(|| {});
                }
                let status = if actual { "enabled" } else { "disabled" };
                tracing::info!(
                    session_id = %self.session_info.id.0,
                    requested = enabled,
                    enabled = actual,
                    "YOLO mode {status} via /yolo slash command",
                );
                ok_end_turn(0, None)
            }
            BuiltinAction::FlushMemory => {
                if self.memory.is_enabled() {
                    let did_flush = self.run_memory_flush("slash_command", None).await;
                    if !did_flush {
                        tracing::info!(
                            session_id = %self.session_info.id.0,
                            "memory flush skipped via /flush: another flush already in progress",
                        );
                    }
                } else {
                    tracing::warn!(
                        session_id = %self.session_info.id.0,
                        "memory flush skipped via /flush: memory not enabled for this session",
                    );
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::Dream => {
                // No user-visible output — intentional, matches /flush behaviour.
                if self.memory.is_enabled() {
                    self.run_dream_slash_command().await;
                } else {
                    tracing::warn!(
                        session_id = %self.session_info.id.0,
                        "dream skipped via /dream: memory not enabled for this session",
                    );
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::ContextInfo => ok_end_turn(0, None),
            BuiltinAction::HooksTrust => {
                let msg = match Self::do_hooks_trust_project(&self.session_info.cwd) {
                    Ok(root) => {
                        format!("Trusted: {}.", root.display())
                    }
                    Err(e) => e,
                };
                self.send_slash_command_output(&msg).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::HooksList => {
                let text = match &*self.hook_registry.borrow() {
                    Some(registry) => {
                        let hooks = registry.all_hooks();
                        if hooks.is_empty() {
                            "No hooks loaded for this session.".to_string()
                        } else {
                            let mut lines = vec![format!("Loaded hooks ({}):", hooks.len())];
                            for spec in &hooks {
                                let matcher_str = spec
                                    .configured_matcher
                                    .as_ref()
                                    .map(|m| format!("  matcher: {m}"))
                                    .unwrap_or_default();
                                let target = if let Some(ref cmd) = spec.command {
                                    format!("command: {}", cmd.display())
                                } else if let Some(ref url) = spec.url {
                                    format!("url: {url}")
                                } else {
                                    "target: <none>".to_string()
                                };
                                lines.push(format!(
                                    "  {}{}  {}  timeout: {}s",
                                    spec.name,
                                    matcher_str,
                                    target,
                                    spec.timeout_ms / 1000,
                                ));
                            }
                            lines.join("\n")
                        }
                    }
                    None => "No hooks loaded for this session.".to_string(),
                };
                self.send_slash_command_output(&text).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::HooksAdd { path } => {
                if path.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /hooks add <path>\nProvide a path to a hook JSON file or directory under ~/.kigi/.",
                    )
                    .await;
                } else {
                    // CWE-427: Use shared add_hooks_path() which validates
                    // paths are under ~/.kigi/ to prevent hook path injection.
                    match crate::config::add_hooks_path(&path) {
                        Ok(()) => {
                            self.send_slash_command_output(&format!(
                                "Added hook path: {path}\n\
                                 Restart session to load hooks from this path."
                            ))
                            .await;
                        }
                        Err(e) => {
                            self.send_slash_command_output(&format!(
                                "Failed to add hook path: {e}"
                            ))
                            .await;
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::HooksRemove { path } => {
                if path.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /hooks-remove <path>\nProvide the path to remove from hooks-paths.",
                    )
                    .await;
                } else {
                    match crate::config::remove_hooks_path(&path) {
                        Ok(()) => {
                            self.send_slash_command_output(&format!(
                                "Removed hook path: {path}\nRestart session to stop loading hooks from this path."
                            ))
                            .await;
                        }
                        Err(e) => {
                            self.send_slash_command_output(&format!(
                                "Failed to remove hook path: {e}"
                            ))
                            .await;
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::HooksUntrust => {
                let msg = match Self::do_hooks_untrust_project(&self.session_info.cwd) {
                    Ok((root, true)) => format!("Untrusted: {}.", root.display()),
                    Ok((root, false)) => format!("Not currently trusted: {}", root.display()),
                    Err(e) => e,
                };
                self.send_slash_command_output(&msg).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsList => {
                let text = match &*self.plugin_registry.borrow() {
                    Some(registry) if !registry.is_empty() => {
                        let mut lines = Vec::new();
                        for plugin in registry.list() {
                            let status = if !plugin.enabled {
                                " [disabled]"
                            } else if !plugin.trusted {
                                " [untrusted]"
                            } else {
                                ""
                            };
                            let version = plugin
                                .version
                                .as_deref()
                                .map(|v| format!(" v{v}"))
                                .unwrap_or_default();
                            lines.push(format!(
                                "  {}{} ({}{})",
                                plugin.name, version, plugin.scope, status,
                            ));
                            let mut components = Vec::new();
                            if plugin.skill_count > 0 {
                                components.push(format!("{} skills", plugin.skill_count));
                            }
                            if plugin.agent_count > 0 {
                                components.push(format!("{} agents", plugin.agent_count));
                            }
                            if plugin.has_hooks {
                                components.push(if plugin.has_inline_hooks_only {
                                    "hooks: active (inline)".into()
                                } else if plugin.trusted {
                                    "hooks: active".into()
                                } else {
                                    "hooks: blocked".into()
                                });
                            }
                            if plugin.mcp_server_count > 0 {
                                components.push(if plugin.has_inline_mcp_only {
                                    format!("{} MCP servers (inline)", plugin.mcp_server_count)
                                } else if plugin.trusted {
                                    format!("{} MCP servers", plugin.mcp_server_count)
                                } else {
                                    format!("{} MCP servers: blocked", plugin.mcp_server_count)
                                });
                            }
                            if !components.is_empty() {
                                lines.push(format!("    {}", components.join(", ")));
                            }
                            if !plugin.trusted {
                                lines.push(format!(
                                    "    Run: /plugins trust {}",
                                    plugin.root.display()
                                ));
                            }
                        }
                        format!(
                            "Installed plugins ({}):\n{}",
                            registry.len(),
                            lines.join("\n")
                        )
                    }
                    _ => "No plugins installed.".to_string(),
                };
                self.send_slash_command_output(&text).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsReload => {
                match &self.plugin_registry_handle {
                    Some(handle) => {
                        // Explicit user reload: force a full local-install re-copy.
                        let msg = self.reload_plugins_impl(handle, true).await;
                        self.send_slash_command_output(&msg).await;
                    }
                    None => {
                        self.send_slash_command_output(
                            "No plugin registry handle available. Start a new session to discover plugins.",
                        )
                        .await;
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsTrust => {
                self.send_slash_command_output(
                    "Trust/untrust has been replaced by enable/disable. Use /plugins enable <id> instead.",
                )
                .await;
                ok_end_turn(0, None)
            }
            BuiltinAction::SessionInfo => {
                let info = self.build_session_info().await;

                let model = info.model.unwrap_or_else(|| "unknown".to_string());
                let model_line = if let Some(ref resolved) = info.resolved_model_id {
                    if resolved != &model {
                        format!("**Model:** {} ({})", model, resolved)
                    } else {
                        format!("**Model:** {}", model)
                    }
                } else {
                    format!("**Model:** {}", model)
                };
                let model_hash_line = if crate::session::acp_types::should_show_model_fingerprint(
                    info.show_model_fingerprint,
                    &model,
                ) {
                    info.model_fingerprint
                        .as_deref()
                        .map(|fp| format!("\n\n**Model Hash:** {fp}"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let ctx = &info.context;
                let context_pct = kigi_token_estimation::usage_percentage(ctx.used, ctx.total);

                let summary_path = crate::session::persistence::session_dir(&self.session_info)
                    .join("summary.json");
                let title = tokio::task::spawn_blocking(move || {
                    std::fs::read_to_string(&summary_path)
                        .ok()
                        .and_then(|raw| {
                            serde_json::from_str::<crate::session::persistence::Summary>(&raw).ok()
                        })
                        .map(|s| s.session_summary)
                        .filter(|s| !s.is_empty())
                })
                .await
                .ok()
                .flatten();

                let title_line = match &title {
                    Some(t) => format!("**Title:** {t}\n\n"),
                    None => String::new(),
                };

                let text = format!(
                    "{}**Session ID:** {}\n\n\
                     **Working directory:** {}\n\n\
                     {}{}\n\n\
                     **Turn:** {}\n\n\
                     **Context:** {} / {} tokens ({:.0}%)",
                    title_line,
                    self.session_info.id.0,
                    self.session_info.cwd,
                    model_line,
                    model_hash_line,
                    info.turn_index,
                    ctx.used,
                    ctx.total,
                    context_pct,
                );
                self.send_slash_command_output(&text).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsAdd { path } => {
                if path.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /plugins add <path>\n\
                         Provide the path to a plugin directory to add.",
                    )
                    .await;
                } else {
                    let resolved = {
                        let p = std::path::Path::new(&path);
                        if p.is_relative() {
                            std::path::PathBuf::from(&self.session_info.cwd).join(p)
                        } else {
                            p.to_path_buf()
                        }
                    };
                    let path_str = resolved.to_string_lossy().to_string();
                    match crate::config::add_plugin_path(&path_str) {
                        Ok(()) => {
                            let msg = format!("Added plugin path: {path_str}");
                            self.send_slash_command_output(&msg).await;
                            if let Some(ref handle) = self.plugin_registry_handle {
                                let reload_msg = self.reload_plugins_impl(handle, false).await;
                                self.send_slash_command_output(&reload_msg).await;
                            }
                        }
                        Err(e) => {
                            self.send_slash_command_output(&format!(
                                "Failed to add plugin path: {e}"
                            ))
                            .await;
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsRemove { path } => {
                if path.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /plugins remove <path>\n\
                         Provide the path to a plugin directory to remove.",
                    )
                    .await;
                } else {
                    let resolved = {
                        let p = std::path::Path::new(&path);
                        if p.is_relative() {
                            std::path::PathBuf::from(&self.session_info.cwd).join(p)
                        } else {
                            p.to_path_buf()
                        }
                    };
                    let path_str = resolved.to_string_lossy().to_string();
                    match crate::config::remove_plugin_path(&path_str) {
                        Ok(()) => {
                            let msg = format!("Removed plugin path: {path_str}");
                            self.send_slash_command_output(&msg).await;
                            if let Some(ref handle) = self.plugin_registry_handle {
                                let reload_msg = self.reload_plugins_impl(handle, false).await;
                                self.send_slash_command_output(&reload_msg).await;
                            }
                        }
                        Err(e) => {
                            self.send_slash_command_output(&format!(
                                "Failed to remove plugin path: {e}"
                            ))
                            .await;
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsInstall { source, trust } => {
                if source.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /plugins install <source>\n\
                         Source can be a git URL or local path.\n\
                         Examples:\n\
                           /plugins install https://github.com/user/my-plugin\n\
                           /plugins install https://github.com/user/repo@v1.0\n\
                           /plugins install ./local-plugin",
                    )
                    .await;
                } else {
                    let cwd = std::path::Path::new(&self.session_info.cwd);

                    if !trust {
                        let install_source =
                            kigi_agent::plugins::git_install::parse_install_source(&source, cwd);
                        let source_desc = match &install_source {
                            kigi_agent::plugins::git_install::InstallSource::Git {
                                url, ..
                            } => {
                                format!("remote git repo: {url}")
                            }
                            kigi_agent::plugins::git_install::InstallSource::Local {
                                path, ..
                            } => {
                                format!("local directory: {}", path.display())
                            }
                        };
                        self.send_slash_command_output(&format!(
                            "About to install plugin from: {source_desc}\n\
                             \n\
                             This will clone/link the source and activate all executable surfaces:\n\
                               - Hook scripts will run on tool use events\n\
                               - MCP servers will be started\n\
                               - Skills will be available to the model\n\
                             \n\
                             To proceed, re-run with --trust:\n\
                               /plugins install {source} --trust"
                        ))
                        .await;
                    } else {
                        match crate::plugin::install_plugin(&source, cwd) {
                            Ok(outcome) => {
                                for w in &outcome.warnings {
                                    tracing::warn!("{w}");
                                }
                                let kind = if outcome.is_local { "local" } else { "git" };
                                tracing::info_span!(
                                    "plugin.installed",
                                    success = true,
                                    install_kind = kind,
                                    plugin_count = outcome.plugin_names.len() as i64,
                                    plugin_name = %outcome.plugin_names.join(","),
                                )
                                .in_scope(|| {});
                                self.send_slash_command_output(&format!(
                                    "Installed {} plugin(s) from {source}: {}\n\
                                     Run /plugins reload to activate.",
                                    outcome.plugin_names.len(),
                                    outcome.plugin_names.join(", "),
                                ))
                                .await;
                            }
                            Err(e) => {
                                let error_category = Self::classify_install_error(&e);
                                let kind = if crate::plugin::install_source_is_local(&source, cwd) {
                                    "local"
                                } else {
                                    "git"
                                };
                                tracing::info_span!(
                                    "plugin.installed",
                                    success = false,
                                    install_kind = kind,
                                    error_category = %error_category,
                                )
                                .in_scope(|| {});
                                self.send_slash_command_output(&format!(
                                    "Failed to install plugin: {e}"
                                ))
                                .await;
                            }
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsUninstall { name, confirm } => {
                if name.is_empty() {
                    self.send_slash_command_output(
                        "Usage: /plugins uninstall <name>\n\
                         Provide the name of an installed plugin to remove.",
                    )
                    .await;
                } else {
                    use crate::plugin::UninstallError;
                    match crate::plugin::uninstall_plugin(&name, confirm, false) {
                        Ok(outcome) => {
                            self.send_slash_command_output(&format!(
                                "Uninstalled repo \"{}\" ({} plugin(s): {})",
                                outcome.repo_key,
                                outcome.removed_plugins.len(),
                                outcome.removed_plugins.join(", "),
                            ))
                            .await;
                        }
                        Err(UninstallError::NeedsConfirm {
                            name,
                            repo_key,
                            other_plugins,
                            total,
                        }) => {
                            self.send_slash_command_output(&format!(
                                "Plugin \"{name}\" belongs to repo \"{repo_key}\" which also contains:\n\
                                 {}\n\
                                 \n\
                                 Uninstalling will remove all {total} plugin(s). To proceed:\n\
                                   /plugins uninstall {name} --confirm\n\
                                 \n\
                                 To disable a single plugin without removing the repo, add to config.toml:\n\
                                   [plugins]\n\
                                   disabled = [\"{name}\"]",
                                other_plugins.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"),
                            ))
                            .await;
                        }
                        Err(UninstallError::NotFound { name }) => {
                            self.send_slash_command_output(&format!(
                                "Plugin \"{name}\" not found in install registry.\n\
                                 Use /plugins list to see installed plugins."
                            ))
                            .await;
                        }
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::PluginsUpdate { name } => {
                use crate::plugin::RepoUpdateOutcome;

                match crate::plugin::update_plugins(name.as_deref()) {
                    Ok(outcomes) if outcomes.is_empty() => {
                        self.send_slash_command_output("No installed plugins to update.")
                            .await;
                    }
                    Ok(outcomes) => {
                        fn short(c: Option<&str>) -> &str {
                            c.map(|s| &s[..7.min(s.len())]).unwrap_or("?")
                        }
                        let messages: Vec<String> = outcomes
                            .iter()
                            .map(|o| match o {
                                RepoUpdateOutcome::Updated { repo_key, old_commit, new_commit } => {
                                    format!(
                                        "{repo_key}: updated ({} -> {})",
                                        short(old_commit.as_deref()),
                                        short(new_commit.as_deref()),
                                    )
                                }
                                RepoUpdateOutcome::AlreadyUpToDate { repo_key } => {
                                    format!("{repo_key}: already up to date")
                                }
                                RepoUpdateOutcome::Pinned { repo_key, ref_name } => {
                                    format!("{repo_key}: pinned to {ref_name} (use /plugins install <url>@<new-ref> to switch)")
                                }
                                RepoUpdateOutcome::LiveLocal { repo_key } => {
                                    format!("{repo_key}: local symlink (already live, no update needed)")
                                }
                                RepoUpdateOutcome::Failed { repo_key, error } => {
                                    format!("{repo_key}: update failed: {error}")
                                }
                            })
                            .collect();
                        self.send_slash_command_output(&messages.join("\n")).await;
                    }
                    Err(e) => {
                        self.send_slash_command_output(&format!("{e}")).await;
                    }
                }
                ok_end_turn(0, None)
            }
            BuiltinAction::Feedback { text } => self.execute_feedback_command(text).await,
            BuiltinAction::MemoryBrowse => {
                let file_infos = if let Some(ref storage) = *self.memory.storage.borrow() {
                    match storage.list_memory_files() {
                        Ok(files) => files
                            .into_iter()
                            .map(|path| {
                                let meta = match std::fs::metadata(&path) {
                                    Ok(m) => Some(m),
                                    Err(e) => {
                                        tracing::debug!(
                                            path = %path.display(),
                                            error = %e,
                                            "skipping memory file with unreadable metadata",
                                        );
                                        None
                                    }
                                };
                                crate::extensions::notification::MemoryFileInfo {
                                    source: storage.classify_source(&path).to_string(),
                                    path: path.display().to_string(),
                                    size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                                    modified_epoch_secs: meta
                                        .and_then(|m| m.modified().ok())
                                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                        .map(|d| d.as_secs()),
                                }
                            })
                            .collect(),
                        Err(e) => {
                            tracing::warn!(
                                session_id = %self.session_info.id.0,
                                error = %e,
                                "failed to list memory files",
                            );
                            self.send_slash_command_output(&format!(
                                "Failed to list memory files: {e}"
                            ))
                            .await;
                            vec![]
                        }
                    }
                } else {
                    self.send_slash_command_output("Memory is not enabled for this session.")
                        .await;
                    vec![]
                };
                tracing::info!(
                    session_id = %self.session_info.id.0,
                    file_count = file_infos.len(),
                    "memory browse: listing files",
                );
                self.send_xai_notification(XaiSessionUpdate::MemoryFiles { files: file_infos })
                    .await;
                ok_end_turn(0, None)
            }
            BuiltinAction::MemoryToggle { enabled } => {
                tracing::info!(
                    session_id = %self.session_info.id.0,
                    enabled,
                    "memory toggle via /memory slash command",
                );
                let msg = if enabled && !self.memory.is_enabled() {
                    if let Some(ref params) = self.memory.backend_params {
                        let storage = crate::session::memory::MemoryStorage::new(
                            std::path::Path::new(&self.session_info.cwd),
                            None,
                        );
                        if let Err(e) = storage.ensure_initialized() {
                            tracing::warn!(error = %e, "failed to initialize memory storage on re-enable");
                            format!("Memory could not be enabled: {e}")
                        } else {
                            let backend =
                                crate::session::memory::MemoryBackendImpl::from_session_params(
                                    storage.clone(),
                                    params,
                                );
                            *self.memory.search_counter.borrow_mut() =
                                Some(backend.search_counter.clone());
                            let backend: std::sync::Arc<
                                dyn kigi_tools::types::memory_backend::MemoryBackend,
                            > = std::sync::Arc::new(backend);
                            let bridge = self.agent.borrow().tool_bridge().clone();
                            bridge.update_resource(backend.clone()).await;
                            if let Err(e) = self.register_memory_tools(&bridge).await {
                                tracing::warn!(error = %e, "memory tool registration failed during toggle");
                            }
                            *self.memory.storage.borrow_mut() = Some(storage);
                            "Memory enabled for this session.".to_owned()
                        }
                    } else {
                        "Memory cannot be enabled (not configured for this session).".to_owned()
                    }
                } else if !enabled && self.memory.is_enabled() {
                    let bridge = self.agent.borrow().tool_bridge().clone();
                    if !bridge.unregister_tool_by_name(
                        kigi_tools::implementations::memory::MEMORY_SEARCH_TOOL_NAME,
                    ) {
                        tracing::debug!("memory_search tool was not registered during unregister");
                    }
                    if !bridge.unregister_tool_by_name(
                        kigi_tools::implementations::memory::MEMORY_GET_TOOL_NAME,
                    ) {
                        tracing::debug!("memory_get tool was not registered during unregister");
                    }
                    *self.memory.storage.borrow_mut() = None;
                    *self.memory.search_counter.borrow_mut() = None;
                    "Memory disabled for this session.".to_owned()
                } else {
                    let state = if enabled { "enabled" } else { "disabled" };
                    format!("Memory is already {state}.")
                };
                self.send_slash_command_output(&msg).await;
                self.refresh_goal_harness_enabled().await;
                ok_end_turn(0, None)
            }
            // GoalSet is handled directly in handle_prompt (before this
            // function is called) so the turn flows through to model inference
            // instead of ending immediately.
            BuiltinAction::GoalSet { .. } => {
                unreachable!("GoalSet is intercepted in handle_prompt")
            }
            BuiltinAction::GoalStatus => {
                let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                let goal_tokens = self.goal_tokens_used(current_tokens);
                let msg = {
                    let mut tracker = self.goal_tracker.lock();
                    // Flush pending wall-clock delta so elapsed_ms is up to date.
                    tracker.account_elapsed();
                    match tracker.snapshot() {
                        Some(o) => {
                            let phase = format!("{:?}", o.phase);
                            let status = format!("{:?}", o.status);
                            let elapsed =
                                crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
                            // `goal_tokens` already includes live subagent
                            // spend via the token records — no live fold-in.
                            let mut buf = format!(
                                "Goal: {}\n\
                                 Status: {status} | Phase: {phase}\n\
                                 Tokens used: {goal_tokens}\n\
                                 Elapsed: {elapsed}",
                                o.objective,
                            );
                            if let Some(budget) = o.token_budget {
                                buf.push_str(&format!(" | Budget: {budget}"));
                            }
                            if let Some(ref role) = o.current_subagent_role {
                                buf.push_str(&format!("\nActive subagent: {role}"));
                            }
                            buf
                        }
                        None => "No goal is currently set. Use /goal <objective> to start one."
                            .to_string(),
                    }
                };
                self.send_slash_command_output(&msg).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::GoalPause => {
                if self.graph_owns_goal_engine() {
                    self.send_slash_command_output(
                        "A graph owns the goal engine. Use /graph pause instead.",
                    )
                    .await;
                    return ok_end_turn(0, None);
                }
                let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                use crate::session::goal_tracker::{GoalPauseReason, GoalStatus};
                let (msg, changed) = {
                    let mut tracker = self.goal_tracker.lock();
                    match tracker.status() {
                        Some(GoalStatus::Active) => {
                            // Side effect OUTSIDE the assert: debug_assert!
                            // strips its condition in release builds, which
                            // would silently skip the pause itself.
                            let paused = tracker.pause(GoalPauseReason::User);
                            debug_assert!(paused, "Active goal must pause");
                            ("Goal paused. Use /goal resume to continue.", true)
                        }
                        Some(
                            GoalStatus::UserPaused
                            | GoalStatus::BackOffPaused
                            | GoalStatus::NoProgressPaused
                            | GoalStatus::InfraPaused
                            | GoalStatus::Blocked,
                        ) => ("Goal is already paused.", false),
                        Some(GoalStatus::Complete) => ("Goal is already complete.", false),
                        Some(GoalStatus::BudgetLimited) => ("Goal is budget-limited.", false),
                        None => ("No goal is currently set.", false),
                    }
                };
                if changed {
                    self.clear_pending_classifier_completions();
                    let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(
                        &mut self.goal_tracker.lock(),
                        tokens_used,
                        finished_marginal,
                    );
                }
                self.send_slash_command_output(msg).await;
                ok_end_turn(0, None)
            }
            // GoalResume is intercepted in handle_prompt (like GoalSet) so a
            // successful resume flows through to inference — see `resume_goal`.
            BuiltinAction::GoalResume => {
                unreachable!("GoalResume is intercepted in handle_prompt")
            }
            BuiltinAction::GoalClear => {
                if self.graph_owns_goal_engine() {
                    self.send_slash_command_output(
                        "A graph owns the goal engine. Use /graph clear instead.",
                    )
                    .await;
                    return ok_end_turn(0, None);
                }
                // `/goal clear` is a deliberate user reset — the shared
                // helper drops the tracker, both streaks, goal-turn task
                // ids, per-subagent token records, pending classifier
                // claims, and notifies the pager. Shared with the graph
                // node boundary and `/graph clear`.
                self.reset_goal_engine_state().await;
                self.send_slash_command_output("Goal cleared.").await;
                ok_end_turn(0, None)
            }
            BuiltinAction::GraphStatus => {
                let msg = self.graph_status_message().await;
                self.send_slash_command_output(&msg).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::GraphPause => {
                use crate::session::goal_tracker::{GoalPauseReason, GoalStatus};
                let (msg, changed) = {
                    let mut tracker = self.graph_tracker.lock();
                    match tracker.status() {
                        Some(GoalStatus::Active) => {
                            // Side effect OUTSIDE the assert: debug_assert!
                            // strips its condition in release builds, which
                            // would silently skip the pause itself.
                            let paused = tracker.pause(GoalPauseReason::User);
                            debug_assert!(paused, "Active graph must pause");
                            ("Graph paused. Use /graph resume to continue.", true)
                        }
                        Some(s) if s.is_paused() => ("Graph is already paused.", false),
                        Some(GoalStatus::Complete) => ("Graph is already complete.", false),
                        Some(GoalStatus::BudgetLimited) => ("Graph is budget-limited.", false),
                        Some(_) | None => ("No graph is currently set.", false),
                    }
                };
                if changed {
                    // Pause the running node's goal too so the in-turn
                    // loop stops at the next round boundary.
                    self.auto_pause_goal_if_active(GoalPauseReason::User).await;
                    self.persist_graph_state();
                }
                self.send_slash_command_output(msg).await;
                ok_end_turn(0, None)
            }
            BuiltinAction::GraphClear => {
                let had_graph = self.graph_tracker.lock().snapshot().is_some();
                // Clear the goal engine ONLY when the graph owns it
                // (Active/paused ⇒ the engine's goal is a node goal). A
                // terminal graph (Complete/BudgetLimited) may coexist
                // with an unrelated standalone /goal the user started
                // afterwards — that goal must survive /graph clear.
                if self.graph_owns_goal_engine() {
                    self.reset_goal_engine_state().await;
                }
                // Projection teardown only when there is something of
                // OURS to un-project: with no session graph, deleting
                // .kigi/graph.jsonl would destroy another session's
                // revivable graph while replying "No graph is set".
                let session_graph_id = self
                    .graph_tracker
                    .lock()
                    .snapshot()
                    .map(|s| s.graph_id.clone());
                self.graph_tracker.lock().clear();
                if had_graph {
                    // Take the writer lock if we don't hold it (e.g. a
                    // session-snapshot-restored graph cleared before any
                    // resume). Busy = another instance owns the project
                    // graph; local-only clear is then correct.
                    match self.acquire_project_graph_writer() {
                        Ok(true) => {
                            // Identity check: only remove a projection
                            // that belongs to the graph being cleared.
                            let foreign = match (self.projected_graph_id(), &session_graph_id) {
                                (Some(projected), Some(ours)) => projected != *ours,
                                _ => false,
                            };
                            if foreign {
                                tracing::info!(
                                    "graph clear: projection belongs to a different \
                                     graph; leaving .kigi/graph.jsonl in place"
                                );
                                self.graph_project_lock.borrow_mut().take();
                            }
                        }
                        Ok(false) => {
                            tracing::info!(
                                "graph clear: another instance holds the project \
                                 graph; local session state cleared only"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                %err,
                                "graph clear: project lock acquisition failed; \
                                 .kigi/graph.jsonl may survive as stale"
                            );
                        }
                    }
                }
                // persist runs BEFORE the lock drops so the projection
                // removal (when we hold writer rights on OUR graph)
                // executes; without the lock it is a session-only clear.
                self.persist_graph_state();
                self.graph_project_lock.borrow_mut().take();
                self.send_slash_command_output(if had_graph {
                    "Graph cleared."
                } else {
                    "No graph is currently set."
                })
                .await;
                ok_end_turn(0, None)
            }
            // GraphSet / GraphResume are intercepted in handle_prompt
            // (like GoalSet / GoalResume) so a successful setup/resume
            // flows through to model inference.
            BuiltinAction::GraphSet { .. } => {
                unreachable!("GraphSet is intercepted in handle_prompt")
            }
            BuiltinAction::GraphResume { .. } => {
                unreachable!("GraphResume is intercepted in handle_prompt")
            }
        }
    }

    async fn execute_feedback_command(self: &Arc<Self>, text: String) -> PromptTurnResult {
        if text.is_empty() {
            self.send_slash_command_output("Usage: /feedback <text>")
                .await;
            return ok_end_turn(0, None);
        }

        let (sampling_config, model_metadata, credentials) = tokio::join!(
            self.chat_state_handle.get_sampling_config(),
            self.chat_state_handle.get_last_model_metadata(),
            self.chat_state_handle.get_credentials(),
        );
        let model_id = sampling_config.map(|c| c.model);
        let resolved_model_id = model_metadata.resolved_model_id;
        let client_version = Some(kigi_version::VERSION.to_string());

        use crate::session::feedback_manager::{SessionFeedbackData, SubmitOutcome};
        let outcome = self
            .feedback_manager
            .submit_text_feedback(
                text,
                SessionFeedbackData {
                    model_id,
                    resolved_model_id,
                    client_version,
                    session_cwd: self.session_info.cwd.clone(),
                },
                Some(&self.notifications.persistence_tx),
            )
            .await;

        match outcome {
            SubmitOutcome::Submitted => {
                self.send_slash_command_output("Feedback submitted. Thank you!")
                    .await;
            }
            SubmitOutcome::LocalOnly => {
                self.send_slash_command_output(
                    "Feedback saved locally; no feedback server is configured for this session.",
                )
                .await;
            }
            SubmitOutcome::Failed(err) => {
                tracing::warn!(error = %err, "feedback submission failed");
                self.send_slash_command_output(
                    "Feedback saved locally; submitting to the server failed (see logs).",
                )
                .await;
            }
        }

        ok_end_turn(0, None)
    }
}
