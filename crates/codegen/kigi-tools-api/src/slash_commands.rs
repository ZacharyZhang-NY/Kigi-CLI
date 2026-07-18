//! Canonical slash-command wording (`/loop`, `/goal`), shared by every
//! front-end (shell/pager and other hosts) so expansions cannot drift.

/// Canonical tool name advertised by the scheduler create tool. Gating code
/// (shell `CommandAvailability`, pager `required_tools`, host command lists)
/// keys `/loop` availability on this name.
pub const SCHEDULER_CREATE_TOOL_NAME: &str = "scheduler_create";

/// Usage hint shown when `/loop` is invoked with no arguments.
pub fn loop_usage_message() -> &'static str {
    "Usage: /loop [interval] <prompt>\n\
     Example: /loop 30m check deploy status\n\
     Example: /loop check deploy status every hour\n\n\
     Tell me how often it should run (e.g. 30m, 1 hour, every 2 days)."
}

/// Build the model instruction that `/loop` expands into for `args`.
///
/// The model, not brittle host parsing, turns the request into the
/// `scheduler_create` interval, accepting every natural phrasing and erroring
/// on bad input rather than silently defaulting. See [`loop_usage_message`].
pub fn loop_schedule_instruction(args: &str) -> String {
    format!(
        "# /loop -- schedule a recurring prompt\n\n\
         Parse the input below into an interval and a prompt, then schedule it with scheduler_create.\n\n\
         ## Deriving the interval\n\
         Read how often to run from the user's request — however they phrase it — and convert it\n\
         to a compact `<number><unit>` string, where unit is one of `s` (seconds), `m` (minutes),\n\
         `h` (hours), or `d` (days). The interval may appear at the start or end of the request;\n\
         extract it and use the remaining text as the prompt.\n\n\
         The minimum interval is 60 seconds; shorter values are raised to 60s, so tell the user if that applies.\n\n\
         If the request contains no interval at all, ask the user how often it should run before\n\
         scheduling. Do NOT invent or assume a default interval.\n\n\
         ## Action\n\
         1. Call scheduler_create with: interval (the compact string you derived), prompt,\n\
            recurring: true, fire_immediately: true. If the interval is unparseable, the tool\n\
            returns an error — fix the interval string rather than guessing.\n\
         2. Confirm: what's scheduled, the cadence, that it auto-expires after 7 days,\n\
            and that they can cancel with scheduler_delete (include the job ID).\n\
         3. Do NOT execute the prompt inline. The scheduler will fire it immediately.\n\n\
         ## Input\n\
         {args}"
    )
}

pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";

pub const GOAL_COMMAND_NAME: &str = "goal";

/// Bare subcommand tokens reserved for goal lifecycle control rather than
/// being treated as an objective, matching the shell's /goal grammar.
pub const GOAL_RESERVED_SUBCOMMANDS: &[&str] = &["status", "pause", "resume", "clear", "edit"];

pub fn goal_usage_message() -> &'static str {
    "Usage: /goal <objective>\n\
     Set an objective to work toward until it is complete."
}

pub fn goal_instruction(objective: &str) -> String {
    format!(
        "# /goal -- pursue an objective\n\n\
         A goal has been set: {objective}\n\n\
         Work directly on this goal and carry it as far as you can. Deliver \
         everything the user asked for yourself: no follow-up questions, no \
         manual steps left for the user. If the conversation continues, keep \
         pursuing the goal until it is complete.\n\n\
         TRACKING: break the objective into concrete steps and track them \
         (use your todo tool if one is available), marking each done as you \
         finish it.\n\n\
         VERIFY AS YOU GO: test each change on the real path before moving on. \
         A completion claim must be backed by evidence produced in this \
         session, not assumptions.\n\n\
         Call update_goal(completed: true, message: \"summary\") ONLY when the \
         goal is fully achieved. Call update_goal(blocked_reason: \"reason\") \
         only when truly stuck after 3+ consecutive failed attempts at the \
         same problem. Call update_goal(message: \"status note\") to log \
         progress along the way. If update_goal returns an error, continue \
         working the goal and report status in your reply instead.\n\n\
         Start now."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_carries_args_and_contract_tokens() {
        let text = loop_schedule_instruction("every 30 minutes do x");
        assert!(text.contains("every 30 minutes do x"));
        assert!(text.contains("<number><unit>"));
        assert!(text.contains("ask the user how often"));
        assert!(!text.contains("10m"), "no host-side default interval");
    }

    #[test]
    fn goal_instruction_carries_objective_and_contract_tokens() {
        let text = goal_instruction("ship the widget");
        assert!(text.contains("ship the widget"));
        assert!(text.contains("update_goal(completed: true"));
        assert!(text.contains("blocked_reason"));
        assert!(text.contains("If update_goal returns an error"));
        assert!(
            !text.contains("system-reminder"),
            "expansions ride as user messages and must not claim reminder authority"
        );
        assert!(goal_usage_message().contains("Usage: /goal"));
    }

    #[test]
    fn usage_message_has_no_default_claim() {
        assert!(loop_usage_message().contains("Usage: /loop"));
        assert!(!loop_usage_message().contains("10m"));
    }
}
