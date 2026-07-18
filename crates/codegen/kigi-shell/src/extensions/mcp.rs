//! MCP extension methods and business logic.
//!
//! - `x.ai/mcp/list` — list available MCP servers (agent-scoped or session-annotated)
//! - `x.ai/mcp/call` — invoke an MCP tool directly, outside the LLM loop
//! - `x.ai/mcp/servers_updated` — notification pushed when the server list changes
//! - `x.ai/mcp/server_status` — per-server delta pushed by the
//!   `StatusDispatcher` (transport-closed pollers, handshake failures,
//!   config diffs, server-pushed list-changed notifications). See
//!   [`crate::session::mcp_dispatcher`] for the coalescing /
//!   payload-shaping logic. Re-exported below so other crates have a
//!   single import point.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
// rmcp is quarantined in kigi-mcp; see that crate's docs.
use kigi_mcp::rmcp;
// `wire::MCP_CALL` is the one cross-SDK contract literal; the agent-only siblings
// live in `mcp_methods` below.
use kigi_mcp::wire;

use super::{ExtResult, parse_params, to_ext_response};

/// Agent-only `x.ai/mcp/*` ACP method/notification names.
///
/// Unlike [`wire::MCP_CALL`] (the cross-SDK contract, which stays in
/// `kigi_mcp::wire`), these methods are private to the agent↔client channel and
/// are NOT spoken by the SDK. They are centralized here only to avoid scattering the
/// same string literal across dispatch and notification send sites.
pub mod mcp_methods {
    /// Shared prefix that routes every MCP ext method to this module's dispatcher.
    pub const PREFIX: &str = "x.ai/mcp/";

    pub const LIST: &str = "x.ai/mcp/list";
    pub const READ_RESOURCE: &str = "x.ai/mcp/read_resource";
    pub const AUTH_STATUS: &str = "x.ai/mcp/auth_status";
    pub const AUTH_TRIGGER: &str = "x.ai/mcp/auth_trigger";
    pub const TOGGLE: &str = "x.ai/mcp/toggle";
    pub const TOGGLE_TOOL: &str = "x.ai/mcp/toggle_tool";
    pub const UPSERT: &str = "x.ai/mcp/upsert";
    pub const DELETE: &str = "x.ai/mcp/delete";

    pub const SERVERS_UPDATED: &str = "x.ai/mcp/servers_updated";
    pub const TOOLS_CHANGED: &str = "x.ai/mcp/tools_changed";
    pub const INIT_PROGRESS: &str = "x.ai/mcp/init_progress";
}
use crate::agent::MvpAgent;
use crate::session::mcp_servers::{MCP_TOOL_NAME_DELIMITER, McpClient, McpServerName, McpState};

// ── Wire types: mcp/list ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    /// When false, forces an OAuth retry pass on servers stuck in
    /// auth-required state before building the list. Set this after OAuth
    /// enrollment or disconnect.
    #[serde(default = "default_true")]
    pub cache: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpListResponse {
    pub servers: Vec<McpServerEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub source: McpServerSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    #[serde(flatten)]
    pub config: McpServerConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<McpServerSessionState>,
}

