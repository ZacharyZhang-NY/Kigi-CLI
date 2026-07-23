//! Reminder policy — wraps kigi-tools reminder config.

/// Seeds `TodoGateConfig::max_fires_per_prompt`; the gate reads the live value
/// from `ReminderPolicy.todo_gate`, never this constant.
pub const DEFAULT_TODO_GATE_MAX_FIRES: u32 = 2;

/// Session-level system reminder policy.
#[derive(Debug, Clone)]
pub struct ReminderPolicy {
    pub enabled: bool,
    pub todo_nudge: TodoNudgeConfig,
    pub todo_gate: TodoGateConfig,
}

impl Default for ReminderPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            todo_nudge: TodoNudgeConfig::default(),
            todo_gate: TodoGateConfig::default(),
        }
    }
}

/// Reminds the model to call `todo_write` once it has gone
/// `turns_since_todo_write` turns without one, then stays quiet for
/// `turns_between_reminders` turns.
#[derive(Debug, Clone)]
pub struct TodoNudgeConfig {
    pub enabled: bool,
    pub turns_since_todo_write: u32,
    pub turns_between_reminders: u32,
}

impl Default for TodoNudgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            turns_since_todo_write: 3,
            turns_between_reminders: 5,
        }
    }
}

/// Turn-end gate: inspects `TodoState` after every content-only assistant
/// message and forces another turn via `<system-reminder>` injection if
/// pending/unbacked-in-progress todos remain — see
/// `kigi-shell::session::acp_session::evaluate_todo_gate`.
///
/// **Disabled by default.** Operators opt in via the `todo_gate_enabled`
/// remote settings key, or via the `--todo-gate` CLI flag (session-scoped
/// force-enable, highest precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TodoGateConfig {
    pub enabled: bool,
    /// Past this many fires per user prompt the next turn is allowed to end
    /// with `TurnOutcome::Completed`, bounding worst-case extra inference cost.
    pub max_fires_per_prompt: u32,
}

impl Default for TodoGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_fires_per_prompt: DEFAULT_TODO_GATE_MAX_FIRES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_todo_gate_is_disabled_with_const_cap() {
        let cfg = TodoGateConfig::default();
        assert!(!cfg.enabled, "TodoGate must be opt-in");
        assert_eq!(cfg.max_fires_per_prompt, DEFAULT_TODO_GATE_MAX_FIRES);
        assert_eq!(DEFAULT_TODO_GATE_MAX_FIRES, 2);
    }

    #[test]
    fn reminder_policy_default_disables_gate_but_keeps_nudge_and_global_enabled() {
        let policy = ReminderPolicy::default();
        assert!(
            policy.enabled,
            "global system reminders stay enabled by default"
        );
        assert!(
            !policy.todo_gate.enabled,
            "TodoGate ships disabled; remote/local opt-in required"
        );
        assert_eq!(policy.todo_gate.max_fires_per_prompt, 2);
        assert!(policy.todo_nudge.enabled);
    }

    #[test]
    fn todo_gate_enable_does_not_disturb_nudge() {
        let mut policy = ReminderPolicy::default();
        policy.todo_gate.enabled = true;
        assert!(policy.todo_gate.enabled);
        assert!(policy.todo_nudge.enabled, "TodoNudge must stay enabled");
        assert!(policy.enabled, "global enable must stay true");
    }
}
