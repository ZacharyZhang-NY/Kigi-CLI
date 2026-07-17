//! `/usage` -- display Kimi API usage and quota information.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Display API usage and quota information.
pub struct UsageCommand;

impl SlashCommand for UsageCommand {
    fn name(&self) -> &str {
        "usage"
    }

    /// `/cost` is the minimal-mode name for the same usage summary: it
    /// commits a usage system block rather than opening a pane, so it's
    /// an alias rather than a separate command.
    fn aliases(&self) -> &[&str] {
        &["cost"]
    }

    fn description(&self) -> &str {
        "Display API usage and quota information"
    }

    fn usage(&self) -> &str {
        "/usage"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ShowUsage)
    }
}
