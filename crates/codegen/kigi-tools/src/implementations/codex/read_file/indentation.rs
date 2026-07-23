//! Indentation-mode reader — exact port of codex `indentation::*`.
//!
//! Loads all file lines, computes effective indents (blank lines inherit
//! from the previous non-blank line), then expands bidirectionally from an
//! anchor line. Sibling filtering and header-comment inclusion happen inline
//! during that expansion rather than as separate passes.

use std::collections::VecDeque;

use super::text_utils::format_display;

const TAB_WIDTH: usize = 4;

/// Prefixes that make a line eligible for `include_header`.
const COMMENT_PREFIXES: &[&str] = &["#", "//", "--"];

/// Mirrors codex `IndentationModeOptions`.
#[derive(Debug, Clone)]
pub(crate) struct IndentationOptions {
    pub anchor_line: Option<usize>,
    pub max_levels: usize,
    pub include_siblings: bool,
    pub include_header: bool,
    pub max_lines: Option<usize>,
}

/// Matches codex `LineRecord`. Classification (`trimmed`, `is_blank`,
/// `is_comment`) reads `raw`; only output formatting reads `display`.
#[derive(Debug)]
struct LineRecord {
    /// 1-indexed line number.
    number: usize,
    /// Untruncated line content (UTF-8 lossy).
    raw: String,
    /// Line content truncated at MAX_LINE_LENGTH.
    display: String,
    /// Leading spaces, counting each tab as TAB_WIDTH.
    indent: usize,
}

impl LineRecord {
    fn trimmed(&self) -> &str {
        self.raw.trim_start()
    }

    fn is_blank(&self) -> bool {
        self.trimmed().is_empty()
    }

    fn is_comment(&self) -> bool {
        let t = self.raw.trim();
        COMMENT_PREFIXES.iter().any(|p| t.starts_with(p))
    }
}

fn collect_lines(bytes: &[u8]) -> Vec<LineRecord> {
    if bytes.is_empty() {
        return vec![];
    }

    let mut records = Vec::new();
    let mut line_num = 0usize;
    let mut start = 0;

    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            line_num += 1;
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            let raw_bytes = &bytes[start..end];
            let raw = String::from_utf8_lossy(raw_bytes).into_owned();
            let display = format_display(raw_bytes);
            let indent = measure_indent(&raw);
            records.push(LineRecord {
                number: line_num,
                raw,
                display,
                indent,
            });
            start = i + 1;
        }
    }

    // Trailing content after the last \n, i.e. a file with no final newline.
    if start < bytes.len() {
        line_num += 1;
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        let raw_bytes = &bytes[start..end];
        let raw = String::from_utf8_lossy(raw_bytes).into_owned();
        let display = format_display(raw_bytes);
        let indent = measure_indent(&raw);
        records.push(LineRecord {
            number: line_num,
            raw,
            display,
            indent,
        });
    }

    records
}

fn measure_indent(line: &str) -> usize {
    let mut indent = 0;
    for ch in line.chars() {
        match ch {
            ' ' => indent += 1,
            '\t' => indent += TAB_WIDTH,
            _ => break,
        }
    }
    indent
}

/// Blank lines inherit the indent of the previous non-blank line. The result
/// is parallel to `records`.
fn compute_effective_indents(records: &[LineRecord]) -> Vec<usize> {
    let mut effective = Vec::with_capacity(records.len());
    let mut last_non_blank_indent = 0usize;

    for rec in records {
        if rec.is_blank() {
            effective.push(last_non_blank_indent);
        } else {
            last_non_blank_indent = rec.indent;
            effective.push(rec.indent);
        }
    }

    effective
}

