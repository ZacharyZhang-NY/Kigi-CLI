//! Environment variable helpers and process isolation for terminal execution.
//!
//! All implementations now live in the lightweight [`kigi_tty_utils`] crate
//! so that every crate in the workspace can use them without pulling in the
//! heavyweight `kigi-tools` dependency. This module re-exports the public
//! API for backward compatibility.

pub use kigi_tty_utils::{detach_from_tty, pager_env};

/// Env var set on agent-spawned terminal processes so host tools (e.g. `x ban`)
/// can distinguish agent invocations from human interactive shells.
/// Note: the CLI also uses `KIGI_AGENT` as an
/// optional agent-definition selector for launching `grok` itself; child terminal
/// processes only need the sentinel value `"1"`.
pub const KIGI_AGENT_ENV: &str = "KIGI_AGENT";

/// Sentinel value for [`KIGI_AGENT_ENV`] on agent tool terminals.
pub const KIGI_AGENT_ENV_VALUE: &str = "1";

/// Force `KIGI_AGENT=1` on an agent terminal child so request/login env cannot
/// clear the agent marker.
pub fn apply_grok_agent_marker(cmd: &mut tokio::process::Command) {
    cmd.env(KIGI_AGENT_ENV, KIGI_AGENT_ENV_VALUE);
}

/// Expand the four plugin-path tokens (`${CLAUDE_PLUGIN_ROOT}` / `${KIGI_PLUGIN_ROOT}`
/// and `${CLAUDE_PLUGIN_DATA}` / `${KIGI_PLUGIN_DATA}`) in `s`. Each pair is expanded
/// only when its value is provided. Single source of truth for plugin agent bodies,
/// plugin skill/command bodies, and plugin MCP/hook config substitution.
pub fn substitute_plugin_tokens(
    s: &str,
    plugin_root: Option<&str>,
    plugin_data: Option<&str>,
) -> String {
    let mut out = s.to_string();
    if let Some(root) = plugin_root {
        out = out
            .replace("${CLAUDE_PLUGIN_ROOT}", root)
            .replace("${KIGI_PLUGIN_ROOT}", root);
    }
    if let Some(data) = plugin_data {
        out = out
            .replace("${CLAUDE_PLUGIN_DATA}", data)
            .replace("${KIGI_PLUGIN_DATA}", data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{KIGI_AGENT_ENV, KIGI_AGENT_ENV_VALUE, substitute_plugin_tokens};

    const ALL_TOKENS: &str = "${CLAUDE_PLUGIN_ROOT}/a ${KIGI_PLUGIN_ROOT}/b ${CLAUDE_PLUGIN_DATA}/c ${KIGI_PLUGIN_DATA}/d";

    #[test]
    fn expands_all_four_tokens_when_both_provided() {
        let out = substitute_plugin_tokens(ALL_TOKENS, Some("/root"), Some("/data"));
        assert_eq!(out, "/root/a /root/b /data/c /data/d");
    }

    #[test]
    fn leaves_tokens_literal_when_both_none() {
        let out = substitute_plugin_tokens(ALL_TOKENS, None, None);
        assert_eq!(out, ALL_TOKENS);
    }

    #[test]
    fn expands_only_root_when_data_none() {
        let out = substitute_plugin_tokens(ALL_TOKENS, Some("/root"), None);
        assert_eq!(
            out,
            "/root/a /root/b ${CLAUDE_PLUGIN_DATA}/c ${KIGI_PLUGIN_DATA}/d"
        );
    }

    #[test]
    fn agent_marker_constants_match_cursor_parity() {
        assert_eq!(KIGI_AGENT_ENV, "KIGI_AGENT");
        assert_eq!(KIGI_AGENT_ENV_VALUE, "1");
    }
}
