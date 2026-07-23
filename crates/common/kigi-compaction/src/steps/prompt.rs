//! Prompt construction for **steps** compaction. Templates live in the
//! crate-root `templates/`.

use crate::prompt::CompactionPrompt;

/// Short prompts focused on summarising tool-call history mid-task: the
/// assistant has already done several steps of work and needs context freed up
/// to continue.
pub fn format_compaction_prompt() -> CompactionPrompt {
    CompactionPrompt {
        system: include_str!("../templates/intra_compaction_system.txt").to_string(),
        user: include_str!("../templates/intra_compaction_user.txt").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_non_empty() {
        let p = format_compaction_prompt();
        assert!(!p.system.trim().is_empty(), "system prompt empty");
        assert!(!p.user.trim().is_empty(), "user prompt empty");
    }
}
