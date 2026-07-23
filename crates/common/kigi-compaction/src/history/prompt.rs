//! Prompt construction for conversation history compaction.
//!
//! The developer and user prompts are intentionally identical so the model
//! sees the instructions on both turns.

use anyhow::Result;

pub fn format_compaction_developer_prompt() -> Result<String> {
    Ok(include_str!("../templates/compaction_developer_prompt.txt").to_string())
}

pub fn format_compaction_user_prompt() -> Result<String> {
    Ok(include_str!("../templates/compaction_user_prompt.txt").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_non_empty() {
        let dev = format_compaction_developer_prompt().expect("dev prompt renders");
        assert!(!dev.trim().is_empty(), "developer prompt empty");
        let user = format_compaction_user_prompt().expect("user prompt renders");
        assert!(!user.trim().is_empty(), "user prompt empty");
    }

    /// The two templates are separate files that must stay byte-identical; this
    /// catches drift when only one is edited.
    #[test]
    fn compaction_prompts_match() {
        let dev = format_compaction_developer_prompt().expect("dev prompt renders");
        let user = format_compaction_user_prompt().expect("user prompt renders");
        assert_eq!(
            dev, user,
            "compaction_developer_prompt.txt and compaction_user_prompt.txt must stay in sync"
        );
    }
}
