//! Pure text-classification helpers shared by the memory flush
//! (`session::helpers::memory_flush`) and dream (`session::memory::dream`)
//! response-processing paths.
//!
//! They live down here rather than in `memory_flush` so that `dream` need not
//! reach *up* into it, which would form a `dream` <-> `memory_flush` cycle.

pub fn has_markdown_headers(text: &str) -> bool {
    text.contains("## ") || text.contains("# ")
}

/// Matches the NO_REPLY convention across separator variants — `"no reply"`,
/// `"no_reply"`, `"no-reply"`, `"NO REPLY"` — by comparing only the
/// lowercased alphanumerics.
pub fn is_no_reply(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    normalized == "noreply"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_no_reply() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("no reply"));
        assert!(is_no_reply("No-Reply"));
        assert!(is_no_reply("noreply"));
        assert!(!is_no_reply("no reply needed"));
        assert!(!is_no_reply("I have things to store"));
    }

    #[test]
    fn test_has_markdown_headers() {
        assert!(has_markdown_headers("## Topic"));
        assert!(has_markdown_headers("# Title\n\nBody"));
        assert!(has_markdown_headers("preamble\n\n## Topic"));
        assert!(!has_markdown_headers("plain text without headers"));
        assert!(!has_markdown_headers("#hashtag without space"));
    }
}
