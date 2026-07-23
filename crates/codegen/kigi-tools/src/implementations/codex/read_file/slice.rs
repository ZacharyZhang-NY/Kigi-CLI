//! Slice-mode reader — exact port of codex `slice::read()`.

pub(crate) const MAX_LINE_LENGTH: usize = 500;

/// Errs when `offset` lies past the last line of the file.
pub(crate) fn read_slice(
    file_bytes: &[u8],
    offset: usize,
    limit: usize,
) -> Result<Vec<String>, String> {
    let mut collected = Vec::new();
    let mut seen = 0usize;

    for raw_line in split_lines(file_bytes) {
        seen += 1;

        if seen < offset {
            continue;
        }
        if collected.len() == limit {
            break;
        }

        let formatted = format_line(raw_line);
        collected.push(format!("L{seen}: {formatted}"));

        if collected.len() == limit {
            break;
        }
    }

    if seen < offset {
        return Err("offset exceeds file length".to_string());
    }

    Ok(collected)
}

fn format_line(bytes: &[u8]) -> String {
    super::text_utils::format_display(bytes)
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return vec![];
    }

    let mut lines = Vec::new();
    let mut start = 0;

    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push(&bytes[start..end]);
            start = i + 1;
        }
    }

    if start < bytes.len() {
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        lines.push(&bytes[start..end]);
    } else if start == bytes.len() && !bytes.is_empty() && bytes[bytes.len() - 1] == b'\n' {
        // Intentionally empty: codex's `read_until(b'\n')` loop consumes the
        // delimiter along with the line before it, so a file ending in `\n`
        // yields no empty final line.
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_requested_range() {
        let content = b"first\nsecond\nthird\nfourth\n";
        let result = read_slice(content, 2, 2).unwrap();
        assert_eq!(result, vec!["L2: second", "L3: third"]);
    }

    #[test]
    fn errors_when_offset_exceeds_length() {
        let content = b"one\ntwo\n";
        let result = read_slice(content, 100, 10);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "offset exceeds file length");
    }

    #[test]
    fn reads_non_utf8_lines() {
        let content = b"\xff\xfe\n";
        let result = read_slice(content, 1, 10).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].contains('\u{FFFD}'));
    }

    #[test]
    fn trims_crlf_endings() {
        let content = b"hello\r\nworld\r\n";
        let result = read_slice(content, 1, 10).unwrap();
        assert_eq!(result, vec!["L1: hello", "L2: world"]);
    }

    #[test]
    fn respects_limit_even_with_more_lines() {
        let content = b"a\nb\nc\nd\ne\n";
        let result = read_slice(content, 1, 3).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result, vec!["L1: a", "L2: b", "L3: c"]);
    }

    #[test]
    fn truncates_lines_longer_than_max_length() {
        let long_line = "x".repeat(600);
        let content = format!("{}\n", long_line);
        let result = read_slice(content.as_bytes(), 1, 10).unwrap();
        assert_eq!(result.len(), 1);
        let expected_content = &long_line[..MAX_LINE_LENGTH];
        assert_eq!(result[0], format!("L1: {}", expected_content));
    }

    #[test]
    fn reads_single_line_no_trailing_newline() {
        let content = b"hello";
        let result = read_slice(content, 1, 10).unwrap();
        assert_eq!(result, vec!["L1: hello"]);
    }

    #[test]
    fn reads_from_offset_1() {
        let content = b"first\nsecond\nthird\n";
        let result = read_slice(content, 1, 10).unwrap();
        assert_eq!(result, vec!["L1: first", "L2: second", "L3: third"]);
    }

    #[test]
    fn empty_file_returns_error() {
        // Codex treats an empty file as zero lines, so even offset=1 is past
        // the end.
        let content = b"";
        let result = read_slice(content, 1, 10);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "offset exceeds file length");
    }

    #[test]
    fn truncation_at_multibyte_char_boundary() {
        // Straddle MAX_LINE_LENGTH with a 2-byte char so the cut lands
        // mid-character.
        let mut s = "a".repeat(498);
        s.push('é');
        s.push('x');
        assert!(s.len() > MAX_LINE_LENGTH);
        let content = format!("{}\n", s);
        let result = read_slice(content.as_bytes(), 1, 10).unwrap();
        let line_content = result[0].strip_prefix("L1: ").unwrap();
        assert!(line_content.len() <= MAX_LINE_LENGTH);
        assert!(line_content.is_char_boundary(line_content.len()));
    }
}
