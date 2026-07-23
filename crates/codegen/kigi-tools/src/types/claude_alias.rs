//! Canonical external-settings tool name ↔ Kigi tool correspondence.
//!
//! Two consumers read it independently. The hook matcher (`kigi-hooks`) needs the
//! Kigi tool **names** an external settings term maps to (and the reverse, for regex
//! matchers); the agent builder (`kigi-agent`) needs the [`ToolKind`] a `tools:`
//! allowlist entry resolves to. A row may carry a kind without names (`PowerShell`
//! shares `Execute`, with no distinct tool) or names without a kind (e.g.
//! `Agent`/`ExitPlanMode`/`Cron*` are matchable but not allowlist-resolvable).
//!
//! The `kigi` names are test-checked against the live registry.

use super::tool::ToolKind;
use ToolKind::*;

/// One Claude tool's correspondence to Kigi, read via the accessor functions below.
struct ClaudeTool {
    claude: &'static str,
    /// Kigi [`ToolKind`] for allowlist resolution; `None` for names that are matchable
    /// (spawn/plan-mode directives) but must not resolve an allowlist.
    kind: Option<ToolKind>,
    /// Kigi tool names this Claude tool maps to (empty when there is no direct
    /// Kigi tool — the entry then only contributes a `kind`).
    kigi: &'static [&'static str],
}

/// Row that resolves an allowlist (carries a [`ToolKind`]) — the common case.
const fn k(claude: &'static str, kind: ToolKind, kigi: &'static [&'static str]) -> ClaudeTool {
    ClaudeTool {
        claude,
        kind: Some(kind),
        kigi,
    }
}

/// Row that is matchable but not allowlist-resolvable (`kind: None`).
const fn match_only(claude: &'static str, kigi: &'static [&'static str]) -> ClaudeTool {
    ClaudeTool {
        claude,
        kind: None,
        kigi,
    }
}

#[rustfmt::skip]
const CLAUDE_TOOLS: &[ClaudeTool] = &[
    k("Read",            Read,                 &["read_file", "hashline_read"]),
    // search_replace kept for back-compat.
    k("Write",           Write,                &["write", "search_replace", "hashline_edit"]),
    k("Edit",            Edit,                 &["search_replace", "hashline_edit"]),
    // Legacy, superseded by Edit.
    k("MultiEdit",       Edit,                 &["search_replace", "hashline_edit"]),
    k("NotebookEdit",    Edit,                 &["search_replace", "hashline_edit"]),
    k("Bash",            Execute,              &["run_terminal_command"]),
    k("PowerShell",      Execute,              &[]),
    k("Grep",            Search,               &["grep", "hashline_grep"]),
    k("Glob",            List,                 &["list_dir"]),
    // Legacy name for Glob.
    k("LS",              List,                 &[]),
    k("LSP",             Lsp,                  &["lsp"]),
    k("WebSearch",       WebSearch,            &["web_search"]),
    k("WebFetch",        WebFetch,             &["web_fetch"]),
    k("DeployApp",       DeployApp,            &[]),
    k("TodoWrite",       Plan,                 &["todo_write"]),
    k("AskUserQuestion", AskUser,              &["ask_user_question"]),
    k("TaskOutput",      BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("BashOutput",      BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("BashOutputTool",  BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("AgentOutputTool", BackgroundTaskAction, &["get_command_or_subagent_output", "get_terminal_command_output"]),
    k("TaskStop",        KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    k("KillShell",       KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    k("KillBash",        KillTaskAction,       &["kill_command_or_subagent", "kill_terminal_command"]),
    // Matcher fires on opencode's `skill` tool; allowlist Read because kigi-build reads SKILL.md.
    k("Skill",           Read,                 &["skill"]),
    k("ToolSearch",      SearchTool,           &["search_tool"]),
    // Canonical; Task is the legacy alias.
    match_only("Agent",         &["spawn_subagent"]),
    match_only("Task",          &["spawn_subagent"]),
    // kind=None: enter/exit must stay paired.
    match_only("EnterPlanMode", &["enter_plan_mode"]),
    match_only("ExitPlanMode",  &["exit_plan_mode"]),
    match_only("CronCreate",    &["scheduler_create"]),
    match_only("CronDelete",    &["scheduler_delete"]),
    match_only("CronList",      &["scheduler_list"]),
    // cursor preset.
    match_only("ListMcpResourcesTool", &["ListMcpResources"]),
];

/// The Kigi [`ToolKind`] a Claude allowlist entry resolves to, if any.
pub fn kind_for(claude: &str) -> Option<ToolKind> {
    CLAUDE_TOOLS
        .iter()
        .find(|t| t.claude == claude)
        .and_then(|t| t.kind)
}

/// The Kigi tool names a Claude matcher term fires on.
pub fn kigi_names_for(claude: &str) -> impl Iterator<Item = &'static str> {
    CLAUDE_TOOLS
        .iter()
        .find(|t| t.claude == claude)
        .map(|t| t.kigi)
        .unwrap_or(&[])
        .iter()
        .copied()
}

/// The Claude names that map to `kigi_name` (reverse lookup, for regex matchers).
pub fn claude_names_for(kigi_name: &str) -> impl Iterator<Item = &'static str> + '_ {
    CLAUDE_TOOLS
        .iter()
        .filter(move |t| t.kigi.contains(&kigi_name))
        .map(|t| t.claude)
}

/// Every distinct Kigi name the table references, for the `kigi-agent` drift-check
/// test that asserts each is a real client tool name.
pub fn kigi_names() -> impl Iterator<Item = &'static str> {
    let mut seen = std::collections::HashSet::new();
    CLAUDE_TOOLS
        .iter()
        .flat_map(|t| t.kigi.iter().copied())
        .filter(move |name| seen.insert(*name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_are_unique() {
        // The drift this registry exists to prevent: two rows for one Claude name.
        let mut seen = std::collections::HashSet::new();
        for t in CLAUDE_TOOLS {
            assert!(seen.insert(t.claude), "duplicate Claude name: {}", t.claude);
        }
    }

    #[test]
    fn every_row_contributes() {
        // A row with neither a kind nor a Kigi name is dead weight (and signals a typo).
        for t in CLAUDE_TOOLS {
            assert!(
                t.kind.is_some() || !t.kigi.is_empty(),
                "dead row: {}",
                t.claude
            );
        }
    }
}