/// MCP server config for the `mcp/list` catalog response.
///
/// Distinct from `acp::McpServer` (session/new input) because:
/// - HTTP: exposes the URL, NOT headers (auth tokens stay private)
/// - Stdio: same structure but optimized for JSON wire format
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpServerConfig {
    #[serde(rename = "http")]
    Http { url: String },
    #[serde(rename = "stdio")]
    Stdio {
        command: std::path::PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<McpEnvVar>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct McpEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpServerSource {
    Local,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSessionState {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<McpSessionStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_required: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpSessionStatus {
    Ready,
    Initializing,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// ── Wire types: mcp/call ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallRequest {
    /// When present: session pool. When absent: agent pool (config.toml only).
    #[serde(default)]
    pub session_id: Option<String>,
    pub server: String,
    /// Endpoint URL — disambiguates when multiple servers share a name.
    #[serde(default)]
    pub server_url: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallResponse {
    pub content: Vec<McpContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

// ── Internal types (not serialized to wire) ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct McpStatusSnapshot {
    pub configs: Vec<acp::McpServer>,
    pub clients: Vec<McpClientStatus>,
    pub auth_required: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct McpClientStatus {
    pub name: String,
    pub status: McpSessionStatus,
    pub tools: Vec<McpToolEntry>,
}

// ── Notification: mcp/servers_updated ────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersUpdated {
    pub mcp_servers: Vec<McpServerEntry>,
}

/// Per-server tool-list change push.
///
/// Emitted by [`crate::session::acp_session::AcpSession`] on the
/// post-handshake / auth-recovery and toggle-tool paths. The
/// `session_id` field lets the pager route
/// the push to the owning agent via `find_session_match` rather than
/// falling back to `app.active_view` (a latent multi-agent bug).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolsChanged {
    /// Session that owns this push. Pager routes via
    /// `find_session_match` so a background-agent push does not land
    /// on the foregrounded agent's modal.
    pub session_id: String,
    /// MCP server whose tool list changed.
    ///
    /// Currently unread by the pager — the pager treats every
    /// `tools_changed` push uniformly as a "schedule a debounced
    /// `mcp/list` refetch" trigger and re-reads the full catalog.
    /// The toggle-tool path therefore leaves this empty for
    /// forward-compat; any future field-aware optimization on the
    /// pager side would need to special-case empty as
    /// "non-server-scoped". No consumer reads that sentinel today.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_name: String,
    /// New tool entries for the named server.
    ///
    /// Currently unread by the pager for the same reason as
    /// `server_name` above. Empty on the toggle-tool path; populated
    /// on the post-handshake / auth-recovery paths so future
    /// field-aware consumers can avoid the `mcp/list` round trip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolEntry>,
}

// Re-export the `x.ai/mcp/server_status` schema +
// method constant from the dispatcher module so external callers
// have a single import point alongside the other `x.ai/mcp/*`
// types.
//
// The canonical definitions still live in
// [`crate::session::mcp_dispatcher`] because their primary consumer
// is the dispatcher loop (and the unit tests there). The
// `session → extensions` direction is the inverse of the typical
// `extensions → session` flow, but moving the types here would
// require either making the dispatcher import from `extensions`
// (same inversion) or duplicating the schema. Leaving the
// re-export here keeps the single import-point ergonomic without
// duplicating definitions.
pub use crate::session::mcp_dispatcher::{
    McpServerStatus, McpServerStatusPayload, McpServerStatusReason, SERVER_STATUS_METHOD,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    pub server: String,
    pub uri: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceResponse {
    pub contents: Vec<McpReadResourceContent>,
}

/// A single resource content block from `resources/read`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// Push the full MCP catalog to the client. Called in the background so
/// `initialize()` isn't blocked by catalog construction.
pub async fn notify_servers_updated(
    gateway: &kigi_acp_lib::AcpAgentGatewaySender,
    local_servers: &[acp::McpServer],
) {
    let catalog = build_mcp_catalog(local_servers);
    let payload = McpServersUpdated {
        mcp_servers: catalog,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&payload) {
        let notification = acp::ExtNotification::new(mcp_methods::SERVERS_UPDATED, params.into());
        let _ = gateway.ext_notification(notification).await;
        tracing::info!("Sent x.ai/mcp/servers_updated notification to client");
    }
}

// ── Dispatch ────────────────────────────────────────────────────────

/// Inbound `x.ai/mcp/*` methods this agent services, resolved from the wire string.
///
/// Single source of truth for forward-method routing: [`handle`] maps each variant to
/// its handler, and an unknown method yields `None` → `method_not_found`. The reverse
/// method [`wire::MCP_SDK_CALL`] is emit-only (agent→client) and has no variant here,
/// so a stray inbound reverse call is never misrouted to the forward `handle_call`.
#[derive(Debug, PartialEq, Eq)]
enum McpRoute {
    List,
    Call,
    ReadResource,
    AuthStatus,
    AuthTrigger,
    Toggle,
    ToggleTool,
    Upsert,
    Delete,
}

fn route_mcp_method(method: &str) -> Option<McpRoute> {
    Some(match method {
        mcp_methods::LIST => McpRoute::List,
        wire::MCP_CALL => McpRoute::Call,
        mcp_methods::READ_RESOURCE => McpRoute::ReadResource,
        mcp_methods::AUTH_STATUS => McpRoute::AuthStatus,
        mcp_methods::AUTH_TRIGGER => McpRoute::AuthTrigger,
        mcp_methods::TOGGLE => McpRoute::Toggle,
        mcp_methods::TOGGLE_TOOL => McpRoute::ToggleTool,
        mcp_methods::UPSERT => McpRoute::Upsert,
        mcp_methods::DELETE => McpRoute::Delete,
        _ => return None,
    })
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match route_mcp_method(args.method.as_ref()) {
        Some(McpRoute::List) => handle_list(agent, args).await,
        Some(McpRoute::Call) => handle_call(agent, args).await,
        Some(McpRoute::ReadResource) => handle_read_resource(agent, args).await,
        Some(McpRoute::AuthStatus) => handle_auth_status(agent, args).await,
        Some(McpRoute::AuthTrigger) => handle_auth_trigger(agent, args).await,
        Some(McpRoute::Toggle) => handle_toggle(agent, args).await,
        Some(McpRoute::ToggleTool) => handle_toggle_tool(agent, args).await,
        Some(McpRoute::Upsert) => handle_upsert(agent, args).await,
        Some(McpRoute::Delete) => handle_delete(agent, args).await,
        None => Err(acp::Error::method_not_found()),
    }
}

// ── Catalog (shared by mcp/list and InitializeResponse._meta) ───────

/// Extract URL from an MCP server (HTTP/SSE only, None for Stdio).
fn mcp_server_url(server: &acp::McpServer) -> Option<&str> {
    match server {
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => Some(url.as_str()),
        acp::McpServer::Stdio(acp::McpServerStdio { .. }) => None,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => None,
    }
}

/// Build MCP server catalog from local servers, deduplicated by name.
/// Pure function — no I/O. Used by `mcp/list`, `InitializeResponse._meta`,
/// and `mcp/servers_updated`.
pub fn build_mcp_catalog(local_servers: &[acp::McpServer]) -> Vec<McpServerEntry> {
    let mut servers: Vec<McpServerEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Local servers (HTTP or Stdio)
    for server in local_servers {
        let name = crate::session::mcp_servers::mcp_server_name(server).to_string();
        if seen.insert(name.clone()) {
            let config = match server {
                acp::McpServer::Http(acp::McpServerHttp { url, .. })
                | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => {
                    McpServerConfig::Http { url: url.clone() }
                }
                acp::McpServer::Stdio(acp::McpServerStdio {
                    command, args, env, ..
                }) => McpServerConfig::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env
                        .iter()
                        .map(|e| McpEnvVar {
                            name: e.name.clone(),
                            value: e.value.clone(),
                        })
                        .collect(),
                },
                // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
                _ => continue,
            };
            servers.push(McpServerEntry {
                name,
                display_name: None,
                source: McpServerSource::Local,
                config,
                source_label: None,
                session: None,
            });
        }
    }

    servers
}

fn disabled_server_placeholder_entry(name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_owned(),
        display_name: None,
        source: McpServerSource::Local,
        source_label: None,
        config: McpServerConfig::Stdio {
            command: std::path::PathBuf::new(),
            args: Vec::new(),
            env: Vec::new(),
        },
        session: Some(McpServerSessionState {
            enabled: false,
            status: None,
            tools: vec![],
            auth_required: false,
        }),
    }
}

// ── Session-level operations (called via SessionCommand) ────────────

/// Build session MCP status: which servers are enabled, healthy, and what tools they expose.
/// Clones state under lock then releases — does not hold lock across awaits.
pub async fn build_mcp_status(
    mcp_state: &Arc<TokioMutex<McpState>>,
    tool_bridge: &Arc<kigi_tools::bridge::ToolBridge>,
    event_writer: Option<&kigi_file_utils::events::EventWriter>,
) -> McpStatusSnapshot {
    let _build_mcp_status_timer = crate::instrumentation::timer("build_mcp_status");
    let (
        configs,
        clients,
        _is_initializing,
        initializing_servers,
        mcp_tool_meta,
        auth_required,
        init_failed,
        disabled_regs,
    ) = {
        let state = mcp_state.lock().await;
        (
            state.configs.clone(),
            state
                .all_clients()
                .map(|(_, c)| c.clone())
                .collect::<Vec<_>>(),
            state.is_initializing(),
            state.handshaking_servers_cloned(),
            state.mcp_tool_meta.clone(),
            state.auth_required.clone(),
            state.init_failed.clone(),
            // Collect (qualified_name, description) for disabled tools so we
            // can include them in the snapshot without cloning the full registration.
            state
                .disabled_tool_registrations
                .iter()
                .map(|(k, v)| (k.clone(), v.description.clone()))
                .collect::<Vec<_>>(),
        )
    };

    let mut client_statuses = Vec::with_capacity(clients.len());
    let _client_loop_timer = crate::instrumentation::timer("mcp_status_client_loop");

    for client in &clients {
        let name = client.server_name().to_string();
        let prefix = format!("{}{}", name, MCP_TOOL_NAME_DELIMITER);

        let healthy = client.is_healthy().await;
        if let Some(ew) = event_writer {
            ew.emit(kigi_file_utils::events::Event::McpHealthCheck {
                server_name: name.clone(),
                healthy,
                client_state: Some(if healthy { "ready" } else { "unavailable" }.to_string()),
            });
        }
        // A server whose background init failed (handshake/`tools/list`
        // error or timeout) is reported as Unavailable even when the
        // transport is still technically alive — otherwise a server that
        // connected but wedged on `tools/list` (0 tools registered) would
        // misleadingly show as Ready.
        let ready = healthy && !init_failed.contains_key(name.as_str());
        let (status, tools) = if ready {
            let _tool_defs_timer = crate::instrumentation::timer("mcp_status_tool_definitions");
            let mut tools: Vec<McpToolEntry> = tool_bridge
                .tool_definitions()
                .await
                .into_iter()
                .filter(|t| t.function.name.starts_with(&prefix))
                .map(|t| {
                    let qualified_name = &t.function.name;
                    let unqualified = qualified_name
                        .strip_prefix(&prefix)
                        .unwrap_or(qualified_name)
                        .to_string();
                    let meta = mcp_tool_meta.get(qualified_name).cloned();
                    McpToolEntry {
                        name: unqualified,
                        display_name: None,
                        description: t.function.description.clone(),
                        meta,
                        enabled: true,
                    }
                })
                .collect();

            // Include disabled tools from stashed registrations.
            for (qname, desc) in &disabled_regs {
                if qname.starts_with(&prefix) {
                    let unqualified = qname.strip_prefix(&prefix).unwrap_or(qname).to_string();
                    let meta = mcp_tool_meta.get(qname).cloned();
                    tools.push(McpToolEntry {
                        name: unqualified,
                        display_name: None,
                        description: Some(desc.clone()),
                        meta,
                        enabled: false,
                    });
                }
            }

            // Stable alphabetical order so tools don't jump around
            // when toggled between enabled and disabled.
            tools.sort_by(|a, b| a.name.cmp(&b.name));

            (McpSessionStatus::Ready, tools)
        } else {
            (McpSessionStatus::Unavailable, vec![])
        };

        client_statuses.push(McpClientStatus {
            name,
            status,
            tools,
        });
    }

    // Configured but not yet handshaked (either global init or per-server bg init) → Initializing.
    // We use initializing_servers (populated before spawning handshakes) so that
    // slow servers continue showing Initializing after we call finish_init() early.
    for config in &configs {
        let cname = crate::session::mcp_servers::mcp_server_name(config);
        if !client_statuses.iter().any(|c| c.name == cname) && initializing_servers.contains(cname)
        {
            client_statuses.push(McpClientStatus {
                name: cname.to_string(),
                status: McpSessionStatus::Initializing,
                tools: vec![],
            });
        }
    }

    McpStatusSnapshot {
        configs,
        clients: client_statuses,
        auth_required,
    }
}

/// Ensure the agent-level MCP pool is initialized, waiting if another
/// caller is already initializing. Safe to call concurrently.
async fn ensure_agent_pool_initialized(mcp_state: &Arc<TokioMutex<McpState>>) {
    loop {
        let state = mcp_state.lock().await;
        if state.is_initialized() {
            return;
        }
        if state.is_initializing() {
            // Another call is initializing — wait and retry.
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        drop(state);
        let cwd = std::env::current_dir().unwrap_or_default();
        init_agent_mcp_pool(mcp_state, &cwd).await;
        return;
    }
}

/// Spawn config.toml MCP clients into the agent pool. Handshakes happen
/// lazily on first `CallMcpTool`.
pub async fn init_agent_mcp_pool(mcp_state: &Arc<TokioMutex<McpState>>, cwd: &std::path::Path) {
    use crate::session::mcp_servers::start_mcp_servers;

    let configs = {
        let mut state = mcp_state.lock().await;
        if !state.try_start_init() {
            return;
        }
        state.configs.clone()
    };

    if configs.is_empty() {
        let mut state = mcp_state.lock().await;
        state.finish_init();
        return;
    }

    let noop = kigi_file_utils::events::EventWriter::noop();
    let results = start_mcp_servers(
        configs,
        None,
        Some(cwd),
        &Default::default(),
        &Default::default(),
        &noop,
        // Pass Interactive to preserve prior deferred-OAuth behavior. A session-less SDK agent can
        // reach this non-interactively; threading real non-interactivity here is a deliberate follow-up.
        crate::session::mcp_servers::OauthInteractivity::Interactive,
    )
    .await;
    let clients: HashMap<McpServerName, Arc<McpClient>> = results
        .into_iter()
        .filter_map(|r| match r {
            Ok(client) => {
                tracing::info!("Agent MCP server '{}' spawned", client.server_name());
                let name = client.server_name().to_string();
                Some((name, Arc::new(client)))
            }
            Err(e) => {
                tracing::warn!("Failed to spawn agent MCP server: {}", e);
                None
            }
        })
        .collect();

    let mut state = mcp_state.lock().await;
    state.owned_clients = clients;
    state.finish_init();
    tracing::info!(
        "Agent MCP pool: {} servers ready",
        state.owned_clients.len()
    );
}

/// Call an MCP tool directly (outside the LLM tool-use loop).
#[tracing::instrument(name = "mcp.call_tool", skip_all, fields(server_name, tool_name))]
pub async fn call_mcp_tool(
    mcp_state: &Arc<TokioMutex<McpState>>,
    server_name: &str,
    server_url: Option<&str>,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<McpCallResponse, String> {
    let client = {
        let state = mcp_state.lock().await;

        // Resolve: (name + url) > url-only > name-only.
        let target = if let Some(url) = server_url {
            let config_name =
                |c: &acp::McpServer| crate::session::mcp_servers::mcp_server_name(c).to_string();
            state
                .configs
                .iter()
                .find(|c| {
                    crate::session::mcp_servers::mcp_server_name(c) == server_name
                        && mcp_server_url(c) == Some(url)
                })
                .map(&config_name)
                .or_else(|| {
                    state
                        .configs
                        .iter()
                        .find(|c| mcp_server_url(c) == Some(url))
                        .map(&config_name)
                })
                .unwrap_or_else(|| server_name.to_string())
        } else {
            server_name.to_string()
        };

        Arc::clone(
            state
                .get_client(&target)
                .ok_or_else(|| format!("server '{}' not found", target))?,
        )
    };

    let tool_timeout_sec = client.tool_timeout_for(tool_name);
    let timeout = std::time::Duration::from_secs(tool_timeout_sec);
    let result = tokio::time::timeout(timeout, client.call_tool(tool_name, arguments))
        .await
        .map_err(|_| format!("tool '{}' timed out after {}s", tool_name, tool_timeout_sec))?
        .map_err(|e| format!("tool call failed: {}", e))?;

    let content = result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(McpContentBlock {
                kind: "text".to_string(),
                text: t.text.clone(),
            }),
            rmcp::model::ContentBlock::Resource(r) => {
                serde_json::to_string(r).ok().map(|json| McpContentBlock {
                    kind: "resource".to_string(),
                    text: json,
                })
            }
            _ => None,
        })
        .collect();

    Ok(McpCallResponse {
        content,
        is_error: result.is_error,
    })
}

// ── mcp/list handler ────────────────────────────────────────────────

async fn handle_list(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpListRequest>(args)?;

    let cwd = req
        .session_id
        .as_ref()
        .and_then(|sid| agent.get_session_cwd(&acp::SessionId::new(sid.clone())))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let session_handle = req.session_id.as_ref().and_then(|sid| {
        let acp_id = acp::SessionId::new(sid.clone());
        agent.get_session_handle(&acp_id)
    });
    if let (Some(sid), None) = (req.session_id.as_ref(), session_handle.as_ref()) {
        tracing::debug!(
            session_id = %sid,
            "mcp/list: session not found, returning agent-level catalog only"
        );
    }

    let session_snapshot = match session_handle.as_ref() {
        Some(handle) => {
            // Auth retries belong on explicit refresh: skipping them on
            // cached opens saves ~500ms when multiple OAuth servers are
            // configured.
            if !req.cache {
                handle.retry_auth_required_servers().await;
            }
            Some(handle.get_mcp_status().await)
        }
        None => None,
    };

    let local_servers =
        crate::util::config::load_mcp_servers(&cwd, &agent.cfg.borrow().compat_resolved);
    let mut servers = build_mcp_catalog(&local_servers);

    // Include disabled servers from config so they appear in the list
    // with enabled=false and can be re-enabled by the user.
    let disabled_names = crate::util::config::disabled_mcp_server_names(&cwd);
    let catalog_names: std::collections::HashSet<String> =
        servers.iter().map(|s| s.name.clone()).collect();
    for name in &disabled_names {
        if !catalog_names.contains(name) {
            servers.push(disabled_server_placeholder_entry(name));
        }
    }

    if let Some(snapshot) = session_snapshot {
        // `session_snapshot` is `Some` only when `session_handle` resolved,
        // which requires `req.session_id` to have been `Some`. Rather than
        // assert that non-local invariant with `expect` (which a future
        // refactor could silently turn into a panic in a request handler),
        // use a local `if let` guard around the only consumer — the debug
        // log.
        if let Some(sid) = req.session_id.as_ref() {
            tracing::debug!(session_id = %sid, "Annotating mcp/list with session state");
        }
        let catalog_names: std::collections::HashSet<String> =
            servers.iter().map(|s| s.name.clone()).collect();

        // Annotate catalog entries with session state.
        for entry in &mut servers {
            let enabled = snapshot
                .configs
                .iter()
                .any(|c| crate::session::mcp_servers::mcp_server_name(c) == entry.name);
            let (status, tools) = snapshot
                .clients
                .iter()
                .find(|c| c.name == entry.name)
                .map(|c| (Some(c.status.clone()), c.tools.clone()))
                .unwrap_or((None, vec![]));
            entry.session = Some(McpServerSessionState {
                enabled,
                status,
                tools,
                auth_required: snapshot.auth_required.contains(&entry.name),
            });
        }

        // Append session-only servers (passed via session/new but not in catalog).
        for client_status in &snapshot.clients {
            if !catalog_names.contains(&client_status.name) {
                servers.push(McpServerEntry {
                    name: client_status.name.clone(),
                    display_name: None,
                    source: McpServerSource::Local,
                    source_label: None,
                    config: McpServerConfig::Stdio {
                        command: std::path::PathBuf::new(),
                        args: Vec::new(),
                        env: Vec::new(),
                    },
                    session: Some(McpServerSessionState {
                        enabled: true,
                        status: Some(client_status.status.clone()),
                        tools: client_status.tools.clone(),
                        auth_required: snapshot.auth_required.contains(&client_status.name),
                    }),
                });
            }
        }
    }

    // Tag servers with the owning plugin (covers both a plugin's .mcp.json and
    // its inline plugin.json mcpServers via the registry's deduped owner map).
    if let Some(registry) = agent.plugin_registry_snapshot() {
        for entry in &mut servers {
            if let Some(plugin_name) = registry.mcp_server_owner(&entry.name) {
                entry.source_label = Some(format!("plugin: {plugin_name}"));
            }
        }
    }
    to_ext_response(Ok(McpListResponse { servers }))
}

// ── mcp/call handler ────────────────────────────────────────────────

async fn handle_call(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpCallRequest>(args)?;

    let result = match req.session_id {
        Some(sid) => {
            // Session-provided servers: route through the session's MCP pool.
            // Load-race-tolerant: waits for an in-flight `session/load`
            // (reconnect replay after a leader restart) before failing.
            let acp_id = acp::SessionId::new(sid);
            let handle = agent
                .session_handle_waiting_for_load(&acp_id)
                .await
                .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
            handle
                .call_mcp_tool(req.server, req.server_url, req.tool, req.arguments)
                .await
        }
        None => {
            // No session: use the agent-level MCP pool (config.toml servers).
            let mcp_state = agent.agent_mcp_state();
            ensure_agent_pool_initialized(&mcp_state).await;
            call_mcp_tool(
                &mcp_state,
                &req.server,
                req.server_url.as_deref(),
                &req.tool,
                req.arguments,
            )
            .await
        }
    }
    .map_err(|e| acp::Error::internal_error().data(e))?;

    to_ext_response(Ok(result))
}

async fn handle_read_resource(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpReadResourceRequest>(args)?;

    let result = if let Some(ref sid) = req.session_id {
        // Load-race-tolerant: see `handle_call` above.
        let acp_id = acp::SessionId::new(sid.clone());
        let handle = agent
            .session_handle_waiting_for_load(&acp_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
        handle
            .read_mcp_resource(req.server.clone(), req.uri.clone())
            .await
    } else {
        let mcp_state = agent.agent_mcp_state();
        ensure_agent_pool_initialized(&mcp_state).await;
        read_mcp_resource(&mcp_state, &req.server, &req.uri).await
    }
    .map_err(|e| acp::Error::internal_error().data(e))?;

    to_ext_response(Ok(result))
}

pub async fn read_mcp_resource(
    mcp_state: &Arc<TokioMutex<McpState>>,
    server_name: &str,
    uri: &str,
) -> Result<McpReadResourceResponse, String> {
    let client = {
        let state = mcp_state.lock().await;
        Arc::clone(
            state
                .get_client(server_name)
                .ok_or_else(|| format!("server '{}' not found", server_name))?,
        )
    };

    let mcp_service = client
        .ensure_initialized()
        .await
        .map_err(|e| format!("MCP init failed: {}", e))?;

    let result = mcp_service
        .read_resource(rmcp::model::ReadResourceRequestParams::new(uri))
        .await
        .map_err(|e| format!("resource read failed: {}", e))?;

    if result.contents.is_empty() {
        return Err("empty resource".to_string());
    }

    let contents: Vec<McpReadResourceContent> = result
        .contents
        .into_iter()
        .filter_map(|c| match c {
            rmcp::model::ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                meta,
                ..
            } => Some(McpReadResourceContent {
                uri,
                mime_type,
                text: Some(text),
                blob: None,
                meta: meta.and_then(|m| serde_json::to_value(m).ok()),
            }),
            rmcp::model::ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                meta,
                ..
            } => Some(McpReadResourceContent {
                uri,
                mime_type,
                text: None,
                blob: Some(blob),
                meta: meta.and_then(|m| serde_json::to_value(m).ok()),
            }),
            // `ResourceContents` is non_exhaustive; skip unknown variants so
            // the rest of the resource still renders, but log the drop so the
            // missing content is diagnosable.
            _ => {
                tracing::warn!(
                    server = server_name,
                    uri,
                    "skipping unknown MCP resource content variant"
                );
                None
            }
        })
        .collect();

    if contents.is_empty() {
        return Err("resource contained only unsupported content variants".to_string());
    }

    Ok(McpReadResourceResponse { contents })
}

// ── McpResourceProvider bridge ───────────────────────────────────────
//
// Implements the `McpResourceProvider` trait from kigi-tools so that
// `ListMcpResources` / `FetchMcpResource` tools can access MCP
// servers without depending on `kigi-mcp` directly.

/// Bridge from `McpState` to the `McpResourceProvider` trait.
///
/// Injected into the agent's `SharedResources` via `tool_bridge.update_resource()`
/// at session startup so tools can enumerate and fetch MCP resources.
pub struct McpStateResourceProvider(pub Arc<TokioMutex<McpState>>);

#[async_trait::async_trait]
impl kigi_tools::types::resources::McpResourceProvider for McpStateResourceProvider {
    async fn list_resources(
        &self,
        server: Option<String>,
    ) -> Result<Vec<kigi_tools::types::resources::McpResourceInfo>, String> {
        let clients: Vec<(String, Arc<McpClient>)> = {
            let state = self.0.lock().await;
            match &server {
                Some(name) => match state.get_client(name) {
                    Some(c) => vec![(name.clone(), Arc::clone(c))],
                    None => return Err(format!("MCP server '{name}' not found")),
                },
                None => state
                    .all_clients()
                    .map(|(name, client)| (name.to_string(), Arc::clone(client)))
                    .collect(),
            }
        };

        let mut resources = Vec::new();
        for (server_name, client) in clients {
            let mcp_service = match client.ensure_initialized().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "Failed to initialize MCP server for list_resources"
                    );
                    continue;
                }
            };

            match mcp_service.list_all_resources().await {
                Ok(all_resources) => {
                    for r in all_resources {
                        resources.push(kigi_tools::types::resources::McpResourceInfo {
                            uri: r.uri.clone(),
                            name: Some(r.name.clone()),
                            description: r.description.clone(),
                            mime_type: r.mime_type.clone(),
                            server: server_name.clone(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "list_resources RPC failed"
                    );
                    if server.is_some() {
                        return Err(format!("list_resources failed for '{server_name}': {e}"));
                    }
                    // For all-servers mode, skip failures and continue.
                }
            }
        }

        Ok(resources)
    }

    async fn read_resource(
        &self,
        server: String,
        uri: String,
    ) -> Result<kigi_tools::types::resources::McpResourceReadResult, String> {
        let client = {
            let state = self.0.lock().await;
            Arc::clone(
                state
                    .get_client(&server)
                    .ok_or_else(|| format!("MCP server '{server}' not found"))?,
            )
        };

        let mcp_service = client
            .ensure_initialized()
            .await
            .map_err(|e| format!("MCP init failed: {e}"))?;

        let result = mcp_service
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.clone()))
            .await
            .map_err(|e| format!("resource read failed: {e}"))?;

        if result.contents.is_empty() {
            return Err(format!("Resource not found: {uri}"));
        }

        let first = result
            .contents
            .into_iter()
            .find(|c| {
                let supported = matches!(
                    c,
                    rmcp::model::ResourceContents::TextResourceContents { .. }
                        | rmcp::model::ResourceContents::BlobResourceContents { .. }
                );
                if !supported {
                    tracing::warn!(uri, "skipping unknown MCP resource content variant");
                }
                supported
            })
            .ok_or_else(|| format!("Unsupported resource content type for: {uri}"))?;
        match first {
            rmcp::model::ResourceContents::TextResourceContents {
                uri: content_uri,
                mime_type,
                text,
                ..
            } => Ok(kigi_tools::types::resources::McpResourceReadResult {
                uri: content_uri,
                name: None,
                description: None,
                mime_type,
                content: Some(kigi_tools::types::resources::McpResourceContent::Text(text)),
            }),
            rmcp::model::ResourceContents::BlobResourceContents {
                uri: content_uri,
                mime_type,
                blob,
                ..
            } => Ok(kigi_tools::types::resources::McpResourceReadResult {
                uri: content_uri,
                name: None,
                description: None,
                mime_type,
                content: Some(kigi_tools::types::resources::McpResourceContent::Blob(
                    blob.into_bytes(),
                )),
            }),
            // Unreachable: `first` is pre-filtered to supported variants, but
            // `ResourceContents` is non_exhaustive so the match must be total.
            _ => Err(format!("Unsupported resource content type for: {uri}")),
        }
    }
}

