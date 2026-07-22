//! `/login` -- pick a provider to log in or re-authenticate with.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Pick a provider to log in with (connected ones are marked)"
    }

    fn usage(&self) -> &str {
        "/login"
    }

    /// Opens the provider picker rather than auto-starting a flow: the user
    /// chooses a row there, and already-connected providers show a green
    /// "connected" badge.
    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenLoginPicker)
    }
}
