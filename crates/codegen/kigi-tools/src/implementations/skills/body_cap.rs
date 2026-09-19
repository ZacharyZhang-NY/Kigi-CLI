//! Cap an injected skill body at the file-read token cap.

use crate::implementations::kigi::read_file::{MAX_NUM_BYTES, MAX_NUM_TOKENS, exceeds_read_cap};
use crate::util::truncate::floor_char_boundary;

/// Cuts `body` on a line boundary under the read cap and appends a note; true when it cut.
pub fn cap_skill_body(body: &mut String) -> bool {
    if !exceeds_read_cap(body) {
        return false;
    }
    let note = format!(
        "[Skill body truncated at the {MAX_NUM_TOKENS}-token cap. Read the rest of the \
         skill file with the file read tool using a line offset and limit.]"
    );
    let budget = MAX_NUM_BYTES.saturating_sub(note.len() + 2);
    let end = floor_char_boundary(body, budget);
    let cut = body[..end].rfind('\n').filter(|&i| i > 0).unwrap_or(end);
    body.truncate(cut);
    body.push_str("\n\n");
    body.push_str(&note);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_body_under_cap_untouched() {
        let mut body = "# Small skill\n\nDo the thing.".to_owned();
        assert!(!cap_skill_body(&mut body));
        assert_eq!("# Small skill\n\nDo the thing.", body);
    }

    #[test]
    fn cuts_on_line_boundary_under_cap() {
        let mut body = (1..=1100)
            .map(|n| format!("{n:05} {}", "x".repeat(194)))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(cap_skill_body(&mut body));
        assert!(!exceeds_read_cap(&body));
        assert!(!body.contains("01100 "));
        assert!(body.contains("offset"));
        let (head, _note) = body.rsplit_once("\n\n").expect("note separator");
        let last_line = head.rsplit('\n').next().expect("head has a line");
        assert_eq!(200, last_line.len());
    }

    #[test]
    fn single_long_line_falls_back_to_char_cut() {
        let mut body = "é".repeat(60_000);

        assert!(cap_skill_body(&mut body));
        assert!(!exceeds_read_cap(&body));
        assert!(body.starts_with("éé"));
    }
}
