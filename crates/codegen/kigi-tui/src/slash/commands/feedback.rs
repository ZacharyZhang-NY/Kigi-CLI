//! `/feedback` -- open the Kigi GitHub issues page.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Where feedback goes: the project's own issue tracker. Kigi is a community
/// build, so its feedback belongs on its GitHub repo — mirroring the official
/// kimi-cli, whose `/feedback` opens its repo's issues page.
pub const FEEDBACK_ISSUES_URL: &str = "https://github.com/ZacharyZhang-NY/Kigi-CLI/issues";

/// Open the Kigi issue tracker in the browser.
pub struct FeedbackCommand;

impl SlashCommand for FeedbackCommand {
    fn name(&self) -> &str {
        "feedback"
    }

    fn description(&self) -> &str {
        "Report feedback on the Kigi GitHub issues page"
    }

    fn usage(&self) -> &str {
        "/feedback"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenUrl(FEEDBACK_ISSUES_URL.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    static DEFAULT_BUNDLE_STATE: crate::app::bundle::BundleState =
        crate::app::bundle::BundleState {
            has_cache: false,
            version: String::new(),
            personas: Vec::new(),
            roles: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            persona_details: Vec::new(),
            role_details: Vec::new(),
        };

    fn make_ctx<'a>(models: &'a ModelState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn feedback_opens_github_issues() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        match FeedbackCommand.run(&mut ctx, "") {
            CommandResult::Action(Action::OpenUrl(url)) => {
                assert_eq!(url, FEEDBACK_ISSUES_URL);
            }
            other => panic!("expected OpenUrl, got {other:?}"),
        }
    }

    #[test]
    fn feedback_ignores_stray_args() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        assert!(matches!(
            FeedbackCommand.run(&mut ctx, "some typed text"),
            CommandResult::Action(Action::OpenUrl(_))
        ));
    }

    #[test]
    fn feedback_metadata() {
        let cmd = FeedbackCommand;
        assert_eq!(cmd.name(), "feedback");
        assert!(!cmd.takes_args());
    }
}