// ── Auth status / trigger ────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpAuthStatusRequest {
    session_id: String,
}

#[derive(serde::Serialize)]
pub struct McpAuthStatusEntry {
    pub server_name: String,
    pub status: &'static str,
}

#[derive(serde::Serialize)]
struct McpAuthStatusResponse {
    servers: Vec<McpAuthStatusEntry>,
}

async fn handle_auth_status(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpAuthStatusRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id);
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let entries = handle.mcp_auth_status().await;
    to_ext_response(Ok(McpAuthStatusResponse { servers: entries }))
}

#[derive(serde::Deserialize)]
struct McpAuthTriggerRequest {
    session_id: String,
    server_name: String,
}

#[derive(serde::Serialize)]
struct McpAuthTriggerResponse {
    status: &'static str,
    /// Descriptive failure reason from the shell. `None` on success and on
    /// failures with no detail; surfaced verbatim by the TUI.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn handle_auth_trigger(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpAuthTriggerRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id);
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    match handle.mcp_auth_trigger(req.server_name).await {
        Ok(()) => to_ext_response(Ok(McpAuthTriggerResponse {
            status: "authenticated",
            error: None,
        })),
        Err(e) => {
            tracing::warn!(%e, "MCP auth trigger failed");
            to_ext_response(Ok(McpAuthTriggerResponse {
                status: "failed",
                error: Some(e),
            }))
        }
    }
}

