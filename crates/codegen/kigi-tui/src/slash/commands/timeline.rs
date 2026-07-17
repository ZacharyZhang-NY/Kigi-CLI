//! `/timeline` -- toggle the timeline sidebar (per-turn tick rail).
//!
//! Computes the new value itself and dispatches the typed
//! `Action::SetTimeline(bool)`, mirroring `/timestamps`.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct TimelineCommand;

impl SlashCommand for TimelineCommand {
    fn name(&self) -> &str {
        "timeline"
    }

    fn description(&self) -> &str {
        "Toggle the timeline sidebar"
    }

    /// Minimal mode has no interactive scrollback pane for the rail.
    fn available_in_minimal(&self) -> bool {
        false
    }

    fn usage(&self) -> &str {
        "/timeline"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let new = !crate::appearance::cache::load_show_timeline();
        CommandResult::Action(Action::SetTimeline(new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_available_in_minimal() {
        assert!(!TimelineCommand.available_in_minimal());
    }
}
