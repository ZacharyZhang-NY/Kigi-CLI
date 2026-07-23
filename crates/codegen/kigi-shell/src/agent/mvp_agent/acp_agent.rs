#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! [`acp::Agent`] trait implementation for [`MvpAgent`].
use super::*;
#[async_trait::async_trait(?Send)]
impl acp::Agent for MvpAgent {
    /// SINGLE-CALL INVARIANT: this method is the sole writer of
    /// `self.auth_method_id` during initialization. It is called exactly once
    /// per agent process by the ACP server before any session-creating
    /// requests, while `auth_method_id` is still `None` (initialized at
    /// `MvpAgent::new`). The auth-method block below relies on that
    /// invariant when it unconditionally writes the default id returned by
    /// `auth_method::build_auth_methods`. If you ever need to call
    /// `initialize()` more than once, restore an `is_none()` guard around
    /// the `auth_method_id` write at the call site so a re-init doesn't
    /// silently downgrade an api-key user to a session-token user.
    async fn initialize(
        &self,
        arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!(target : "sampling_log", "Received initialize request");
        kigi_log::unified_log::info("agent initialized", None, None);
        self.start_subagent_coordinator();
        tokio::task::spawn_blocking(|| {
            crate::session::worktree_pool::cleanup_stale_pool_worktrees(None);
        });
        tokio::task::spawn_blocking(|| {
            crate::session::persistence::cleanup_stale_sessions(None);
        });
        {
            let root = crate::util::kigi_home::kigi_home();
            crate::session::storage::search::SEARCH_INDEX_MANAGER.bootstrap_once(root);
        }
        const PERMISSION_CLEANUP_TTL_DAYS: u64 = 30;
        static CLEANUP_PERMISSIONS_ONCE: std::sync::Once = std::sync::Once::new();
        CLEANUP_PERMISSIONS_ONCE
            .call_once(|| {
                tokio::task::spawn(
                    kigi_workspace::permission::cleanup_stale_permission_state(
                        std::time::Duration::from_secs(
                            PERMISSION_CLEANUP_TTL_DAYS * 24 * 60 * 60,
                        ),
                    ),
                );
            });
        kigi_workspace::trust::migrate_legacy_hook_trust();
        let mut client_type = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientType"))
            .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
            .unwrap_or_default();
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref id) = client_identifier {
            tracing::info!("Client identifier set to: {}", id);
        }
        if client_type == ClientType::Generic {
            match client_identifier.as_deref() {
                Some("kigi-web") => client_type = ClientType::KigiWeb,
                Some("nebula") => client_type = ClientType::Nebula,
                Some("kigi-code-extension") => client_type = ClientType::Extension,
                Some("kigi-desktop") => client_type = ClientType::Desktop,
                _ => {}
            }
        }
        *self.client_type.borrow_mut() = client_type;
        tracing::info!("Client type set to: {:?}", client_type);
        let code_nav_enabled = Self::parse_code_nav_capability(&arguments);
        self.code_nav_enabled.set(code_nav_enabled);
        tracing::info!(
            code_nav_enabled, client_type = ? client_type, event =
            "code_nav_capability_parsed",
            "code-nav capability initialized from initialize request; \
             index will start lazily on first kigi/code/* request if eligible"
        );
        let interactive_trust_client = Self::parse_interactive_trust_capability(
            &arguments,
        );
        self.interactive_trust_client.set(interactive_trust_client);
        let client_supports_mcp_apps = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mcpApps"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if client_supports_mcp_apps {
            tracing::info!("Client supports MCP Apps");
        }
        let buffering_settings = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("bufferingSettings"))
            .map(|value| serde_json::from_value::<
                update_chunk_merge::BufferingSettings,
            >(value.clone()))
            .transpose()
            .map_err(|err| {
                tracing::warn!(
                    error = ? err, "Failed to parse buffering settings from init meta"
                );
                err
            })
            .unwrap_or(None);
        tracing::info!(? buffering_settings, "Buffering settings from init");
        *self.buffering_settings.borrow_mut() = buffering_settings;
        if self.initialize_request.set(arguments).is_err() {
            tracing::info!("Initialize called on reconnect (already initialized)");
        }
        let pre = self
            .auth_manager
            .current()
            .map(|a| (
                crate::auth::token_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| crate::auth::token_suffix(t).to_owned()),
            ));
        self.auth_manager.force_reload_from_disk();
        let post = self
            .auth_manager
            .current()
            .map(|a| (
                crate::auth::token_suffix(&a.key).to_owned(),
                a
                    .refresh_token
                    .as_deref()
                    .map(|t| crate::auth::token_suffix(t).to_owned()),
            ));
        kigi_log::unified_log::info(
            "auth init disk refresh",
            None,
            Some(
                serde_json::json!(
                    { "pre_key" : pre.as_ref().map(| p | & p.0), "pre_rt" : pre.as_ref()
                    .and_then(| p | p.1.as_deref()), "post_key" : post.as_ref().map(| p |
                    & p.0), "post_rt" : post.as_ref().and_then(| p | p.1.as_deref()),
                    "changed" : pre.as_ref().map(| p | & p.0) != post.as_ref().map(| p |
                    & p.0), }
                ),
            ),
        );
        kigi_log::unified_log::info(
            "auth: initialize() refreshed auth state from disk",
            None,
            Some(
                serde_json::json!(
                    { "has_current" : self.auth_manager.current().is_some(), "is_expired"
                    : self.auth_manager.is_expired(), "auth_mode" : self.auth_manager
                    .current().map(| a | format!("{:?}", a.auth_mode)), }
                ),
            ),
        );
        if auth_method::read_xai_api_key_env().is_err()
            && let Some(api_key) = crate::auth::read_api_key(
                &crate::util::kigi_home::kigi_home(),
            )
        {
            unsafe { std::env::set_var("KIGI_API_KEY", &api_key) };
            tracing::info!("auth: loaded API key from auth.json (xai::api_key scope)");
            kigi_log::unified_log::info(
                "auth: loaded API key from auth.json (xai::api_key scope)",
                None,
                None,
            );
        }
        let has_external_api_key =
            auth_method::should_advertise_xai_api_key(self.models_manager.models().values());
        let init_has_current = self.auth_manager.current().is_some();
        let init_is_expired = self.auth_manager.is_expired();
        kigi_log::unified_log::info(
            "auth init token state",
            None,
            Some(
                serde_json::json!(
                    { "has_current" : init_has_current, "is_expired" : init_is_expired, }
                ),
            ),
        );
        let mut has_cached_token = init_has_current;
        if !init_has_current && init_is_expired {
            let refreshed = self.auth_manager.auth().await.is_ok();
            if refreshed {
                tracing::debug!(
                    auth_type = ? self.auth_type(),
                    "auth: initialize() silent refresh succeeded",
                );
                kigi_log::unified_log::info(
                    "auth: initialize() silent refresh succeeded",
                    None,
                    Some(
                        serde_json::json!(
                            { "auth_type" : format!("{:?}", self.auth_type()) }
                        ),
                    ),
                );
                has_cached_token = true;
            } else {
                tracing::warn!(
                    "auth: token expired, silent refresh failed - re-authentication required"
                );
                kigi_log::unified_log::warn(
                    "auth: token expired, silent refresh failed - re-authentication required",
                    None,
                    None,
                );
            }
        }
        tracing::info!("auth: advertising Kimi Code device login auth method");
        let built = auth_method::build_auth_methods(auth_method::AuthMethodsBuildInputs {
            has_external_api_key,
            has_cached_token,
            login_label: None,
        });
        let mut auth_methods = built.methods;
        // Connected badges for the client's login picker: probe stored
        // credentials once (auth.json scopes + resolved platform keys) and
        // stamp `_meta.connected` on every method that already has one.
        {
            let store = crate::auth::read_auth_json(
                &crate::util::kigi_home::kigi_home().join("auth.json"),
            )
            .unwrap_or_default();
            let keys = crate::agent::models::PlatformApiKeys::resolve_from_effective_config();
            let connected = auth_method::connected_method_ids(
                has_cached_token,
                has_external_api_key,
                |scope| store.contains_key(scope),
                |p| keys.key_for(p).is_some(),
            );
            auth_method::stamp_connected_meta(&mut auth_methods, &connected);
        }
        kigi_log::unified_log::info(
            "auth: initialize() built auth_methods for ACP response",
            None,
            Some(
                serde_json::json!(
                    { "kigi_home" : crate ::util::kigi_home::kigi_home().display()
                    .to_string(), "HOME" : std::env::var("HOME").unwrap_or_else(| _ |
                    "(unset)".into()), "has_external_api_key" : has_external_api_key,
                    "has_cached_token" :
                    has_cached_token, "init_has_current" : init_has_current, "init_is_expired" :
                    init_is_expired, "auth_mode" : self.auth_manager.current().map(| a |
                    format!("{:?}", a.auth_mode)), "methods" : auth_methods.iter().map(|
                    m | m.id().0.as_ref()).collect::< Vec < _ >> (),
                    "default_auth_method_id" : built.default_auth_method_id.as_ref()
                    .map(| id | id.0.as_ref()), }
                ),
            ),
        );
        debug_assert!(
            ! has_external_api_key || matches!(auth_methods.first().map(| m |
            auth_method::AuthMethodKind::from_id(m.id())),
            Some(auth_method::AuthMethodKind::XaiApiKey)),
            "BYOK invariant violated: xai.api_key MUST be auth_methods.first() \
             when has_external_api_key is true; got {:?}",
            auth_methods.first().map(| m | m.id()),
        );
        let default_auth_method_id_wire: Option<String> = built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.to_string());
        if let Some(default_id) = built.default_auth_method_id {
            kigi_log::unified_log::info(
                "auth method selection",
                None,
                Some(
                    serde_json::json!(
                        { "default_auth_method_id" : default_id.0.as_ref(),
                        "has_external_api_key" : has_external_api_key, "has_cached_token"
                        : has_cached_token, "methods_first" : auth_methods.first().map(|
                        m | m.id().0.as_ref()), "methods_count" : auth_methods.len(), }
                    ),
                ),
            );
            self.set_auth_method(default_id);
        }
        let current_working_directory = self.launch_cwd.clone();
        let hostname = gethostname::gethostname();
        let mcp_servers: Vec<crate::extensions::mcp::McpServerEntry> = Vec::new();
        self.spawn_initialize_launch_mcp_setup();
        let init_model_state = self.model_state(None);
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::V1)
                .agent_capabilities(
                    acp::AgentCapabilities::new()
                        .load_session(true)
                        .meta(
                            serde_json::json!(
                                { "kigi/fs_notify" : true, "kigi/hooks" : { "blockingEvents"
                                : [kigi_hooks::event::HookEventName::PreToolUse],
                                "decisions" : ["deny"], }, }
                            )
                                .as_object()
                                .cloned(),
                        )
                        .prompt_capabilities(
                            acp::PromptCapabilities::new().embedded_context(true),
                        )
                        .mcp_capabilities(
                            acp::McpCapabilities::new().http(true).sse(true),
                        ),
                )
                .auth_methods(auth_methods)
                .meta({
                    let metadata = parse_json_object_env("KIGI_AGENT_METADATA");
                    serde_json::json!(
                        { "kigiShell" : true, "defaultAuthMethodId" :
                        default_auth_method_id_wire, (kigi_mcp::wire::MCP_SDK) :
                        true, (SESSION_PLUGIN_DIRS_CAPABILITY_KEY) : true,
                        "currentWorkingDirectory" : current_working_directory
                        .to_string_lossy().to_string(), "agentVersion" :
                        kigi_version::VERSION, "agentId" : crate::util::agent_id::agent_id(),
                        "agentInstanceId" : crate::util::agent_id::agent_instance_id(), "hostname" : hostname
                        .to_string_lossy().to_string(), "modelState" : init_model_state,
                        "mcpServers" : mcp_servers, "mcpApps" : client_supports_mcp_apps,
                        "metadata" : metadata, "availableCommands" : crate
                        ::session::slash_commands::builtin_commands(self
                        .command_availability()), "cancelRewind" : self.cfg.borrow()
                        .resolve_cancel_rewind().value, "sessionRecap" : self.cfg
                        .borrow().is_session_recap_enabled(), "voiceMode" : self.cfg
                        .borrow().is_voice_mode_enabled(), }
                    )
                        .as_object()
                        .cloned()
                }),
        )
    }
    async fn authenticate(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        tracing::info!(method = % arguments.method_id.0, "auth: authenticate request");
        kigi_log::unified_log::info(
            "auth started",
            None,
            Some(serde_json::json!({ "method" : arguments.method_id.0.as_ref() })),
        );
        match arguments.method_id.0.as_ref() {
            auth_method::XAI_API_KEY_METHOD_ID => {
                // C1: the SECOND writer of the shared `sampling_config.api_key`.
                // The house `KIGI_API_KEY` is the user's own key for the
                // session's own endpoint, so the stamp requires the authority to
                // classify that endpoint `CredentialClass::Primary` — NOT merely
                // "takes some session credential", which is also true on a
                // subscription-OAuth platform's host, where this key has no
                // business (the shared config is the subagent baseline and the
                // unresolved-model fallback). The key is still persisted to
                // auth.json either way; only the stamp is guarded.
                let takes_house_key = self.shared_config_takes_house_key();
                let mut sampling_config = self.sampling_config.borrow_mut();
                if sampling_config.api_key.is_none() {
                    if let Ok(api_key) = auth_method::read_xai_api_key_env() {
                        if takes_house_key {
                            sampling_config.api_key = Some(api_key.clone());
                        } else {
                            tracing::debug!(
                                model = sampling_config.model.as_str(),
                                "auth: house api key withheld from the shared sampling config \
                                 (its endpoint is not this session's own)"
                            );
                        }
                        if let Err(e) = crate::auth::store_api_key(
                            &crate::util::kigi_home::kigi_home(),
                            &api_key,
                        ) {
                            tracing::warn!(
                                "failed to persist API key to auth.json: {e}"
                            );
                            kigi_log::unified_log::warn(
                                "failed to persist API key to auth.json",
                                None,
                                Some(serde_json::json!({ "error" : e.to_string() })),
                            );
                        }
                    } else if !self
                        .models_manager
                        .models()
                        .values()
                        .any(|m| m.has_own_credentials())
                    {
                        emit_login_span(false, "api_key", None, Some("no_credentials"));
                        return Err(
                            acp::Error::auth_required()
                                .data(
                                    "Set KIGI_API_KEY or add api_key/env_key to config.toml.",
                                ),
                        );
                    }
                }
                self.set_auth_method(arguments.method_id.clone());
                emit_login_span(true, "api_key", None, None);
                Ok(Default::default())
            }
            auth_method::CACHED_TOKEN_AUTH_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                if auth_meta.force_interactive {
                    return self
                        .authenticate(
                            acp::AuthenticateRequest::new(
                                    acp::AuthMethodId::new(auth_method::KIMI_CODE_METHOD_ID),
                                )
                                .meta(arguments.meta),
                        )
                        .await;
                }
                let has_current = self.auth_manager.current().is_some();
                let is_expired = self.auth_manager.is_expired();
                kigi_log::unified_log::info(
                    "auth cached_token check",
                    None,
                    Some(
                        serde_json::json!(
                            { "has_current" : has_current, "is_expired" : is_expired, }
                        ),
                    ),
                );
                let Some(_auth) = self.auth_manager.current() else {
                    let message = if self.auth_manager.is_expired() {
                        "Session expired, re-authentication required"
                    } else {
                        "No cached auth token found"
                    };
                    tracing::info!(
                        % message, "cached_token missing/expired, falling through"
                    );
                    kigi_log::unified_log::warn(
                        "auth cached_token fallthrough",
                        None,
                        Some(serde_json::json!({ "reason" : message })),
                    );
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                };
                self.emit_settings_update_notification();
                // H2/C1: route the stamp through the ONE guard, which asks the
                // authority which credential governs the shared config rather
                // than being handed this one. That config may already point at
                // a third-party model or at another provider's subscription
                // host (it is the subagent baseline and the unresolved-model
                // fallback), and `auth.key` authorizes only the session's own
                // coding endpoint. The manager already holds this token, so the
                // authority reads it back where it belongs.
                if self.stamp_session_credential(true) {
                    tracing::debug!(
                        "auth: cached_token handler set api_key (SessionToken)"
                    );
                    kigi_log::unified_log::debug(
                        "auth: cached_token handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.set_auth_method(arguments.method_id.clone());
                let uid = self.auth_manager.current().map(|a| a.user_id);
                emit_login_span(true, "cached_token", uid.as_deref(), None);
                Ok(self.auth_response_with_meta())
            }
            auth_method::KIMI_CODE_METHOD_ID => {
                let kimi_ctx = self.auth_manager.kimi_code_config().clone();
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                tracing::info!(
                    method = arguments.method_id.0.as_ref(), headless = auth_meta
                    .headless, reauth = auth_meta.reauth, "auth: inline auth flow",
                );
                kigi_log::unified_log::info(
                    "auth: inline auth flow",
                    None,
                    Some(
                        serde_json::json!(
                            { "method" : arguments.method_id.0.as_ref(), "headless" :
                            auth_meta.headless, "reauth" : auth_meta.reauth, }
                        ),
                    ),
                );
                if auth_meta.reauth {
                    let _ = self.auth_manager.clear();
                }
                let (auth, _did_auth) = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    *self.auth_code_tx.borrow_mut() = Some(code_tx);
                    *self.auth_url_rx.borrow_mut() = Some(url_rx);
                    let result = crate::auth::run_auth_flow_with_stderr_bridge(
                            &self.auth_manager,
                            &kimi_ctx,
                            crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            },
                            auth_meta.reauth,
                            auth_meta.force_interactive,
                        )
                        .await;
                    *self.auth_code_tx.borrow_mut() = None;
                    *self.auth_url_rx.borrow_mut() = None;
                    result
                } else {
                    crate::auth::run_auth_flow(
                                &self.auth_manager,
                                &kimi_ctx,
                                auth_meta.reauth,
                                None,
                            )
                            .await
                }
                    .map_err(|e| {
                        emit_login_span(
                            false,
                            arguments.method_id.0.as_ref(),
                            None,
                            Some("login_flow_failed"),
                        );
                        let mut err = acp::Error::auth_required();
                        err.message = e.to_string();
                        err
                    })?;
                // C1: hot-swap FIRST, then let the authority read the fresh
                // token back where it belongs. Nothing hand-carries `auth.key`
                // to the shared config — the stamp is whatever
                // credential governs that config's own model + endpoint, which
                // for a session whose current model is another provider's
                // subscription model is that provider's pooled token, and for a
                // third-party host is nothing at all.
                self.auth_manager.hot_swap(auth.clone());
                if self.stamp_session_credential(true) {
                    tracing::debug!(
                        "auth: kimi.com/oidc handler set api_key (SessionToken)"
                    );
                    kigi_log::unified_log::debug(
                        "auth: kimi.com/oidc handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.emit_settings_update_notification();
                        self.set_auth_method(arguments.method_id.clone());
                self.models_manager.on_auth_changed().await;
                emit_login_span(
                    true,
                    arguments.method_id.0.as_ref(),
                    Some(auth.user_id.as_str()),
                    None,
                );
                Ok(self.auth_response_with_meta())
            }
            _ => {
                if let Some(platform) = auth_method::AuthMethodKind::from_id(
                    &arguments.method_id,
                )
                .oauth_platform()
                {
                    self.authenticate_oauth_platform(platform, arguments).await
                } else if let Some(platform) =
                    auth_method::platform_for_method_id(&arguments.method_id)
                {
                    self.authenticate_api_key_platform(
                        platform,
                        arguments.method_id.clone(),
                    )
                    .await
                } else {
                    Err(
                        acp::Error::invalid_params()
                            .data(
                                format!("unsupported auth method: {}", arguments.method_id.0),
                            ),
                    )
                }
            }
        }
    }
    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        tracing::debug!(
            config = ? self.sampling_config, "Received new session request {arguments:?}"
        );
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before new_session")
            })?;
        self.seed_client_config_auth_if_available();
        self.refresh_settings_and_reapply().await;
        let cwd = AbsPathBuf::new(arguments.cwd.clone())
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let initial_client_mcp_servers = arguments.mcp_servers.clone();
        let mcp_servers = self.resolve_mcp_servers(arguments.mcp_servers, cwd.as_path());
        let mcp_meta_config_map = parse_mcp_meta_config(arguments.meta.as_ref());
        let client_session_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sessionId"))
            .and_then(|v| v.as_str());
        let custom_model_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("modelId").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
        #[allow(unused_variables)]
        let session_computer_sessions = parse_session_computer_sessions(
            arguments.meta.as_ref(),
        );
        let is_chat_kind = is_chat_session_kind(arguments.meta.as_ref());
        let session_yolo_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            arguments.meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        let session_id = match client_session_id {
            Some(s) => {
                uuid::Uuid::try_parse(s)
                    .map_err(|e| {
                        acp::Error::invalid_params()
                            .data(
                                format!(
                                    "Invalid UUID format for _meta.sessionId '{}': {}", s, e
                                ),
                            )
                    })?;
                acp::SessionId::new(s.to_string())
            }
            None => acp::SessionId::new(uuid::Uuid::now_v7().to_string()),
        };
        let mut session_timer = crate::instrumentation_timer!("session.new_session");
        session_timer.with_field("session_id", session_id.0.as_ref());
        session_timer.with_field("cwd", cwd.as_str());
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                self
                    .initialize_request
                    .get()
                    .and_then(|req| req.meta.as_ref())
                    .and_then(|m| m.get("clientIdentifier"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let mut model_agent_type: Option<String> = None;
        let mut session_sampling_override: Option<SamplingConfig> = None;
        let mut disallowed_custom: Option<String> = None;
        let session_initial_model = chat_initial_model(is_chat_kind, custom_model_id);
        let build_custom_model_id = if is_chat_kind { None } else { custom_model_id };
        let resolved_custom_model = build_custom_model_id
            .and_then(|custom_model| match self
                .resolve_model_id(&acp::ModelId::new(custom_model))
            {
                Ok(model) if model.info.user_selectable => {
                    model_agent_type = Some(model.info().agent_type.clone());
                    let origin_client = self
                        .origin_client_info_from_meta(arguments.meta.as_ref());
                    session_sampling_override = Some(
                        self.prepare_sampling_config_for_model(&model, origin_client),
                    );
                    Some(custom_model)
                }
                Ok(_) => {
                    tracing::warn!(
                        requested_model = custom_model,
                        "Requested model not allowed by allowed_models; falling back to current default model"
                    );
                    disallowed_custom = Some(custom_model.to_string());
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        requested_model = custom_model, fallback_model = % self
                        .models_manager.current_model_id().0,
                        "Requested model not found, falling back to current default model"
                    );
                    None
                }
            });
        if model_agent_type.is_none() && custom_model_id.is_none()
            && let Ok(default_model) = self
                .resolve_model_id(&self.models_manager.current_model_id())
        {
            model_agent_type = Some(default_model.info().agent_type.clone());
        } else if model_agent_type.is_none() && custom_model_id.is_some() {
            tracing::debug!(
                custom_model = ? custom_model_id, current_model_id = % self
                .models_manager.current_model_id().0,
                "Skipping current_model_id agent_type fallback: custom model was requested, \
                 avoiding cross-client agent_type contamination in leader mode"
            );
        }
        let origin_client = self.origin_client_info_from_meta(arguments.meta.as_ref());
        let mut session_sampling = session_sampling_override
            .unwrap_or_else(|| {
                self
                    .resolve_sampling_config_for_model(
                        &self.models_manager.current_model_id(),
                        origin_client.clone(),
                    )
            });
        if let Some(effort) = self.models_manager.current_reasoning_effort()
            && self
                .models_manager
                .model_supports_reasoning_effort(&session_sampling.model)
        {
            session_sampling.reasoning_effort = Some(effort);
        }
        let (summary_client, summary_model) = self
            .build_summary_client(&session_sampling)?;
        let model_id = match &session_initial_model {
            Some(chat_model) => acp::ModelId::new(chat_model.clone()),
            None => {
                resolved_custom_model
                    .map(acp::ModelId::new)
                    .unwrap_or_else(|| self.models_manager.current_model_id())
            }
        };
        let session_model_id = model_id.clone();
        let persistence = if is_chat_kind {
            crate::session::persistence::PersistenceHandle::noop()
        } else {
            let _timer = crate::instrumentation_timer!("session.persistence_init");
            let registry_title_sync = self
                .session_registry_client()
                .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                    client,
                    suppress_for_zdr: false,
                });
            crate::session::persistence::new(
                    &session_info,
                    model_id,
                    summary_client,
                    self.storage_mode,
                    Some(self.auth_manager.clone()),
                    Some(self.gateway.clone()),
                    summary_model,
                    registry_title_sync,
                )
                .await
                .map_err(|e| crate::session::persistence::io_error_to_acp(&e))?
        };
        self.session_turn_numbers.borrow_mut().insert(session_id.clone(), 0u64);
        let chat_history = vec![];
        let client_code_nav_enabled = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            arguments.meta.as_ref(),
            init,
        );
        let spawn_res = {
            let mut timer = crate::instrumentation_timer!("session.spawn_session_actor");
            timer.with_field("session_id", session_id.0.as_ref());
            let spawn_opts = if is_chat_kind {
                chat_session_spawn_options(
                    session_info.clone(),
                    cwd.clone(),
                    arguments.meta.as_ref(),
                    model_agent_type.as_deref(),
                    session_model_id,
                    session_yolo_mode,
                )
            } else {
                SessionSpawnOptions {
                        session_info: session_info.clone(),
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path: None,
                        initial_total_tokens: 0,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: None,
                        persisted_signals: None,
                        persisted_plan_mode: None,
                        persisted_goal_mode: None,
                        persisted_graph_mode: None,
                        persisted_announcement_state: None,
                        session_meta: arguments.meta.as_ref(),
                        model_agent_type: model_agent_type.as_deref(),
                        session_model_id,
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd: None,
                    }
            };
            self.spawn_and_register_session(init, spawn_opts).await
        };
        spawn_res?;
        tracing::debug!(session_id = % session_id.0, "new_session: spawn_session_actor");
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        let bridge_attach = BridgeAttach::NotAttached;
        if let Some(model_id) = resolved_custom_model {
            let _ = crate::timed!(
                log : "new_session: set_session_model", { crate
                ::agent::handlers::model_switch::apply(self,
                acp::SetSessionModelRequest::new(session_id.clone(),
                acp::ModelId::new(model_id)),). await }
            );
            tracing::debug!(
                session_id = % session_id.0, "new_session: set_session_model"
            );
        }
        if let Some(requested) = disallowed_custom {
            let current = self.models_manager.current_model_id();
            let reason = format!(
                "\"{requested}\" isn't allowed by your allowed_models setting, so this session is using \"{}\".",
                current.0
            );
            self.send_model_auto_switched(
                    &session_id,
                    &acp::ModelId::new(requested),
                    &current,
                    &reason,
                )
                .await;
        }
        let indexed_roots = self.indexed_roots_for(cwd.as_path());
        let (git_root, is_git_repo, discovery_failed) = match kigi_workspace::session::git::discover_git_root(
            cwd.as_path(),
        ) {
            GitDiscoveryResult::Found(root) => {
                let root_str = root.to_string_lossy().trim_end_matches('/').to_string();
                (Some(root_str), true, false)
            }
            GitDiscoveryResult::NotARepo => {
                tracing::debug!("new_session: not a git repository");
                (None, false, false)
            }
            GitDiscoveryResult::DiscoveryFailed(e) => {
                tracing::warn!(
                    error = % e, cwd = % cwd.as_str(),
                    "new_session: git repo discovery failed unexpectedly"
                );
                (None, false, true)
            }
        };
        let (show_non_git_warning, feedback_enabled) = {
            let cfg = self.cfg.borrow();
            let show_non_git_warning = !is_git_repo && !discovery_failed
                && cfg
                    .remote_settings
                    .as_ref()
                    .and_then(|s| s.non_git_warning)
                    .unwrap_or(cfg.features.non_git_warning);
            let feedback_enabled = cfg.is_feedback_enabled();
            (show_non_git_warning, feedback_enabled)
        };
        kigi_log::unified_log::info(
            "session created",
            Some(session_id.0.as_ref()),
            Some(serde_json::json!({ "cwd" : cwd.as_str() })),
        );
        let models = if is_chat_kind {
            // A chat-kind session has no managed catalog to offer.
            chat_new_session_model_state(
                acp::SessionModelState::new(acp::ModelId::from(String::new()), Vec::new()),
                session_initial_model
                    .filter(|_| matches!(bridge_attach, BridgeAttach::Spawned)),
            )
        } else {
            self.model_state(Some(&session_id))
        };
        let (session_config_value, session_detail_value) = self
            .session_config_meta(&session_id, cwd.as_str().to_owned(), None, &models);
        let mut meta = serde_json::json!(
            { "currentWorkingDirectory" : cwd.as_str().to_owned(), "codebaseIndexed" :
            indexed_roots, "isGitRepo" : is_git_repo, "gitRoot" : git_root,
            "showNonGitWarning" : show_non_git_warning, "feedbackEnabled" :
            feedback_enabled, }
        );
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("kigi/sessionConfig".to_string(), session_config_value);
            obj.insert("kigi/sessionDetail".to_string(), session_detail_value);
        }
        Ok(
            acp::NewSessionResponse::new(session_id)
                .models(Some(models))
                .meta(meta.as_object().cloned()),
        )
    }
    async fn load_session(
        &self,
        arguments: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let _load_guard = self.begin_session_load(&arguments.session_id);
        self.sweep_dead_sessions();
        self.drain_old_session_thread(&arguments.session_id).await;
        tracing::debug!("Received load session request {arguments:?}");
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before load_session")
            })?;
        self.seed_client_config_auth_if_available();
        let persist_data = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("kigi/persist"))
            .cloned();
        let target_client_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("kigi/leaderClientId"))
            .cloned();
        let acp::LoadSessionRequest {
            session_id,
            cwd,
            mcp_servers: client_mcp_servers,
            meta: request_meta,
            ..
        } = arguments;
        let cwd = AbsPathBuf::new(cwd)
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let initial_client_mcp_servers = client_mcp_servers.clone();
        let mcp_servers = self.resolve_mcp_servers(client_mcp_servers, cwd.as_path());
        let mcp_meta_config_map = parse_mcp_meta_config(request_meta.as_ref());
        let mut load_timer = crate::instrumentation_timer!("session.load_session");
        load_timer.with_field("session_id", session_id.0.as_ref());
        load_timer.with_field("cwd", cwd.as_str());
        let git_root = kigi_workspace::session::git::find_git_root_from_path(
                cwd.as_path(),
            )
            .ok();
        if let Some(root) = git_root {
            tokio::task::spawn_blocking(move || {
                crate::session::worktree_pool::cleanup_stale_pool_worktrees(Some(&root));
            });
        }
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let current_session_dir = crate::session::persistence::session_dir(
            &session_info,
        );
        tokio::task::spawn_blocking(move || {
            crate::session::persistence::cleanup_stale_sessions(
                Some(&current_session_dir),
            );
        });
        let session_exists = self.sessions.borrow().contains_key(&session_id);
        if session_exists {
            tracing::info!(
                session_id = % session_id.0,
                "Reconnect detected: flushing persistence buffer before replay"
            );
            if let Some(handle) = self.sessions.borrow().get(&session_id) {
                handle
                    .gateway_enabled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            let mut flush_timer = crate::instrumentation_timer!(
                "session.reconnect_flush"
            );
            flush_timer.with_field("session_id", session_id.0.as_ref());
            if let Err(reason) = self.flush_session(&session_id).await {
                tracing::warn!(
                    session_id = % session_id.0, reason, "Reconnect flush failed"
                );
            }
            drop(flush_timer);
        }
        let origin_client = self.origin_client_info_from_meta(request_meta.as_ref());
        let load_session_sampling = self
            .resolve_sampling_config_for_model(
                &self.models_manager.current_model_id(),
                origin_client.clone(),
            );
        let (summary_client, summary_model) = self
            .build_summary_client(&load_session_sampling)?;
        let mut persistence_timer = crate::instrumentation_timer!("session.load_light");
        persistence_timer.with_field("session_id", session_id.0.as_ref());
        let registry_title_sync = self
            .session_registry_client()
            .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                client,
                suppress_for_zdr: false,
            });
        let (persistence_info, persistence) = crate::session::persistence::load_light(
                &session_info,
                summary_client,
                Some(self.gateway.clone()),
                summary_model,
                registry_title_sync,
            )
            .await
            .map_err(|e| crate::session::persistence::io_error_to_acp(&e))?;
        drop(persistence_timer);
        let crate::session::persistence::PersistedInfoLight {
            summary,
            chat_history,
            plan_state: _,
            plan_mode_state: persisted_plan_mode,
            updates_file_path,
            rewind_points_file_path,
            signals: persisted_signals,
            announcement_state: persisted_announcement_state,
            goal_mode_state: _persisted_goal_mode,
            graph_mode_state: _persisted_graph_mode,
        } = persistence_info;
        let restored_awaiting_plan_approval = persisted_plan_mode
            .as_ref()
            .is_some_and(|s| s.awaiting_plan_approval);
        self.session_turn_numbers
            .borrow_mut()
            .insert(session_id.clone(), summary.next_trace_turn);
        tracing::info!(
            session_id = % session_id.0, next_trace_turn = summary.next_trace_turn,
            "Loaded session telemetry turn counter from persistence"
        );
        let no_replay = parse_no_replay(request_meta.as_ref());
        let cursor = request_meta
            .as_ref()
            .and_then(|m| m.get("cursor"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let session_yolo_mode = request_meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            request_meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        #[allow(unused_variables)]
        let session_computer_sessions = parse_session_computer_sessions(
            request_meta.as_ref(),
        );
        let restore_code_requested = request_meta
            .as_ref()
            .and_then(|m| m.get("kigi/restore_code"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.restore_code);
        let registry_client_for_restore = self.session_registry_client();
        if restore_code_requested && registry_client_for_restore.is_none() {
            kigi_workspace::session::git::warn_registry_disabled_restore(
                session_id.0.as_ref(),
            );
        }
        let restore_checkout_allowed = kigi_workspace::session::git::restore_code_checkout_allowed(
            cwd.as_path(),
            Some(summary.info.cwd.as_str()),
        );
        if restore_code_requested && !restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            tracing::warn!(
                target : kigi_workspace::session::git::RESTORE_CODE_LOG, session_id =
                % session_id.0, supplied_cwd = % cwd.as_str(), persisted_cwd = % summary
                .info.cwd, target_sha = % target_sha,
                "restore_code: skipping session HEAD checkout — supplied cwd is neither a kigi worktree nor the session's persisted cwd (refusing to detach the source repo)"
            );
            kigi_log::unified_log::warn(
                "restore_code: skipped session HEAD checkout (unsafe cwd)",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!(
                        { "supplied_cwd" : cwd.as_str(), "persisted_cwd" : summary.info
                        .cwd, "target_sha" : target_sha, }
                    ),
                ),
            );
        }
        let mut code_restore_info: Option<serde_json::Value> = None;
        if restore_code_requested && restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            use kigi_workspace::session::git::RestoreKind;
            let outcome = kigi_workspace::session::git::checkout_session_commit(
                    cwd.as_path(),
                    target_sha,
                    true,
                    session_id.0.as_ref(),
                )
                .await;
            let kind = if !outcome.checked_out {
                RestoreKind::CheckoutFailed
            } else {
                match registry_client_for_restore {
                        None => RestoreKind::RegistryOff,
                        Some(registry_client) => {
                            let _ = registry_client;
                            RestoreKind::RegistryOff
                        }
                    }
            };
            code_restore_info = crate::agent::restore_code::build_code_restore_meta(
                target_sha,
                &outcome,
                kind,
            );
        }
        let load_envrc = {
            let skip_envrc = request_meta
                .as_ref()
                .and_then(|m| m.get("kigi/skip_envrc"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if skip_envrc {
                false
            } else {
                self.cfg.borrow().session.load_envrc.unwrap_or(true)
            }
        };
        let (initial_total_tokens, delta_completions, unfinished_subagents) = if no_replay {
            tracing::info!(
                session_id = % session_id.0,
                "Skipping session replay (noReplay flag set by relay)"
            );
            (
                Self::extract_initial_tokens_from_updates(&updates_file_path),
                Vec::new(),
                Vec::new(),
            )
        } else {
            let (tokens, replay_end_offset, unfinished_subagents) = self
                .replay_session_updates(
                    &session_id,
                    &cwd,
                    &updates_file_path,
                    persist_data.as_ref(),
                    target_client_id.as_ref(),
                    cursor.as_deref(),
                )
                .await?;
            let cursor_mark_replay = cursor.is_none();
            let _timer = crate::instrumentation_timer!("session.delta_flush_replay");
            let completions = match self.flush_session(&session_id).await {
                Ok(()) => {
                    self.replay_session_updates_from_offset_enqueue(
                        &session_id,
                        &updates_file_path,
                        replay_end_offset,
                        persist_data.as_ref(),
                        target_client_id.as_ref(),
                        cursor_mark_replay,
                    )
                }
                Err(reason) => {
                    tracing::warn!(
                        session_id = % session_id.0, reason,
                        "Post-replay flush failed, skipping delta replay"
                    );
                    Vec::new()
                }
            };
            (tokens, completions, unfinished_subagents)
        };
        if let Some(handle) = self.sessions.borrow().get(&session_id) {
            handle.gateway_enabled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for rx in delta_completions {
            let _ = rx.await;
        }
        let reconcile_completions = {
            let _timer = crate::instrumentation_timer!("session.reconcile_stale_tasks");
            self.reconcile_stale_background_tasks(&session_id, &updates_file_path)
        };
        for rx in reconcile_completions {
            let _ = rx.await;
        }
        let preloaded_envrc = kigi_workspace::envrc::load_envrc_or_empty_when_trusted(
            cwd.as_path(),
            load_envrc && folder_trust::project_scope_allowed(cwd.as_path()),
        );
        let client_code_nav_enabled = request_meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            request_meta.as_ref(),
            init,
        );
        let prompt_display_cwd = request_meta
            .as_ref()
            .and_then(|m| m.get("kigi/display_cwd"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| summary.prompt_display_cwd.clone());
        if self.sessions.borrow().get(&session_id).is_none() {
            tracing::info!(
                session_id = % session_id.0,
                "load_session: spawning new session actor (session not in memory)"
            );
            let mut spawn_timer = crate::instrumentation_timer!(
                "session.spawn_and_register_session"
            );
            spawn_timer.with_field("session_id", session_id.0.as_ref());
            let persisted_agent_name: Option<String> = summary
                .agent_name
                .clone()
                .or_else(|| {
                    self
                        .resolve_model_id(&summary.current_model_id)
                        .ok()
                        .map(|m| m.info().agent_type.clone())
                });
            self.spawn_and_register_session(
                    init,
                    SessionSpawnOptions {
                        session_info,
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path,
                        initial_total_tokens,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: Some(preloaded_envrc),
                        persisted_signals,
                        persisted_plan_mode,
                        persisted_goal_mode: _persisted_goal_mode,
                        persisted_graph_mode: _persisted_graph_mode,
                        persisted_announcement_state,
                        session_meta: request_meta.as_ref(),
                        model_agent_type: persisted_agent_name.as_deref(),
                        session_model_id: summary.current_model_id.clone(),
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd,
                    },
                )
                .await?;
            drop(spawn_timer);
        } else if !mcp_servers.is_empty() {
            tracing::info!(
                session_id = % session_id.0, mcp_server_count = mcp_servers.len(),
                "load_session: reconnecting to existing session, updating MCP servers"
            );
            if let Some(handle) = self.sessions.borrow_mut().get_mut(&session_id) {
                handle.initial_client_mcp_servers = initial_client_mcp_servers;
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(crate::session::SessionCommand::UpdateMcpServers {
                        mcp_servers,
                        respond_to: tx,
                    });
            }
        } else {
            tracing::info!(
                session_id = % session_id.0,
                "load_session: reconnecting to existing session (feedback manager already initialized)"
            );
        }
        {
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|init| init.meta.as_ref());
            if let Some(handle) = self.sessions.borrow().get(&session_id) {
                enqueue_replace_system_prompt_override(
                    &handle.cmd_tx,
                    request_meta.as_ref(),
                    init_meta,
                );
            }
        }
        if session_exists
            && let Some(hooks) = crate::extensions::hooks::reconnect_client_hooks(
                request_meta.as_ref(),
            ) && let Some(handle) = self.sessions.borrow().get(&session_id)
        {
            handle.set_client_hooks(hooks);
        }
        #[allow(unused_variables)]
        let local_transcript_rendered = !no_replay
            && updates_file_path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.len() > 0);
        if let Some(handle) = self.sessions.borrow_mut().get_mut(&session_id) {
            handle.code_nav_enabled = client_code_nav_enabled;
            if session_yolo_mode && !handle.yolo_mode {
                tracing::debug!(
                    session_id = % session_id.0,
                    "Setting YOLO mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = true;
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SetYoloMode {
                        enabled: true,
                    });
            }
            if session_auto_mode && !session_yolo_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                tracing::debug!(
                    session_id = % session_id.0,
                    "Setting auto mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = false;
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SetAutoMode {
                        enabled: true,
                    });
            }
        }
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        if let Some((parent_cmd_tx, session_cwd)) = self
            .sessions
            .borrow()
            .get(&session_id)
            .map(|h| (h.cmd_tx.clone(), h.info.cwd.clone()))
        {
            let session_dir = crate::session::persistence::session_dir(
                &SessionInfo {
                    id: session_id.clone(),
                    cwd: session_cwd,
                },
            );
            crate::agent::subagent::reconcile_orphaned_subagents(
                &unfinished_subagents,
                &self.subagent_coordinator.borrow(),
                &session_dir,
                session_id.0.as_ref(),
                &self.gateway,
                Some(&parent_cmd_tx),
            );
        }
        let persisted_model = summary.current_model_id.clone();
        let models = self.models_manager.models();
        let available = self.models_manager.available();
        self.model_unavailable_sessions.borrow_mut().remove(session_id.0.as_ref());
        let resolved_catalog_key = resolve_catalog_key(&models, &persisted_model);
        tracing::debug!(
            session_id = % session_id.0, persisted = % persisted_model.0,
            resolved_catalog_key = ? resolved_catalog_key.as_ref().map(| k | k.0
            .as_ref()), available_count = available.len(), contains_persisted = available
            .contains_key(& persisted_model), available_keys = ? available.keys()
            .take(10).collect::< Vec < _ >> (),
            "load_session: restoring persisted model (debug)"
        );
        // "Same family" = same platform: catalog keys are
        // `{platform_id}/{model_id}` (PRD F4), so prefer a replacement from
        // the platform the persisted model belonged to (its credentials are
        // known-good) before falling back across platforms.
        let persisted_platform =
            kigi_models::parse_managed_model_key(persisted_model.0.as_ref()).map(|(p, _)| p);
        let same_family_fallback = available
            .keys()
            .find(|id| {
                kigi_models::parse_managed_model_key(id.0.as_ref()).map(|(p, _)| p)
                    == persisted_platform
            })
            .cloned()
            .or_else(|| available.keys().next().cloned());
        let selectable_catalog_key = selectable_catalog_key_for_persisted(
            &models,
            &available,
            &persisted_model,
        );
        let model_id = if let Some(catalog_key) = selectable_catalog_key {
            if catalog_key != persisted_model {
                tracing::info!(
                    session_id = % session_id.0, persisted = % persisted_model.0,
                    catalog_key = % catalog_key.0,
                    "load_session: mapped persisted routing slug to catalog key"
                );
                kigi_log::unified_log::info(
                    "load_session: mapped persisted routing slug to catalog key",
                    Some(session_id.0.as_ref()),
                    Some(
                        serde_json::json!(
                            { "persisted_model" : persisted_model.0.as_ref(),
                            "catalog_key" : catalog_key.0.as_ref(), }
                        ),
                    ),
                );
            }
            catalog_key
        } else if available.is_empty() {
            tracing::warn!(
                session_id = % session_id.0, persisted = % persisted_model.0,
                "load_session: model catalog empty at load; keeping persisted model unverified (catalog fetch may still be in flight)"
            );
            kigi_log::unified_log::warn(
                "load_session: model catalog empty, keeping persisted model unverified",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!(
                        { "persisted_model" : persisted_model.0.as_ref(), }
                    ),
                ),
            );
            persisted_model
        } else if let Some(fallback) = same_family_fallback {
            tracing::warn!(
                session_id = % session_id.0, previous = % persisted_model.0, new = %
                fallback.0,
                "Persisted model no longer available, auto-switching within family"
            );
            let reason = format!(
                "Model \"{}\" is no longer available for your account.", persisted_model
                .0,
            );
            self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &fallback,
                    &reason,
                )
                .await;
            fallback
        } else {
            let fallback = available
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| persisted_model.clone());
            tracing::warn!(
                session_id = % session_id.0, previous = % persisted_model.0, fallback = %
                fallback.0, available_count = available.len(), available_keys = ?
                available.keys().take(10).collect::< Vec < _ >> (),
                "Persisted model no longer available, no same-family fallback — blocking prompts for this session"
            );
            kigi_log::unified_log::warn(
                "load_session: persisted model unavailable, no same-family fallback",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!(
                        { "persisted_model" : persisted_model.0.as_ref(),
                        "fallback_model" : fallback.0.as_ref(), "available_count" :
                        available.len(), }
                    ),
                ),
            );
            let reason = format!(
                "Model \"{}\" is no longer available. Please start a new session.",
                persisted_model.0,
            );
            let empty_id = acp::ModelId::new(String::new());
            self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &empty_id,
                    &reason,
                )
                .await;
            self.model_unavailable_sessions
                .borrow_mut()
                .insert(session_id.0.to_string(), persisted_model.clone());
            fallback
        };
        tracing::debug!(
            session_id = % session_id.0, final_model_id = % model_id.0,
            "load_session: resolved final model_id for set_session_model"
        );
        {
            let _timer = crate::instrumentation_timer!("session.restore_model");
            let restore_meta = summary
                .reasoning_effort
                .map(|effort| {
                    let mut map = acp::Meta::new();
                    map.insert(
                        REASONING_EFFORT_META_KEY.to_string(),
                        reasoning_effort_meta_value(effort),
                    );
                    map
                });
            let _ = crate::agent::handlers::model_switch::apply(
                    self,
                    acp::SetSessionModelRequest::new(session_id.to_owned(), model_id)
                        .meta(restore_meta),
                )
                .await;
        }
        let mut response_meta_map = serde_json::Map::new();
        response_meta_map.insert("sessionId".to_string(), serde_json::json!(session_id));
        if let Some(persist) = persist_data {
            response_meta_map.insert("kigi/persist".to_string(), persist);
        }
        let session_cwd = self
            .sessions
            .borrow()
            .get(&session_id)
            .map(|h| h.info.cwd.clone());
        let indexed_roots = session_cwd
            .as_deref()
            .map(|c| self.indexed_roots_for(std::path::Path::new(c)))
            .unwrap_or_default();
        response_meta_map
            .insert("codebaseIndexed".to_string(), serde_json::json!(indexed_roots));
        if summary.head_commit.is_some() && let Some(ref cwd) = session_cwd
            && summary
                .git_root_dir
                .as_deref()
                .is_none_or(|root| {
                    kigi_workspace::session::git::find_git_root_from_path(
                            std::path::Path::new(cwd.as_str()),
                        )
                        .ok()
                        .is_some_and(|current_root| {
                            current_root == std::path::Path::new(root)
                        })
                })
        {
            let _timer = crate::instrumentation_timer!("session.git_divergence");
            let cwd_path = std::path::Path::new(cwd.as_str());
            let current_head = kigi_workspace::session::git::git_cli(
                    cwd_path,
                    &["rev-parse", "HEAD"],
                )
                .await
                .ok();
            if let Some(divergence) = kigi_workspace::session::git::detect_head_divergence(
                summary.head_commit.as_deref(),
                summary.head_branch.as_deref(),
                current_head.as_deref(),
            ) {
                response_meta_map
                    .insert("gitDivergence".to_string(), serde_json::json!(divergence));
            }
        }
        if let Some(info) = code_restore_info {
            response_meta_map.insert("codeRestore".to_string(), info);
        }
        if let Some(running_prompt_id) = self
            .sessions
            .borrow()
            .get(&session_id)
            .and_then(|h| h.current_prompt_id.lock().ok().and_then(|g| g.clone()))
        {
            response_meta_map
                .insert(
                    "kigi/runningPromptId".to_string(),
                    serde_json::json!(running_prompt_id),
                );
        }
        let model_state = self.model_state(Some(&session_id));
        let (session_config_value, session_detail_value) = self
            .session_config_meta(
                &session_id,
                session_cwd.clone().unwrap_or_default(),
                summary.display_title_opt(),
                &model_state,
            );
        response_meta_map.insert("kigi/sessionConfig".to_string(), session_config_value);
        response_meta_map.insert("kigi/sessionDetail".to_string(), session_detail_value);
        let response_meta = serde_json::Value::Object(response_meta_map);
        kigi_log::unified_log::info(
            "session loaded",
            Some(session_id.0.as_ref()),
            None,
        );
        let response = acp::LoadSessionResponse::new()
            .models(Some(model_state))
            .meta(response_meta.as_object().cloned());
        if let Some(handle) = self.sessions.borrow().get(&session_id) {
            let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
            if restored_awaiting_plan_approval {
                let _ = handle.cmd_tx.send(SessionCommand::RestorePlanApproval);
            }
        }
        Ok(response)
    }
    #[tracing::instrument(
        name = "agent.prompt",
        skip_all,
        fields(
            session_id = %arguments.session_id.0,
            turn_number = tracing::field::Empty,
        )
    )]
    #[allow(unused_mut)]
    async fn prompt(
        &self,
        mut arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        use crate::session::plan_mode::PromptMode;
        if let Some(meta) = arguments.meta.as_ref() {
            kigi_file_utils::trace_context::link_current_span_to_meta(
                &serde_json::Value::Object(meta.clone()),
            );
        }
        tracing::debug!(
            target : "sampling_log", session_id = % arguments.session_id.0,
            "Received prompt request"
        );
        kigi_log::unified_log::info(
            "prompt received",
            Some(arguments.session_id.0.as_ref()),
            None,
        );
        let handle = self
            .session_handle_waiting_for_load(&arguments.session_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
        if self.models_manager.allowlist_excludes_all() {
            self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    "None of your models are allowed by allowed_models. \
                 Broaden it or remove it from your config, then restart.",
                )
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        let latched_model = self
            .model_unavailable_sessions
            .borrow()
            .get(arguments.session_id.0.as_ref())
            .cloned();
        if let Some(unavailable_model) = latched_model {
            let models = self.models_manager.models();
            let available = self.models_manager.available();
            let restore_model_id = selectable_catalog_key_for_persisted(
                    &models,
                    &available,
                    &unavailable_model,
                )
                .unwrap_or(unavailable_model.clone());
            if available.contains_key(&restore_model_id) {
                tracing::info!(
                    session_id = % arguments.session_id.0, model_id = % restore_model_id
                    .0,
                    "prompt: previously-unavailable model is back in the catalog; restoring it and unblocking the session"
                );
                kigi_log::unified_log::info(
                    "prompt: previously-unavailable model recovered, unblocking session",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!({ "model_id" : restore_model_id.0.as_ref(), }),
                    ),
                );
                self.model_unavailable_sessions
                    .borrow_mut()
                    .remove(arguments.session_id.0.as_ref());
                if let Err(e) = crate::agent::handlers::model_switch::apply(
                        self,
                        acp::SetSessionModelRequest::new(
                            arguments.session_id.clone(),
                            restore_model_id.clone(),
                        ),
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = % arguments.session_id.0, model_id = %
                        restore_model_id.0, error = ? e,
                        "prompt: failed to restore previously-unavailable model; continuing with the session's current model"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = % arguments.session_id.0, unavailable_model = %
                    unavailable_model.0, available_count = available.len(),
                    available_keys = ? available.keys().take(10).collect::< Vec < _ >>
                    (),
                    "prompt blocked: session model unavailable since load and still missing from the catalog"
                );
                kigi_log::unified_log::warn(
                    "prompt blocked: model unavailable",
                    Some(arguments.session_id.0.as_ref()),
                    Some(
                        serde_json::json!(
                            { "unavailable_model" : unavailable_model.0.as_ref(),
                            "available_count" : available.len(), }
                        ),
                    ),
                );
                self.send_model_auto_switched(
                        &arguments.session_id,
                        &acp::ModelId::new(String::new()),
                        &acp::ModelId::new(String::new()),
                        "Your previous model is no longer available and could not \
                     be switched to a compatible model. Please start a new session.",
                    )
                    .await;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
        }
        let intake_lock = self.prompt_intake_lock(&arguments.session_id);
        let intake_guard = intake_lock.lock().await;
        let meta_prompt_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mode"))
            .and_then(|v| v.as_str())
            .map(PromptMode::from_meta_str);
        let prompt_mode = if let Some(mode) = meta_prompt_mode {
            mode
        } else {
            let (mode_tx, mode_rx) = oneshot::channel();
            let _ = handle
                .cmd_tx
                .send(crate::session::SessionCommand::GetCurrentPromptMode {
                    responds_to: mode_tx,
                });
            mode_rx.await.unwrap_or_default()
        };
        let turn_started_at = chrono::Utc::now().to_rfc3339();
        let prompt_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("promptId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let turn_number = self.allocate_turn_number(&arguments.session_id);
        tracing::Span::current().record("turn_number", turn_number);
        let (model_tx, model_rx) = oneshot::channel();
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::GetCurrentModel {
                responds_to: model_tx,
            });
        let model = model_rx
            .await
            .unwrap_or_else(|_| self.sampling_config.borrow().model.clone());
        let verbatim = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("verbatim"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let send_now = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sendNow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let next_trace_turn = self
            .session_turn_numbers
            .borrow()
            .get(&arguments.session_id)
            .copied()
            .unwrap_or_else(|| turn_number.saturating_add(1));
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: Some(prompt_id.clone()),
            });
        let (tx, rx) = oneshot::channel();
        let prompt_client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let prompt_screen_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("screenMode"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let json_schema = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("outputSchema"))
            .cloned();
        if json_schema.as_ref().is_some_and(|schema| !schema.is_object()) {
            return Err(
                acp::Error::invalid_params()
                    .data("outputSchema must be a JSON object describing a JSON Schema"),
            );
        }
        handle
            .cmd_tx
            .send(SessionCommand::Prompt {
                prompt_id: prompt_id.clone(),
                prompt_blocks: arguments.prompt.clone(),
                prompt_mode,
                client_identifier: prompt_client_identifier,
                screen_mode: prompt_screen_mode,
                verbatim,
                traceparent: kigi_file_utils::trace_context::current_traceparent(),
                json_schema,
                send_now,
                respond_to: tx,
                persist_ack: None,
            })
            .map_err(|e| {
                acp::Error::internal_error()
                    .data(format!("failed to dispatch prompt to session: {e}"))
            })?;
        drop(intake_guard);
        self.push_roster_activity_delta(
            &arguments.session_id,
            crate::agent::roster::RosterActivity::Working,
        );
        let stop_result = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("session failed to respond")
            })?;
        let last_turn_usage_for_meta = handle
            .chat_state_handle
            .get_last_turn_usage()
            .await;
        if matches!(
            stop_result, Ok(crate ::session::commands::PromptTurnOk { completion_kind :
            crate ::session::commands::PromptCompletionKind::RemovedFromQueue, .. })
        ) {
            return Ok(
                acp::PromptResponse::new(acp::StopReason::Cancelled)
                    .meta(
                        build_prompt_response_meta(PromptResponseMetaArgs {
                                session_id: &arguments.session_id.to_string(),
                                prompt_id: &prompt_id,
                                total_tokens: 0,
                                model_id: &model,
                                last_turn_usage: None,
                                prompt_usage: None,
                                cancellation_category: None,
                                cancel_trigger: None,
                                structured_output: None,
                            })
                            .as_object()
                            .cloned(),
                    ),
            );
        }
        let cancel_trigger: Option<String> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| match &ok.completion_kind {
                crate::session::commands::PromptCompletionKind::Cancelled {
                    context: Some(ctx),
                    ..
                } => ctx.trigger.clone(),
                _ => None,
            });
        {
            let mapped = stop_result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let (stop_reason_value, agent_result_value) = crate::sampling::error::prompt_complete_fields(
                &mapped,
            );
            let turn_id = arguments
                .meta
                .as_ref()
                .and_then(|m| m.get("turnId"))
                .and_then(|v| v.as_u64());
            let mut payload = serde_json::json!(
                { "sessionId" : arguments.session_id.to_string(), "promptId" : prompt_id
                .as_str(), "stopReason" : stop_reason_value, "agentResult" :
                agent_result_value, }
            );
            if let Some(tid) = turn_id {
                payload["turnId"] = serde_json::json!(tid);
            }
            if let Some(ref t) = cancel_trigger {
                payload["cancelTrigger"] = serde_json::json!(t);
            }
            let params = serde_json::value::to_raw_value(&payload)
                .expect("prompt_complete params serialization");
            self.gateway
                .forward_fire_and_forget(
                    acp::ExtNotification::new(
                        "kigi/session/prompt_complete",
                        params.into(),
                    ),
                );
        }
        {
            let end_activity = if handle
                .pending_interactions
                .lock()
                .map(|g| !g.is_empty())
                .unwrap_or(false)
            {
                crate::agent::roster::RosterActivity::NeedsInput
            } else {
                crate::agent::roster::RosterActivity::Idle
            };
            self.push_roster_activity_delta(&arguments.session_id, end_activity);
        }
        match stop_result {
            Ok(turn_ok) => {
                let crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot,
                    completion_kind,
                    structured_output,
                    usage: prompt_usage,
                } = turn_ok;
                let last_turn_usage = last_turn_usage_for_meta;
                let cancellation_category = match &completion_kind {
                    crate::session::commands::PromptCompletionKind::Cancelled {
                        category: Some(cat),
                        ..
                    } => Some(format!("{cat:?}")),
                    crate::session::commands::PromptCompletionKind::MaxTurnsReached {
                        ..
                    } => Some("max_turns_reached".to_string()),
                    _ => None,
                };
                Ok(
                    acp::PromptResponse::new(stop_reason)
                        .meta(
                            build_prompt_response_meta(PromptResponseMetaArgs {
                                    session_id: &arguments.session_id.to_string(),
                                    prompt_id: &prompt_id,
                                    total_tokens,
                                    model_id: &model,
                                    last_turn_usage: last_turn_usage.as_ref(),
                                    prompt_usage,
                                    cancellation_category,
                                    cancel_trigger,
                                    structured_output,
                                })
                                .as_object()
                                .cloned(),
                        ),
                )
            }
            Err(err) => {
                let err = if crate::sampling::error::prompt_usage_from_error(&err)
                    .is_some()
                {
                    err
                } else {
                    let prompt_id = handle
                        .current_prompt_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let usage = if handle
                        .cmd_tx
                        .send(crate::session::commands::SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    };
                    crate::sampling::error::attach_prompt_usage(err, usage)
                };
                Err(err)
            }
        }
    }
    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        tracing::info!("Received cancel request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let cancel_trigger = args
            .meta
            .as_ref()
            .and_then(|m| m.get("cancelTrigger"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        kigi_log::unified_log::info(
            "shell.cancel.received",
            Some(args.session_id.0.as_ref()),
            Some(
                serde_json::json!(
                    { "session_found" : handle.is_some(), "trigger" : cancel_trigger, }
                ),
            ),
        );
        if let Some(handle) = handle {
            let cancel_subagents = args
                .meta
                .as_ref()
                .and_then(|m| m.get("cancelSubagents"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let rewind_if_pristine = args
                .meta
                .as_ref()
                .and_then(|m| m.get("rewindIfPristine"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let _ = handle
                .cmd_tx
                .send(SessionCommand::Cancel {
                    cancel_subagents,
                    kill_background_tasks: false,
                    rewind_if_pristine,
                    trigger: cancel_trigger,
                });
        }
        Ok(())
    }
    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        tracing::info!("Received set session mode request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let (tx, rx) = oneshot::channel();
        if let Some(handle) = handle {
            let _ = handle
                .cmd_tx
                .send(SessionCommand::SessionMode {
                    session_mode: args.mode_id,
                    responds_to: tx,
                });
        }
        let _ = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("response to set session failed")
            })?;
        Ok(acp::SetSessionModeResponse::new())
    }
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let model = self.resolve_model_id(&args.model_id)?;
        if !model.info.user_selectable {
            return Err(
                acp::Error::invalid_params()
                    .data("This model isn't allowed by your allowed_models setting."),
            );
        }
        let session_id = args.session_id.clone();
        let res = crate::agent::handlers::model_switch::apply(self, args).await;
        if res.is_ok()
            && let Some(unavailable) = self
                .model_unavailable_sessions
                .borrow_mut()
                .remove(session_id.0.as_ref())
        {
            tracing::info!(
                session_id = % session_id.0, previously_unavailable_model = % unavailable
                .0,
                "set_session_model: user model switch cleared the model-unavailable block"
            );
        }
        res
    }
    #[tracing::instrument(
        name = "agent.ext_method",
        skip_all,
        fields(method = %args.method)
    )]
    async fn ext_method(
        &self,
        args: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        let request_meta = serde_json::from_str::<serde_json::Value>(args.params.get())
            .ok()
            .and_then(|v| v.get("_meta").cloned());
        if let Some(meta) = &request_meta {
            kigi_file_utils::trace_context::link_current_span_to_meta(meta);
        }
        tracing::info!("Received extension method call: method={}", args.method);
        #[allow(unused_mut)]
        let mut backend_no_bridge_err: Option<acp::Error> = None;
        let method = args.method.clone();
        let result = match method.as_ref() {
            "kigi/getApiKey" | "kigi/setApiKey" => {
                crate::extensions::auth::handle(self, &args).await
            }
            "kigi/session/info" | "kigi/session/close" | "kigi/session/list"
            | "kigi/sessions/list" => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            "kigi/session/updates" => {
                crate::extensions::session_updates::handle(&args, &self.gateway).await
            }
            "kigi/session/load_history" => {
                crate::extensions::chat_conversation_history::handle(self, &args).await
            }
            "kigi/session/search" => {
                crate::extensions::session_search::handle(&args).await
            }
            "kigi/session/resolve_local_for_worktree_resume"
            | "kigi/session/rehydrate" => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            "kigi/session/rename" | "kigi/session/delete"
            | "kigi/session/update_mcp_servers" | "kigi/session/fork"
            | "kigi/internal/reload_all_mcp_servers"
            | "kigi/internal/reload_project_mcp_servers" | "kigi/internal/reload_skills"
            | "kigi/internal/reload_models" | "kigi/internal/reload_models_cache"
            | "kigi/plugins/reload"
            | "kigi/commands/list" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "kigi/session/repair" => crate::extensions::repair::handle(self, &args).await,
            "kigi/billing" => crate::extensions::billing::handle(self, &args).await,
            "kigi/memory/flush" | "kigi/memory/rewrite" => {
                crate::extensions::memory::handle(self, &args).await
            }
            "kigi/skills/refresh-baseline" => {
                self.refresh_skill_baseline_for_all_sessions();
                crate::extensions::to_ext_response(
                    Ok(serde_json::json!({ "ok" : true })),
                )
            }
            "kigi/interject" => crate::extensions::interject::handle(self, &args).await,
            "kigi/feedback" | "kigi/feedback/dismiss" | "kigi/btw" => {
                crate::extensions::feedback::handle(self, &args).await
            }
            "kigi/recap" => crate::extensions::recap::handle(self, &args).await,
            "kigi/rollout/survey" => {
                crate::extensions::rollout::handle(self, &args).await
            }
            "kigi/prompt_history" => {
                crate::extensions::prompt_history::handle(self, &args).await
            }
            "kigi/suggest" => crate::extensions::suggest::handle(self, &args).await,
            "kigi/suggestPrompt" => crate::extensions::suggest::handle(self, &args).await,
            s if s.starts_with("kigi/auth/") => {
                crate::extensions::auth::handle(self, &args).await
            }
            s if s.starts_with("kigi/session_summaries/") => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            s if s.starts_with("kigi/git/worktree/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            s if s.starts_with("kigi/git/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::git::handle(self, &ops, &args).await
            }
            s if s.starts_with("kigi/compact_conversation") => {
                crate::extensions::memory::handle(self, &args).await
            }
            s if s.starts_with("kigi/plugins/") => {
                crate::extensions::plugins::handle(self, &args).await
            }
            s if s.starts_with("kigi/hooks/") => {
                crate::extensions::hooks::handle(self, &args).await
            }
            s if s.starts_with("kigi/hunk-tracker/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::hunk_tracker::handle(self, &ops, &args).await
            }
            s if s.starts_with("kigi/pr/") => {
                crate::extensions::pr::handle(self, &args).await
            }
            s if s.starts_with(crate::extensions::mcp::mcp_methods::PREFIX) => {
                crate::extensions::mcp::handle(self, &args).await
            }
            s if s.starts_with("kigi/task/") => {
                crate::extensions::task::handle(self, &args).await
            }
            s if s.starts_with("kigi/scheduler/") => {
                crate::extensions::task::handle_scheduler(self, &args).await
            }
            s if s.starts_with("kigi/subagent/") => {
                crate::extensions::task::handle_subagent(self, &args).await
            }
            s if s.starts_with("kigi/terminal/") => {
                crate::extensions::terminal::handle(self, &args).await
            }
            s if crate::extensions::fs::is_fs_method(s) => {
                crate::extensions::fs::handle(self, &args).await
            }
            s if s.starts_with("kigi/search/") => {
                crate::extensions::search::handle(self, &args).await
            }
            s if s.starts_with("kigi/code/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::code_nav::handle(self, &ops, &args).await
            }
            s if s.starts_with("kigi/skills/") => {
                let compat = self.cfg.borrow().compat_resolved;
                crate::extensions::skills::handle(
                        &args,
                        self.plugin_registry_handle.snapshot().as_deref(),
                        compat,
                    )
                    .await
            }
            s if s.starts_with("kigi/review") => {
                crate::extensions::feedback::handle(self, &args).await
            }
            s if s.starts_with("kigi/debug/") => {
                crate::extensions::debug::handle(self, &args).await
            }
            s if s.starts_with("kigi/rewind") => {
                crate::extensions::rewind::handle(self, &args).await
            }
            other => {
                Err(
                    acp::Error::method_not_found()
                        .data(format!("unknown ACP extension method: {other}")),
                )
            }
        };
        if let Some(err) = backend_no_bridge_err
            && matches!(
                & result, Err(e) if e.code == acp::Error::method_not_found().code
            )
        {
            return Err(err);
        }
        result
    }
    async fn ext_notification(
        &self,
        args: acp::ExtNotification,
    ) -> Result<(), acp::Error> {
        tracing::info!("Received extension notification: method={}", args.method);
        if args.method.as_ref() == "kigi/yolo_mode_changed"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let sender_id = params.get("clientIdentifier").and_then(|v| v.as_str());
            let permission_mode = params
                .get("permission_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let yolo_signal = params.get("yolo_mode").and_then(|v| v.as_bool());
            if let Some(yolo_mode) = yolo_signal {
                let mut sessions = self.sessions.borrow_mut();
                let updated_sessions = apply_yolo_mode_to_matching_sessions(
                    &mut sessions,
                    sender_id,
                    yolo_mode,
                );
                tracing::info!(
                    yolo_mode, sender = ? sender_id, target_sessions = updated_sessions,
                    total_sessions = sessions.len(),
                    "Setting YOLO mode for matching sessions"
                );
            }
            let auto_mode_explicit = params.get("auto_mode").and_then(|v| v.as_bool());
            let want_auto = auto_mode_explicit == Some(true)
                || permission_mode == "auto";
            let clear_auto = auto_mode_explicit == Some(false)
                || (matches!(permission_mode, "always-approve" | "ask" | "default")
                    && !want_auto);
            let enable_auto = want_auto && yolo_signal != Some(true);
            if enable_auto || clear_auto {
                let enabled = enable_auto;
                let matches_sender = |h: &crate::session::SessionHandle| -> bool {
                    sender_id.is_none()
                        || h.origin_client.as_ref().map(|c| c.product.as_str())
                            == sender_id
                };
                let mut sessions = self.sessions.borrow_mut();
                let total_sessions = sessions.len();
                let mut updated = 0;
                for h in sessions.values_mut() {
                    if !matches_sender(h) {
                        continue;
                    }
                    if h
                        .cmd_tx
                        .send(crate::session::SessionCommand::SetAutoMode {
                            enabled,
                        })
                        .is_ok()
                    {
                        if enabled {
                            h.yolo_mode = false;
                        }
                        updated += 1;
                    }
                }
                tracing::info!(
                    auto_mode = enabled, sender = ? sender_id, target_sessions = updated,
                    total_sessions, "Setting auto permission mode for matching sessions"
                );
            }
        }
        if args.method.as_ref() == "kigi/permissions/reset" {
            let sessions = self.sessions.borrow();
            let updated = sessions
                .values()
                .filter(|h| {
                    h
                        .cmd_tx
                        .send(crate::session::SessionCommand::ResetPermissionState)
                        .is_ok()
                })
                .count();
            tracing::info!(
                target_sessions = updated, total_sessions = sessions.len(),
                "Permission state reset for matching sessions"
            );
        }
        if args.method.as_ref() == "kigi/internal/evict_sessions" {
            self.handle_evict_sessions(&args.params).await;
        }
        if args.method.as_ref() == "kigi/toggle_plan_mode"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let handle = self
                .sessions
                .borrow()
                .values()
                .find(|s| s.info.id.0.as_ref() == session_id_str)
                .cloned();
            if let Some(handle) = handle {
                let is_engaged = handle.plan_mode.lock().state()
                    != crate::session::plan_mode::PlanModeState::Inactive;
                let next_mode_id = acp::SessionModeId::new(
                    if is_engaged { "default" } else { "plan" },
                );
                let (tx, rx) = oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SessionMode {
                        session_mode: next_mode_id.clone(),
                        responds_to: tx,
                    });
                if rx.await.is_err() {
                    tracing::warn!(
                        session_id = % session_id_str, mode_id = % next_mode_id.0,
                        "toggle_plan_mode: session mode update failed"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = % session_id_str, "toggle_plan_mode: session not found"
                );
            }
        }
        if matches!(
            args.method.as_ref(), "kigi/queue/remove" | "kigi/queue/reorder" |
            "kigi/queue/clear" | "kigi/queue/edit" | "kigi/queue/interject"
        )
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let owner = params
                .get("owner")
                .or_else(|| params.get("clientIdentifier"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let handle = self
                .sessions
                .borrow()
                .values()
                .find(|s| s.info.id.0.as_ref() == session_id_str)
                .cloned();
            if let Some(handle) = handle {
                let cmd = crate::agent::ext_parsers::parse_queue_edit_command(
                    args.method.as_ref(),
                    &params,
                    owner,
                );
                if let Some(cmd) = cmd && handle.cmd_tx.send(cmd).is_err() {
                    tracing::warn!(
                        session_id = % session_id_str, method = % args.method,
                        "queue edit: failed to forward SessionCommand (session actor gone)"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = % session_id_str, method = % args.method,
                    "queue edit: session not found"
                );
            }
        }
        if args.method.as_ref() == "kigi/terminal/pty/input"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            crate::extensions::terminal::handle_pty_input(&params).await;
        }
        if args.method.as_ref() == "_kigi/session/update" {
            if let Ok(notification) = serde_json::from_str::<
                SessionNotification,
            >(args.params.get()) {
                tracing::info!(
                    "Storing xAI session notification: session_id={}", notification
                    .session_id.0
                );
                if let Some(handle) = self
                    .sessions
                    .borrow()
                    .get(&notification.session_id)
                {
                    let _ = handle
                        .cmd_tx
                        .send(crate::session::SessionCommand::XaiSessionNotification {
                            notification,
                        });
                } else {
                    tracing::warn!(
                        "Received xAI session notification for unknown session: {}",
                        notification.session_id.0
                    );
                }
            } else {
                tracing::warn!("Failed to parse xAI session notification params");
            }
        }
        if args.method.as_ref() == "kigi/telemetry/non_git_decision" {
            #[derive(serde::Deserialize)]
            struct NonGitDecisionParams {
                decision: String,
                session_id: String,
                #[serde(default)]
                client_version: Option<String>,
            }
            if let Ok(params) = serde_json::from_str::<
                NonGitDecisionParams,
            >(args.params.get()) {
                tracing::info!(
                    decision = % params.decision, session_id = % params.session_id,
                    client_version = ? params.client_version, "non_git_decision",
                );
            } else {
                tracing::warn!("Failed to parse non_git_decision telemetry params");
            }
        }
        if args.method.as_ref() == "kigi/telemetry/multi_agent_followup" {
            #[derive(serde::Deserialize)]
            struct MultiAgentFollowupParams {
                preferred_agent_label: char,
                preferred_agent_session_id: Option<String>,
                preferred_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                other_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentFollowupParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent followup telemetry: preferred_agent={}", params
                    .preferred_agent_label
                );
                let total_agents = 1 + params.other_agents.len();
            } else {
                tracing::warn!("Failed to parse multi-agent followup telemetry params");
            }
        }
        if args.method.as_ref() == "kigi/telemetry/multi_agent_apply" {
            #[derive(serde::Deserialize)]
            struct MultiAgentApplyParams {
                applied_agent_label: char,
                applied_agent_session_id: Option<String>,
                applied_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentApplyParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent apply telemetry: applied_agent={}", params
                    .applied_agent_label
                );
                let total_agents = 1 + params.discarded_agents.len();
            } else {
                tracing::warn!("Failed to parse multi-agent apply telemetry params");
            }
        }
        if args.method.as_ref() == "kigi/telemetry/multi_agent_discard" {
            #[derive(serde::Deserialize)]
            struct MultiAgentDiscardParams {
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentDiscardParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent discard telemetry: {} agents discarded", params
                    .discarded_agents.len()
                );
                let total = params.discarded_agents.len();
            } else {
                tracing::warn!("Failed to parse multi-agent discard telemetry params");
            }
        }
        if args.method.as_ref() == kigi_log::unified_log::LOG_METHOD
            && let Ok(params) = serde_json::from_str::<
                kigi_log::unified_log::LogNotificationParams,
            >(args.params.get())
        {
            kigi_log::unified_log::ingest_client_entries(
                params.src,
                &params.entries,
            );
        }
        Ok(())
    }
}
