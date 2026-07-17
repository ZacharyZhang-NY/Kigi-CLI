use kigi_workspace::permission::Decision;

/// Permission-mode label for the `session.permission_mode_changed` span.
pub(crate) fn permission_mode_label(is_yolo: bool) -> &'static str {
    if is_yolo {
        "bypassPermissions"
    } else {
        "default"
    }
}

/// Telemetry `source` label for a permission [`Decision`] on the `tool.decision`
/// span. `is_yolo` collapses auto-approvals to `config`. `Decision::Allow`/`Ask`
/// carry no provenance, so a config/policy allow is indistinguishable from a
/// user click — report neutral `allowed` rather than guessing `user_temporary`.
pub(crate) fn permission_decision_source(decision: &Decision, is_yolo: bool) -> &'static str {
    match decision {
        Decision::PolicyDeny(_) => "config",
        Decision::Reject(_) => "user_reject",
        Decision::Cancelled => "user_abort",
        Decision::FollowupMessage(_) => "user_followup",
        Decision::Allow | Decision::Ask if is_yolo => "config",
        Decision::Allow | Decision::Ask => "allowed",
    }
}

/// Emit an `mcp.server_connection` span. `duration_ms` / `tool_count` /
/// `error_type` are status-specific; pass `None` when not applicable.
pub(crate) fn emit_mcp_connection_span(
    status: &str,
    server_name: &str,
    transport_type: &str,
    server_scope: &str,
    duration_ms: Option<i64>,
    tool_count: Option<i64>,
    error_type: Option<&str>,
) {
    let span = tracing::info_span!(
        "mcp.server_connection",
        status,
        server_name,
        transport_type,
        server_scope,
        duration_ms = tracing::field::Empty,
        tool_count = tracing::field::Empty,
        error_type = tracing::field::Empty,
    );
    if let Some(d) = duration_ms {
        span.record("duration_ms", d);
    }
    if let Some(t) = tool_count {
        span.record("tool_count", t);
    }
    if let Some(e) = error_type {
        span.record("error_type", e);
    }
    span.in_scope(|| {});
}

/// Provenance for `skill.activated`'s `skill_source`: project (under `cwd`),
/// user (under `$HOME`), else bundled. Paths are canonicalized (symlinked cwd
/// like macOS `/tmp` vs `/private/tmp`); when both roots match, the deepest
/// wins, tie (cwd == `$HOME`) → user.
pub(crate) fn skill_source_label(skill_path: &str, cwd: &str) -> &'static str {
    let canon = |p: &std::path::Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let p = canon(std::path::Path::new(skill_path));
    let depth_if_under =
        |base: std::path::PathBuf| p.starts_with(&base).then(|| base.components().count());
    let project = depth_if_under(canon(std::path::Path::new(cwd)));
    let user = crate::util::kigi_home::kigi_home()
        .parent()
        .and_then(|home| depth_if_under(canon(home)));
    match (project, user) {
        (Some(pd), Some(ud)) if pd > ud => "projectSettings",
        (Some(_), Some(_)) => "userSettings",
        (Some(_), None) => "projectSettings",
        (None, Some(_)) => "userSettings",
        (None, None) => "bundled",
    }
}

pub(crate) fn format_hook_name(spec: &kigi_hooks::config::HookSpec) -> String {
    let scope = spec.name.split(':').next().unwrap_or("unknown");
    match spec.configured_matcher.as_deref() {
        Some(m) if !m.is_empty() => format!("{scope}:{}:{}", spec.event, m.to_lowercase()),
        _ => format!("{scope}:{}", spec.event),
    }
}

/// Provenance from the namespace prefix each loader stamps on the spec name:
/// `global/` → user, `project/` → project, `plugin/` → plugin, `agent:` →
/// agent, else unknown. (Source-dir classification was wrong — both global and
/// project dirs contain `/.kigi/`.)
fn format_hook_source(spec: &kigi_hooks::config::HookSpec) -> &'static str {
    let name = spec.name.as_str();
    if name.starts_with("global/") {
        "userSettings"
    } else if name.starts_with("project/") {
        "projectSettings"
    } else if name.starts_with("plugin/") {
        "pluginHook"
    } else if name.starts_with("agent:") {
        "agentHook"
    } else {
        "unknown"
    }
}

/// Per-hook inventory recorded as a `hook.registered` span at session start.
pub(crate) struct HookRegInfo {
    pub name: String,
    pub event: String,
    pub hook_type: String,
    pub source: &'static str,
}

impl HookRegInfo {
    pub(crate) fn from_spec(spec: &kigi_hooks::config::HookSpec) -> Self {
        Self {
            name: format_hook_name(spec),
            event: spec.event.to_string(),
            hook_type: spec.handler_type.clone(),
            source: format_hook_source(spec),
        }
    }
}

/// Emit one `plugin.loaded` span per enabled plugin and one
/// `hook.registered` span per configured hook at session start.
pub(crate) fn emit_session_registration_spans(
    plugin_registry: Option<&kigi_agent::plugins::PluginRegistry>,
    hooks: &[HookRegInfo],
) {
    if let Some(registry) = plugin_registry {
        for plugin in registry.enabled_plugins() {
            tracing::info_span!(
                "plugin.loaded",
                plugin_name = %plugin.name,
                plugin_version = %plugin.version.as_deref().unwrap_or(""),
                plugin_scope = plugin.scope.id_label(),
                has_hooks = plugin.has_hooks,
                has_mcp = plugin.mcp_server_count > 0,
                skill_count = plugin.skill_count as i64,
                agent_count = plugin.agent_count as i64,
                command_path_count = plugin.command_dirs.len() as i64,
            )
            .in_scope(|| {});
        }
    }
    for h in hooks {
        tracing::info_span!(
            "hook.registered",
            hook_name = %h.name,
            hook_event = %h.event,
            hook_type = %h.hook_type,
            hook_source = %h.source,
        )
        .in_scope(|| {});
    }
}