/// Read a block of lines by expanding outward from an anchor line, following
/// the indentation structure around it.
///
/// Ported from codex `indentation::read_block`: one loop drives two cursors
/// (`i` upward, `j` downward) that alternate, with sibling filtering and
/// header-comment inclusion applied inline rather than as later passes.
pub(crate) fn read_block(
    bytes: &[u8],
    offset: usize,
    limit: usize,
    options: IndentationOptions,
) -> Result<Vec<String>, String> {
    let collected = collect_lines(bytes);

    if collected.is_empty() {
        return Ok(vec![]);
    }

    let anchor = options.anchor_line.unwrap_or(offset);

    if anchor == 0 || anchor > collected.len() {
        return Err("anchor_line exceeds file length".to_string());
    }

    let effective = compute_effective_indents(&collected);

    let guard_limit = options.max_lines.unwrap_or(limit);
    if guard_limit == 0 {
        return Err("max_lines must be greater than zero".to_string());
    }

    let final_limit = limit.min(guard_limit).min(collected.len());

    // anchor is 1-indexed.
    let anchor_idx = anchor - 1;
    let anchor_indent = effective[anchor_idx];

    let min_indent = if options.max_levels == 0 {
        0
    } else {
        anchor_indent.saturating_sub(options.max_levels * TAB_WIDTH)
    };

    if final_limit == 1 {
        let rec = &collected[anchor_idx];
        return Ok(vec![format!("L{}: {}", rec.number, rec.display)]);
    }

    // Interleaved bidirectional expansion, per codex lines 293-357: both
    // cursors are tried on every iteration, up first, and the loop ends once
    // neither direction contributed a line.

    let mut out: VecDeque<usize> = VecDeque::new();
    out.push_back(anchor_idx);

    // `i` is signed so that -1 can mark the upward cursor exhausted; `j`
    // reaching `n` marks the downward one exhausted.
    let mut i: isize = anchor_idx as isize - 1;
    let mut j: usize = anchor_idx + 1;
    let n = collected.len();

    // Boundary-level lines accepted in each direction.
    let mut i_counter_min_indent: usize = 0;
    let mut j_counter_min_indent: usize = 0;

    while out.len() < final_limit {
        let mut progressed = 0usize;

        if i >= 0 {
            let added = expand_up(
                &collected,
                &effective,
                &mut out,
                &mut i,
                min_indent,
                options.include_siblings,
                options.include_header,
                &mut i_counter_min_indent,
            );
            if added {
                progressed += 1;
            }
            // Codex bails out here without trying the downward cursor.
            if out.len() >= final_limit {
                break;
            }
        }

        if j < n {
            let added = expand_down(
                &effective,
                &mut out,
                &mut j,
                n,
                min_indent,
                options.include_siblings,
                &mut j_counter_min_indent,
            );
            if added {
                progressed += 1;
            }
        }

        if progressed == 0 {
            break;
        }
    }

    trim_empty_lines(&collected, &mut out);

    let lines: Vec<String> = out
        .iter()
        .map(|&idx| {
            let rec = &collected[idx];
            format!("L{}: {}", rec.number, rec.display)
        })
        .collect();

    Ok(lines)
}

/// Advance the upward cursor one step, returning true only if the line
/// survived — a line that is pushed and then reverted counts as no gain.
///
/// Codex (lines 296-320) pushes the candidate before deciding whether the
/// sibling filter rejects it, so the revert pops the line just pushed.
#[allow(clippy::too_many_arguments)]
fn expand_up(
    collected: &[LineRecord],
    effective: &[usize],
    out: &mut VecDeque<usize>,
    i: &mut isize,
    min_indent: usize,
    include_siblings: bool,
    include_header: bool,
    counter: &mut usize,
) -> bool {
    if *i < 0 {
        return false;
    }

    let iu = *i as usize;
    let eff = effective[iu];

    if eff < min_indent {
        *i = -1;
        return false;
    }

    // Push first (codex line 300), filter afterwards.
    out.push_front(iu);
    *i -= 1;

    if eff == min_indent && !include_siblings {
        let allow_header_comment = include_header && collected[iu].is_comment();
        let can_take_line = allow_header_comment || *counter == 0;
        if can_take_line {
            *counter += 1;
        } else {
            out.pop_front();
            *i = -1;
            return false;
        }
    }

    true
}

/// Advance the downward cursor one step, returning true only if the line
/// survived — a line that is pushed and then reverted counts as no gain.
///
/// Codex (lines 332-348) pushes the candidate before deciding whether the
/// sibling filter rejects it, so the revert pops the line just pushed.
fn expand_down(
    effective: &[usize],
    out: &mut VecDeque<usize>,
    j: &mut usize,
    n: usize,
    min_indent: usize,
    include_siblings: bool,
    counter: &mut usize,
) -> bool {
    if *j >= n {
        return false;
    }

    let ju = *j;
    let eff = effective[ju];

    if eff < min_indent {
        *j = n;
        return false;
    }

    // Push first (codex line 334), filter afterwards.
    out.push_back(ju);
    *j += 1;

    if eff == min_indent && !include_siblings {
        if *counter > 0 {
            // A second boundary-level line ends the downward walk, but codex
            // line 346 counts it anyway.
            out.pop_back();
            *j = n;
            *counter += 1;
            return false;
        }
        *counter += 1;
    }

    true
}

