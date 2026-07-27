//! Swarm mode: a standing instruction to split work across a fleet.
//!
//! The `agent_swarm` tool works with the mode off; what the mode adds is the
//! doctrine — decompose finely, give every member a disjoint scope, do not do
//! the work yourself. Kept as pure state so the turn loop decides when to
//! inject and the session decides when to persist.

use serde::{Deserialize, Serialize};

/// Why the mode is on, which is what decides when it turns off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmTrigger {
    /// `/swarm on` (or a bare `/swarm` toggle): stays until switched off.
    Manual,
    /// `/swarm <task>`: armed for exactly that turn.
    Task,
}

/// The session's swarm-mode state. `None` means off.
///
/// Deliberately NOT persisted, unlike `/goal` and `/graph`: those drive
/// autonomous multi-turn work that is stranded if it is lost, whereas this is
/// a prompt hint whose worst-case recovery is typing `/swarm on` again. The
/// injected doctrine IS durable (it rides the conversation), so a resumed
/// session can read as "off" with the instruction still in context — which is
/// exactly why an explicit `/swarm off` always retracts (see
/// [`SessionActor::apply_swarm_mode`]) rather than trusting a remembered flag
/// that a restore, a compaction or a fork can each falsify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SwarmMode {
    trigger: Option<SwarmTrigger>,
}

impl SwarmMode {
    pub fn is_active(self) -> bool {
        self.trigger.is_some()
    }

    pub fn trigger(self) -> Option<SwarmTrigger> {
        self.trigger
    }

    /// Arms the mode, and does NOTHING if it is already armed.
    ///
    /// Total no-op, not just "don't re-inject": overwriting the trigger would
    /// let the `/swarm <task>` shorthand downgrade a standing `/swarm on` into
    /// a per-turn mode, which then disarms itself at that turn's end — the
    /// user's deliberate choice, silently undone.
    ///
    /// Returns whether the caller should inject the enter reminder.
    pub fn enter(&mut self, trigger: SwarmTrigger) -> bool {
        if self.trigger.is_some() {
            return false;
        }
        self.trigger = Some(trigger);
        true
    }

    /// Disarms. Returns whether the mode had been armed — which is what an
    /// AUTOMATIC expiry keys its retraction off. An explicit `/swarm off` must
    /// retract regardless (see the type docs).
    pub fn exit(&mut self) -> bool {
        self.trigger.take().is_some()
    }

    /// Whether a turn ending now should disarm the mode.
    ///
    /// Only the `Task` trigger auto-exits: `/swarm on` is a standing choice the
    /// user made and a turn boundary is not a reason to undo it.
    pub fn expires_at_turn_end(self) -> bool {
        self.trigger == Some(SwarmTrigger::Task)
    }
}

/// The doctrine injected when the mode is armed.
///
/// Deliberately short: it is re-read on every turn it is live, and the tool's
/// own description already carries the mechanics.
pub const SWARM_ENTER_REMINDER: &str = "\
Swarm mode is on. Explore only as far as you must to identify the work, then \
split it: use the agent_swarm tool with one item per independent scope rather \
than doing the work yourself. Decompose finely — do not try to conserve \
members. Every member must own a disjoint scope; members share one working \
tree, so two members told to touch the same file will corrupt each other. \
Read-only scopes may overlap. If the work genuinely does not split, say so and \
carry on alone.";

/// Injected when the mode is switched off mid-conversation, so the earlier
/// doctrine does not keep steering the model.
/// Deliberately as emphatic as the enter doctrine it revokes: a one-line
/// "mode is off" is the weaker of the two texts in context and the likelier to
/// be summarised away, leaving the fan-out directives still steering.
pub const SWARM_EXIT_REMINDER: &str = "\
Swarm mode is off. The swarm instructions above no longer apply — you are not \
required to split work across subagents, and you should not decompose a task \
just because they said to. Decide how to approach each new request from the \
request itself. Delegate only where it clearly helps.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_session_has_the_mode_off() {
        let mode = SwarmMode::default();
        assert!(!mode.is_active());
        assert!(!mode.expires_at_turn_end());
    }

    #[test]
    fn arming_asks_for_the_doctrine_once_not_once_per_command() {
        let mut mode = SwarmMode::default();
        assert!(mode.enter(SwarmTrigger::Manual), "first arm injects");
        assert!(
            !mode.enter(SwarmTrigger::Manual),
            "a repeated /swarm on must not stack a second copy"
        );
        assert!(mode.is_active());
    }

    #[test]
    fn only_the_task_trigger_expires_at_a_turn_boundary() {
        let mut manual = SwarmMode::default();
        manual.enter(SwarmTrigger::Manual);
        assert!(
            !manual.expires_at_turn_end(),
            "/swarm on is a standing choice, not a per-turn one"
        );

        let mut task = SwarmMode::default();
        task.enter(SwarmTrigger::Task);
        assert!(task.expires_at_turn_end());
    }

    #[test]
    fn exiting_retracts_exactly_once() {
        let mut mode = SwarmMode::default();
        mode.enter(SwarmTrigger::Manual);
        assert!(mode.exit(), "the live doctrine must be retracted");
        assert!(!mode.is_active());
        assert!(
            !mode.exit(),
            "a second /swarm off has nothing left to retract"
        );
    }

    /// `/swarm <task>` under a standing `/swarm on` must seed the turn and
    /// nothing more — it must not convert the standing mode into a per-turn
    /// one that disarms itself when that turn ends.
    #[test]
    fn the_task_shorthand_never_downgrades_a_standing_mode() {
        let mut mode = SwarmMode::default();
        mode.enter(SwarmTrigger::Manual);
        assert!(
            !mode.enter(SwarmTrigger::Task),
            "the doctrine is already in the conversation"
        );
        assert_eq!(mode.trigger(), Some(SwarmTrigger::Manual));
        assert!(
            !mode.expires_at_turn_end(),
            "the user's standing /swarm on must survive the turn"
        );
    }

    /// An automatic expiry has nothing to retract once the mode is already
    /// off; only an explicit `/swarm off` retracts unconditionally, and that
    /// rule lives at the call site, not here.
    #[test]
    fn exit_reports_whether_it_actually_disarmed_something() {
        let mut armed = SwarmMode::default();
        armed.enter(SwarmTrigger::Task);
        assert!(armed.exit());

        let mut idle = SwarmMode::default();
        assert!(!idle.exit());
    }
}
