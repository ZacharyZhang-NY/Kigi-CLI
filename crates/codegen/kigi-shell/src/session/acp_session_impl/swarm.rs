//! Swarm-mode seam: arming/disarming the mode and the reminder it injects.

use super::*;
use crate::session::swarm_mode::{SWARM_ENTER_REMINDER, SWARM_EXIT_REMINDER, SwarmTrigger};

impl SessionActor {
    /// Arms (`Some(trigger)`) or disarms (`None`) swarm mode, injecting or
    /// retracting the doctrine exactly once, and returns the line to show.
    pub(crate) fn apply_swarm_mode(self: &Arc<Self>, trigger: Option<SwarmTrigger>) -> String {
        let mut mode = self.swarm_mode.get();
        let message = match trigger {
            Some(trigger) => {
                if mode.enter(trigger) {
                    self.inject_swarm_reminder(SWARM_ENTER_REMINDER);
                }
                "Swarm mode on: work will be split across a fleet of subagents. \
                 `/swarm off` to stop."
            }
            None => {
                // Unconditional, unlike the automatic expiry: the doctrine
                // rides the conversation and therefore survives a resume, a
                // fork and a compaction that the in-memory flag does not. If
                // the user explicitly asks for it off, the retraction has to
                // reach the model even when this session never saw it armed.
                mode.exit();
                self.inject_swarm_reminder(SWARM_EXIT_REMINDER);
                "Swarm mode off."
            }
        };
        self.swarm_mode.set(mode);
        message.to_string()
    }

    /// A turn-scoped guard that disarms a per-turn swarm mode however the turn
    /// ends.
    ///
    /// The post-loop call site is not enough: a user interrupt ABORTS the turn
    /// future (`cancel_running_task` → `JoinHandle::abort`), dropping it at its
    /// current await point, and several `?` paths return before the loop's end.
    /// Each of those leaks a `/swarm <task>` mode into the user's next,
    /// unrelated prompt. The goal engine hit the same class and compensates
    /// inside the cancel path; a guard is the version that cannot be forgotten
    /// at a new exit.
    pub(crate) fn swarm_turn_guard(self: &Arc<Self>) -> SwarmTurnGuard {
        SwarmTurnGuard {
            session: self.clone(),
        }
    }

    /// Disarms at a turn boundary when the trigger was per-turn.
    pub(crate) fn expire_swarm_mode_at_turn_end(self: &Arc<Self>) {
        if !self.swarm_mode.get().expires_at_turn_end() {
            return;
        }
        let mut mode = self.swarm_mode.get();
        if mode.exit() {
            self.inject_swarm_reminder(SWARM_EXIT_REMINDER);
        }
        self.swarm_mode.set(mode);
    }

    /// The doctrine rides the session's existing `<system-reminder>` channel,
    /// so it is tagged the same way every other reminder is and needs no
    /// second injection path of its own.
    fn inject_swarm_reminder(self: &Arc<Self>, text: &str) {
        self.push_system_reminder(text);
    }
}

/// Runs [`SessionActor::expire_swarm_mode_at_turn_end`] on every turn exit,
/// including an aborted future.
pub(crate) struct SwarmTurnGuard {
    session: Arc<SessionActor>,
}

impl Drop for SwarmTurnGuard {
    fn drop(&mut self) {
        self.session.expire_swarm_mode_at_turn_end();
    }
}
