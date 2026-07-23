use super::types::{BATCH_TRUNCATION_LIMIT, BUFFER_CAP_BYTES, LINE_TRUNCATION_LIMIT};
use crate::util::floor_char_boundary;

/// Splits raw stdout chunks into complete lines, buffering partial lines
/// across chunks. Individual lines are truncated at `LINE_TRUNCATION_LIMIT`
/// chars and the buffer is capped at `BUFFER_CAP_BYTES`, keeping the tail.
#[derive(Default)]
pub struct LineProcessor {
    buffer: Vec<u8>,
}

impl LineProcessor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);

        if self.buffer.len() > BUFFER_CAP_BYTES {
            let start = self.buffer.len() - BUFFER_CAP_BYTES;
            self.buffer = self.buffer[start..].to_vec();
        }

        let mut lines = Vec::new();
        while let Some(nl_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let raw = self.buffer.drain(..=nl_pos).collect::<Vec<_>>();
            let text = String::from_utf8_lossy(&raw).trim().to_string();
            if text.is_empty() {
                continue;
            }
            lines.push(truncate_line(&text));
        }
        lines
    }

    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.buffer);
        let text = String::from_utf8_lossy(&raw).trim().to_string();
        if text.is_empty() {
            return None;
        }
        Some(truncate_line(&text))
    }
}

fn truncate_line(line: &str) -> String {
    if line.len() > LINE_TRUNCATION_LIMIT {
        let boundary = floor_char_boundary(line, LINE_TRUNCATION_LIMIT);
        format!("{}...(truncated)", &line[..boundary])
    } else {
        line.to_string()
    }
}

pub fn batch_lines(lines: &[String]) -> String {
    let joined = lines.join("\n");
    if joined.len() > BATCH_TRUNCATION_LIMIT {
        let boundary = floor_char_boundary(&joined, BATCH_TRUNCATION_LIMIT);
        format!("{}\n...(truncated)", &joined[..boundary])
    } else {
        joined
    }
}

/// Sanitize a model-supplied monitor description for embedding in the
/// `<monitor-event …>` attribute and in line labels: `"` would break the
/// attribute / the parser's `" task_id="` anchor, and newlines would break
/// the single-line opening-tag shape (`>\n` anchor) and label lines.
pub fn sanitize_monitor_description(description: &str) -> String {
    description.replace('"', "'").replace(['\n', '\r'], " ")
}

/// Wrap event text in XML tags for the LLM conversation.
pub fn wrap_monitor_event(description: &str, event_text: &str, task_id: &str) -> String {
    let description = sanitize_monitor_description(description);
    format!(
        "<monitor-event description=\"{description}\" task_id=\"{task_id}\">\n\
         {event_text}\n\
         </monitor-event>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_sanitizes_description() {
        let wrapped = wrap_monitor_event("watch \"prod\"\nlogs", "line", "t-1");
        assert!(
            wrapped
                .starts_with("<monitor-event description=\"watch 'prod' logs\" task_id=\"t-1\">"),
            "{wrapped}"
        );
    }

    #[test]
    fn push_single_line() {
        let mut proc = LineProcessor::new();
        let lines = proc.push(b"hello world\n");
        assert_eq!(lines, vec!["hello world"]);
    }

    #[test]
    fn push_multiple_lines() {
        let mut proc = LineProcessor::new();
        let lines = proc.push(b"line1\nline2\nline3\n");
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn partial_line_buffered() {
        let mut proc = LineProcessor::new();
        let lines = proc.push(b"partial");
        assert!(lines.is_empty());
        let lines = proc.push(b" line\n");
        assert_eq!(lines, vec!["partial line"]);
    }

    #[test]
    fn empty_lines_skipped() {
        let mut proc = LineProcessor::new();
        let lines = proc.push(b"hello\n\n\nworld\n");
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn long_line_truncated() {
        let mut proc = LineProcessor::new();
        let long = "x".repeat(600);
        let input = format!("{long}\n");
        let lines = proc.push(input.as_bytes());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("...(truncated)"));
        assert!(lines[0].len() < 600);
    }

    #[test]
    fn buffer_cap_enforced() {
        let mut proc = LineProcessor::new();
        let big = vec![b'a'; BUFFER_CAP_BYTES + 1000];
        proc.push(&big);
        assert!(proc.buffer.len() <= BUFFER_CAP_BYTES);
    }

    #[test]
    fn flush_returns_partial() {
        let mut proc = LineProcessor::new();
        proc.push(b"no newline");
        let flushed = proc.flush();
        assert_eq!(flushed, Some("no newline".to_string()));
    }

    #[test]
    fn flush_empty_returns_none() {
        let mut proc = LineProcessor::new();
        assert!(proc.flush().is_none());
    }

    #[test]
    fn batch_lines_joins() {
        let lines = vec!["line1".into(), "line2".into(), "line3".into()];
        assert_eq!(batch_lines(&lines), "line1\nline2\nline3");
    }

    #[test]
    fn batch_lines_truncates_at_limit() {
        let long_line = "x".repeat(2000);
        let lines = vec![long_line.clone(), long_line];
        let batched = batch_lines(&lines);
        assert!(batched.ends_with("...(truncated)"));
        assert!(batched.len() < 5000);
    }

    #[test]
    fn truncate_line_multibyte_no_panic() {
        // 3-byte UTF-8 chars — the truncation boundary may land mid-char
        let line = "\u{4e16}\u{754c}".repeat(200);
        let truncated = truncate_line(&line);
        assert!(truncated.ends_with("...(truncated)"));
        let _ = truncated.as_bytes();
    }

    #[test]
    fn truncate_line_emoji_no_panic() {
        let line = "\u{1F600}".repeat(200);
        let truncated = truncate_line(&line);
        assert!(truncated.ends_with("...(truncated)"));
    }

    #[test]
    fn batch_lines_multibyte_no_panic() {
        let long_line = "\u{4e16}\u{754c}".repeat(1500);
        let lines = vec![long_line];
        let batched = batch_lines(&lines);
        assert!(batched.ends_with("...(truncated)"));
    }

    #[test]
    fn xml_wrapping() {
        let result = wrap_monitor_event("errors in log", "ERROR: disk full", "task-123");
        assert_eq!(
            result,
            "<monitor-event description=\"errors in log\" task_id=\"task-123\">\n\
             ERROR: disk full\n\
             </monitor-event>"
        );
    }
}