// ── mcp/toggle handler ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpToggleRequest {
    session_id: String,
    server_name: String,
    enabled: bool,
}

#[derive(serde::Serialize)]
struct McpToggleResponse {
    ok: bool,
}

async fn handle_toggle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpToggleRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    // Build the server config outside the session actor. The actual config
    // mutation happens atomically inside the session actor via
    // ToggleMcpServer.
    let server_config = if req.enabled {
        let cwd = agent
            .get_session_cwd(&acp_id)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Err(e) = crate::util::config::save_mcp_server_enabled(&req.server_name, true).await {
            tracing::warn!(
                server = req.server_name.as_str(),
                error = %e,
                "Failed to persist server re-enable before lookup"
            );
        }

        let all_servers_with_policy =
            crate::session::managed_mcp::merge_managed_mcp_servers_with_policy(
                vec![],
                &cwd,
                agent.plugin_registry_snapshot().as_deref(),
                &agent.cfg.borrow().compat_resolved,
            );
        let found = all_servers_with_policy
            .into_iter()
            .find(|s| crate::session::mcp_servers::mcp_server_name(&s.server) == req.server_name);
        match found {
            Some(s) if s.disabled_reason.is_some() => {
                // Capitalize first letter for display.
                let mut chars = req.server_name.chars();
                let capitalized: String = match chars.next() {
                    Some(c) => c.to_uppercase().chain(chars).collect(),
                    None => req.server_name.clone(),
                };
                let path = match &s.disabled_reason {
                    Some(
                        crate::session::managed_mcp::McpDisabledReason::Allowlist { source }
                        | crate::session::managed_mcp::McpDisabledReason::Denylist { source },
                    ) => source.display().to_string(),
                    None => String::new(),
                };
                return Err(acp::Error::invalid_params().data(format!(
                    "The server {capitalized} can't be enabled due to an organization policy ({path}).",
                )));
            }
            None => {
                return Err(acp::Error::invalid_params()
                    .data(format!("server '{}' not found in config", req.server_name)));
            }
            _ => {}
        }
        found.map(|s| s.server)
    } else {
        None
    };

    handle
        .toggle_mcp_server(req.server_name, req.enabled, server_config)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/toggle_tool handler ─────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpToggleToolRequest {
    session_id: String,
    server_name: String,
    tool_name: String,
    enabled: bool,
}