fn trim_empty_lines(records: &[LineRecord], deque: &mut VecDeque<usize>) {
    while let Some(&idx) = deque.front() {
        if records[idx].is_blank() {
            deque.pop_front();
        } else {
            break;
        }
    }
    while let Some(&idx) = deque.back() {
        if records[idx].is_blank() {
            deque.pop_back();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_opts(
        anchor_line: Option<usize>,
        max_levels: usize,
        include_siblings: bool,
        include_header: bool,
        max_lines: Option<usize>,
    ) -> IndentationOptions {
        IndentationOptions {
            anchor_line,
            max_levels,
            include_siblings,
            include_header,
            max_lines,
        }
    }

    #[test]
    fn captures_function_block_with_limit() {
        // anchor=2 (x = 1, indent 4), max_levels=1, so min_indent = 4-4 = 0 and
        // every line is reachable. Each direction meets only one boundary-level
        // line (def foo upward, def bar downward), so nothing is filtered out.
        let content =
            b"def foo():\n    x = 1\n    y = 2\n    return x + y\n\ndef bar():\n    pass\n";

        let opts_full = make_opts(Some(2), 1, false, true, None);
        let result_full = read_block(content, 1, 2000, opts_full).unwrap();
        assert!(result_full.iter().any(|l| l.contains("def foo():")));
        assert!(result_full.iter().any(|l| l.contains("x = 1")));
        assert!(result_full.iter().any(|l| l.contains("return x + y")));

        let opts_limited = make_opts(Some(2), 1, false, true, Some(4));
        let result = read_block(content, 1, 4, opts_limited).unwrap();
        assert_eq!(
            result,
            vec![
                "L1: def foo():",
                "L2:     x = 1",
                "L3:     y = 2",
                "L4:     return x + y",
            ]
        );
    }

    #[test]
    fn expands_to_parent_class() {
        // L1: class MyClass:   (indent 0)
        // L2:     def method(self):  (indent 4)
        // L3:         x = 1    (indent 8)  ← ANCHOR
        // L4:         y = 2    (indent 8)
        // L5:         return x + y  (indent 8)
        // L6: (blank, effective=8)
        // L7:     def other(self):  (indent 4)
        // L8:         pass     (indent 8)
        // anchor=3, max_levels=2, so min_indent = 8-8 = 0 and every line is
        // reachable; the one boundary line each direction meets is kept.
        let content = b"class MyClass:\n    def method(self):\n        x = 1\n        y = 2\n        return x + y\n\n    def other(self):\n        pass\n";
        let opts = make_opts(Some(3), 2, false, true, None);
        let result = read_block(content, 1, 2000, opts).unwrap();

        assert_eq!(result[0], "L1: class MyClass:");
        assert_eq!(result[1], "L2:     def method(self):");
        assert_eq!(result[2], "L3:         x = 1");
        assert_eq!(result[3], "L4:         y = 2");
        assert_eq!(result[4], "L5:         return x + y");
    }

    #[test]
    fn sibling_filter_at_nonzero_min_indent() {
        // Layout:
        //   L1:  class C:               (indent 0)
        //   L2:      def a(self):       (indent 4, boundary)
        //   L3:          pass            (indent 8)
        //   L4:      def b(self):       (indent 4, boundary)
        //   L5:          pass            (indent 8)
        //   L6:      def anchor(self):  (indent 4, boundary)
        //   L7:          x = 1          (indent 8) ← ANCHOR
        //   L8:      def d(self):       (indent 4, boundary)
        //   L9:          pass            (indent 8)
        //   L10:     def e(self):       (indent 4, boundary)
        //   L11:         pass            (indent 8)
        //
        // anchor=7, max_levels=1, min_indent = 8-4 = 4.
        let content = b"\
class C:
    def a(self):
        pass
    def b(self):
        pass
    def anchor(self):
        x = 1
    def d(self):
        pass
    def e(self):
        pass
";
        // Without siblings each direction accepts exactly one boundary line:
        // upward L6 (def anchor), downward L8 (def d). The next boundary line
        // in each direction (L4, L10) is pushed, then reverted, ending that
        // cursor — so the block spans L5..L9.
        let opts_no_sibs = make_opts(Some(7), 1, false, true, None);
        let result_no_sibs = read_block(content, 1, 2000, opts_no_sibs).unwrap();

        assert_eq!(
            result_no_sibs,
            vec![
                "L5:         pass",
                "L6:     def anchor(self):",
                "L7:         x = 1",
                "L8:     def d(self):",
                "L9:         pass",
            ]
        );

        // With siblings, every method at indent 4 survives the filter.
        let opts_sibs = make_opts(Some(7), 1, true, true, None);
        let result_sibs = read_block(content, 1, 2000, opts_sibs).unwrap();

        assert!(result_sibs.iter().any(|l| l.contains("def a(")));
        assert!(result_sibs.iter().any(|l| l.contains("def anchor")));
        assert!(result_sibs.iter().any(|l| l.contains("def e(")));
        assert!(result_sibs.len() > result_no_sibs.len());
    }

    #[test]
    fn include_header_adds_comments() {
        // L1: # Helper function   (indent 0, comment)
        // L2: # for computation   (indent 0, comment)
        // L3: def compute(x):     (indent 0)
        // L4:     return x * 2    (indent 4) ← ANCHOR
        //
        // anchor=4, max_levels=1, min_indent = 4-4 = 0. The comments sit at the
        // boundary indent, so only include_header lets them through.
        let content = b"# Helper function\n# for computation\ndef compute(x):\n    return x * 2\n";

        let opts_header = make_opts(Some(4), 1, false, true, None);
        let result = read_block(content, 1, 2000, opts_header).unwrap();
        assert_eq!(
            result,
            vec![
                "L1: # Helper function",
                "L2: # for computation",
                "L3: def compute(x):",
                "L4:     return x * 2",
            ]
        );

        let opts_no_header = make_opts(Some(4), 1, false, false, None);
        let result_no = read_block(content, 1, 2000, opts_no_header).unwrap();
        assert_eq!(
            result_no,
            vec!["L3: def compute(x):", "L4:     return x * 2",]
        );
    }

    #[test]
    fn limit_caps_output_size() {
        // anchor=3 (b = 2), final_limit = min(3, 3, 6) = 3. The first iteration
        // takes one line upward and one downward, filling the budget.
        let content = b"def foo():\n    a = 1\n    b = 2\n    c = 3\n    d = 4\n    e = 5\n";
        let opts = make_opts(Some(3), 0, false, true, Some(3));
        let result = read_block(content, 1, 3, opts).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result,
            vec!["L2:     a = 1", "L3:     b = 2", "L4:     c = 3",]
        );
    }

    #[test]
    fn final_limit_1_returns_anchor_only() {
        let content = b"line1\nline2\nline3\n";
        let opts = make_opts(Some(2), 0, false, true, Some(1));
        let result = read_block(content, 1, 2000, opts).unwrap();
        assert_eq!(result, vec!["L2: line2"]);
    }

    #[test]
    fn anchor_exceeds_file_length_error() {
        let content = b"one\ntwo\n";
        let opts = make_opts(Some(100), 0, false, true, None);
        let result = read_block(content, 1, 2000, opts);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "anchor_line exceeds file length");
    }

    #[test]
    fn empty_file_returns_empty() {
        let content = b"";
        let opts = make_opts(None, 0, false, true, None);
        let result = read_block(content, 1, 2000, opts).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn guard_limit_zero_returns_error() {
        let content = b"line1\nline2\n";
        let opts = make_opts(Some(1), 0, false, true, Some(0));
        let result = read_block(content, 1, 2000, opts);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "max_lines must be greater than zero");
    }

    #[test]
    fn trims_leading_trailing_blank_lines() {
        let content = b"\ndef foo():\n    x = 1\n\n";
        let opts = make_opts(Some(3), 1, false, true, None);
        let result = read_block(content, 1, 2000, opts).unwrap();
        // A trimmed result never starts or ends with a bare "Ln: " line.
        assert!(!result.first().unwrap().ends_with(": "));
        assert!(!result.last().unwrap().ends_with(": "));
    }

    #[test]
    fn trimmed_uses_trim_start() {
        let rec = LineRecord {
            number: 1,
            raw: "  hello  ".to_string(),
            display: "  hello  ".to_string(),
            indent: 2,
        };
        assert_eq!(rec.trimmed(), "hello  ");
        assert!(!rec.is_blank());
    }

    #[test]
    fn is_comment_uses_raw_trim() {
        let rec = LineRecord {
            number: 1,
            raw: "  // comment  ".to_string(),
            display: "  // comment  ".to_string(),
            indent: 2,
        };
        assert!(rec.is_comment());
    }

    #[test]
    fn cpp_switch_shallow_expansion() {
        let content = b"#include <iostream>\n\nint main() {\n    switch (x) {\n        case 1:\n            std::cout << \"one\";\n            break;\n        case 2:\n            std::cout << \"two\";\n            break;\n    }\n    return 0;\n}\n";
        // anchor=6 (std::cout << "one", indent 12), max_levels=1, min_indent=12-4=8.
        let opts = make_opts(Some(6), 1, false, true, None);
        let result = read_block(content, 1, 2000, opts).unwrap();
        assert!(result.iter().any(|l| l.contains("case 1:")));
        assert!(result.iter().any(|l| l.contains("\"one\"")));
    }

    #[test]
    fn cpp_switch_deeper_expansion() {
        let content = b"// Main entry point\n#include <iostream>\n\nint main() {\n    switch (x) {\n        case 1:\n            std::cout << \"one\";\n            break;\n        case 2:\n            std::cout << \"two\";\n            break;\n    }\n    return 0;\n}\n";
        // anchor=7 (std::cout << "one", indent 12), max_levels=2, min_indent=12-8=4.
        let opts = make_opts(Some(7), 2, false, true, None);
        let result = read_block(content, 1, 2000, opts).unwrap();
        assert!(result.iter().any(|l| l.contains("switch (x)")));
    }
}