async fn handle_toggle_tool(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpToggleToolRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    handle
        .toggle_mcp_tool(req.server_name, req.tool_name, req.enabled)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/upsert handler ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpUpsertRequest {
    session_id: String,
    server_name: String,
    #[serde(flatten)]
    config: crate::util::config::McpServerConfig,
}

async fn handle_upsert(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpUpsertRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    // Persist to config.toml first.
    crate::util::config::save_mcp_server_config(&req.server_name, &req.config)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    // Build the ACP server config for live addition.
    let server_config = req
        .config
        .to_acp_mcp_server(&req.server_name)
        .ok_or_else(|| acp::Error::invalid_params().data("server config is disabled"))?;

    // Reuse the toggle path: enable=true with the built config.
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    handle
        .toggle_mcp_server(req.server_name, true, Some(server_config))
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

// ── mcp/delete handler ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct McpDeleteRequest {
    session_id: String,
    server_name: String,
}

async fn handle_delete(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req = parse_params::<McpDeleteRequest>(args)?;
    let acp_id = acp::SessionId::new(req.session_id.clone());

    // Verify the server exists in local config (not managed).
    let existed = crate::util::config::delete_mcp_server_config(&req.server_name)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    if !existed {
        return Err(acp::Error::invalid_params().data(format!(
            "server '{}' not found in config.toml (only locally-configured servers can be deleted)",
            req.server_name
        )));
    }

    // Live teardown: disable the server in the running session.
    let handle = agent
        .get_session_handle(&acp_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;

    handle
        .toggle_mcp_server(req.server_name.clone(), false, None)
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;

    // The toggle path spawns a task that adds the server to
    // `disabled_mcp_servers`. Clean that up since we're deleting entirely.
    let _ = crate::util::config::save_mcp_server_enabled(&req.server_name, true).await;

    to_ext_response(Ok(McpToggleResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emit-only reverse method (`x.ai/mcp/sdk_call`) shares the `x.ai/mcp/`
    /// prefix, so `mvp_agent`'s dispatcher routes an inbound copy of it to this
    /// module's `handle`. It must NOT collide with any forward route — i.e. it has no
    /// `McpRoute`, so `handle` returns `method_not_found` instead of misrouting a stray
    /// inbound reverse call to `handle_call`.
    #[test]
    fn inbound_sdk_call_has_no_forward_route() {
        assert!(
            wire::MCP_SDK_CALL.starts_with(mcp_methods::PREFIX),
            "reverse method must share the prefix so it reaches handle()"
        );
        assert_eq!(
            route_mcp_method(wire::MCP_SDK_CALL),
            None,
            "inbound x.ai/mcp/sdk_call must not resolve to a forward handler"
        );
        // Sanity: the forward sibling on the same prefix DOES route.
        assert_eq!(route_mcp_method(wire::MCP_CALL), Some(McpRoute::Call));
    }

    #[test]
    fn test_mcp_list_response_serialization() {
        let resp = McpListResponse {
            servers: vec![
                McpServerEntry {
                    name: "linear".to_string(),
                    display_name: None,
                    source: McpServerSource::Local,
                    config: McpServerConfig::Http {
                        url: "https://mcp.linear.app".to_string(),
                    },
                    source_label: None,
                    session: None,
                },
                McpServerEntry {
                    name: "filesystem".to_string(),
                    display_name: None,
                    source: McpServerSource::Local,
                    source_label: None,
                    config: McpServerConfig::Stdio {
                        command: "/usr/bin/mcp-filesystem".into(),
                        args: vec!["--root".to_string(), "/home".to_string()],
                        env: vec![],
                    },
                    session: Some(McpServerSessionState {
                        enabled: true,
                        status: Some(McpSessionStatus::Ready),
                        auth_required: false,
                        tools: vec![McpToolEntry {
                            name: "read_file".to_string(),
                            display_name: None,
                            description: Some("Read a file".to_string()),
                            meta: None,
                            enabled: true,
                        }],
                    }),
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        // [0] local HTTP
        assert_eq!(json["servers"][0]["source"], "local");
        assert_eq!(json["servers"][0]["type"], "http");
        assert_eq!(json["servers"][0]["url"], "https://mcp.linear.app");
        assert!(json["servers"][0].get("session").is_none());
        // [1] local Stdio
        assert_eq!(json["servers"][1]["source"], "local");
        assert_eq!(json["servers"][1]["type"], "stdio");
        assert_eq!(json["servers"][1]["command"], "/usr/bin/mcp-filesystem");
        assert_eq!(
            json["servers"][1]["args"],
            serde_json::json!(["--root", "/home"])
        );
        assert!(json["servers"][1].get("url").is_none());
        assert_eq!(json["servers"][1]["session"]["enabled"], true);
        assert_eq!(json["servers"][1]["session"]["status"], "ready");
        assert_eq!(
            json["servers"][1]["session"]["tools"][0]["name"],
            "read_file"
        );
    }

    #[test]
    fn test_mcp_call_response_serialization() {
        let resp = McpCallResponse {
            content: vec![McpContentBlock {
                kind: "text".to_string(),
                text: "Created issue LIN-123".to_string(),
            }],
            is_error: Some(false),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "Created issue LIN-123");
        assert_eq!(json["isError"], false);
    }

    #[test]
    fn test_mcp_auth_trigger_response_success_no_error_field() {
        let resp = McpAuthTriggerResponse {
            status: "authenticated",
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "authenticated");
        assert!(
            json.get("error").is_none(),
            "error field must be omitted on success: {json}"
        );
    }

    #[test]
    fn test_mcp_auth_trigger_response_failure_carries_error() {
        let resp = McpAuthTriggerResponse {
            status: "failed",
            error: Some("MCP server 'linear' does not use OAuth".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "failed");
        assert_eq!(
            json["error"], "MCP server 'linear' does not use OAuth",
            "failure must carry the descriptive error verbatim: {json}"
        );
    }

    #[test]
    fn test_mcp_auth_trigger_response_failure_omits_error_when_none() {
        let resp = McpAuthTriggerResponse {
            status: "failed",
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "failed");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn test_disabled_session_state_serialization() {
        let entry = McpServerEntry {
            name: "slack".to_string(),
            display_name: None,
            source: McpServerSource::Local,
            source_label: None,
            config: McpServerConfig::Http {
                url: "https://mcp.slack.com".to_string(),
            },
            session: Some(McpServerSessionState {
                enabled: false,
                status: None,
                tools: vec![],
                auth_required: false,
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["session"]["enabled"], false);
        assert!(json["session"].get("status").is_none());
        assert!(json["session"].get("tools").is_none());
    }
}
