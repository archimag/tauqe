//! Control tags of the XML edit protocol that are not code edits:
//! `<context_request>`, `<doc_request>`, `<user_language>`, `<verify>` and `<plan>`.

mod context;
mod docs;
mod language;
mod outcome;
mod plan;
mod scratchpad;
mod search;
mod verify;

use super::engine;

pub(crate) use context::parse_context_request_tags;
pub(crate) use docs::parse_doc_request_tags;
pub(crate) use search::parse_repo_search_tags;

pub use context::strip_context_request_tags;
pub use docs::strip_doc_request_tags;
pub use search::strip_repo_search_tags;
pub use language::{extract_user_language, strip_user_language_tags};
pub use outcome::{
    extract_blocked, extract_error, extract_skip, extract_success, extract_terminal_outcome,
    strip_blocked_tags, strip_error_tags, strip_skip_tags, strip_success_tags,
    strip_terminal_outcome_tags, TerminalOutcome,
};
pub use plan::{
    parse_plan_tags_with_diagnostics, strip_plan_tags, ParsedPlanItem, ParsedPlanTag,
};
pub use scratchpad::{extract_scratchpad, strip_scratchpad_tags};
pub use verify::{
    extract_verify_request, strip_verify_tags, VerifyOnSuccess, VerifyRequest, VerifyTarget,
};

/// Locates the next complete tag named by `open` (for example `<verify`).
///
/// The close mode is declared by the tag's [`XmlTagSpec`], so every tag family
/// shares one scanner: self-closing, closed directly after the header, or paired
/// with a later closing tag. An optional turn-marker suffix is accepted, tags
/// inside markdown code are ignored, and an incomplete tag yields `None`.
fn next_tag_bounds(text: &str, open: &str) -> Option<(usize, usize)> {
    let spec = engine::tag_spec(open.trim_start_matches('<'))?;
    engine::find_next_tag_bounds(text, spec)
}

fn strip_tags(text: &str, next: fn(&str) -> Option<(usize, usize)>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = next(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Returns true if byte offset `target_pos` in `text` is inside an inline markdown code span (`...`).
fn is_inside_inline_code(text: &str, target_pos: usize) -> bool {
    let line_start = text[..target_pos].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[target_pos..].find('\n').map_or(text.len(), |i| target_pos + i);
    let line = &text[line_start..line_end];
    let rel_pos = target_pos - line_start;

    let mut i = 0;
    let bytes = line.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let start_bt = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let bt_len = i - start_bt;
            let delimiter = "`".repeat(bt_len);
            if let Some(close_rel) = line[i..].find(&delimiter) {
                let span_end = i + close_rel + bt_len;
                if rel_pos >= start_bt && rel_pos < span_end {
                    return true;
                }
                i = span_end;
                continue;
            } else {
                if rel_pos >= start_bt {
                    return true;
                }
                break;
            }
        }
        i += 1;
    }
    false
}

/// Sorted, non-overlapping byte ranges covered by markdown fenced code blocks.
///
/// An interval is half-open: `start <= pos < end` means `pos` belongs to a
/// block. It starts at the first byte of the opening fence line and ends after
/// the closing fence line, or at `text.len()` when the block is never closed.
#[derive(Debug)]
pub(crate) struct CodeFences {
    intervals: Vec<(usize, usize)>,
}

impl CodeFences {
    /// Single pass over `text` that records every fenced code block.
    ///
    /// The CommonMark rules that matter for tag detection are enforced: a fence
    /// is indented by at most three spaces, is built from at least three
    /// backticks or tildes, and only a fence with the same character, at least
    /// as long as the opening one, and free of trailing content closes a block.
    /// An unterminated fence extends to the end of `text`.
    pub(crate) fn find(text: &str) -> Self {
        let mut intervals: Vec<(usize, usize)> = Vec::new();
        let mut open: Option<(char, usize, usize)> = None;
        let mut line_start = 0;

        while line_start < text.len() {
            let line_end = text[line_start..]
                .find('\n')
                .map_or(text.len(), |nl| line_start + nl);
            let next_line_start = (line_end + 1).min(text.len());
            let raw_line = &text[line_start..line_end];
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            let marker = if indent <= 3 {
                fence_marker(trimmed)
            } else {
                None
            };

            match (open, marker) {
                (Some((open_ch, open_len, start)), Some((ch, len, rest)))
                    if ch == open_ch && len >= open_len && rest.trim().is_empty() =>
                {
                    intervals.push((start, next_line_start));
                    open = None;
                }
                (None, Some((ch, len, _))) => open = Some((ch, len, line_start)),
                _ => {}
            }

            line_start = next_line_start;
        }

        if let Some((_, _, start)) = open {
            intervals.push((start, text.len()));
        }

        Self { intervals }
    }

    /// Returns true when `target_pos` falls inside any recorded block.
    ///
    /// Intervals are sorted and disjoint, so the lookup is a binary search
    /// costing `O(log K)` for `K` blocks and ends immediately when none exist.
    pub(crate) fn is_inside(&self, target_pos: usize) -> bool {
        let idx = self
            .intervals
            .partition_point(|&(start, _)| start <= target_pos);
        match idx.checked_sub(1).and_then(|i| self.intervals.get(i)) {
            Some(&(_, end)) => target_pos < end,
            None => false,
        }
    }
}

/// Splits the start of a trimmed line into `(fence char, fence length, rest)`.
///
/// Returns `None` when the line does not open with at least three identical
/// backticks or tildes. `rest` keeps the remainder of the line so a caller can
/// tell an opening info string from forbidden closing-fence content.
fn fence_marker(trimmed: &str) -> Option<(char, usize, &str)> {
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    Some((ch, len, &trimmed[len..]))
}

/// Returns true if byte offset `target_pos` in `text` is inside a markdown code block.
///
/// Fenced blocks come from a precomputed [`CodeFences`] index, so a caller that
/// probes many offsets pays for a single scan; inline spans are checked on the
/// spot for the queried position.
pub(crate) fn is_inside_markdown_code_with_fences(
    text: &str,
    target_pos: usize,
    fences: &CodeFences,
) -> bool {
    fences.is_inside(target_pos) || is_inside_inline_code(text, target_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_and_strip_success_self_closing_with_summary() {
        let text = "Changes applied.\n<success summary=\"Created models and passed checks\" />\nDone.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Success {
                summary: Some("Created models and passed checks".to_string()),
            }
        );
        assert_eq!(extract_success(text), Some(Some("Created models and passed checks".to_string())));
        assert_eq!(extract_error(text), None);

        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<success"));
        assert_eq!(stripped.trim(), "Changes applied.\n\nDone.");
    }

    #[test]
    fn test_extract_and_strip_success_paired_with_body() {
        let text = "Result:\n<success>\nRefactored database module and verified transactions.\n</success>\nFinished.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Success {
                summary: Some("Refactored database module and verified transactions.".to_string()),
            }
        );
        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<success"));
        assert_eq!(stripped.trim(), "Result:\n\nFinished.");
    }

    #[test]
    fn test_extract_and_strip_success_empty() {
        let text = "Completed.\n<success />";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(outcome, TerminalOutcome::Success { summary: None });
        assert_eq!(extract_success(text), Some(None));
    }

    #[test]
    fn test_extract_and_strip_error_attribute() {
        let text = "Blocked:\n<error message=\"Missing required cryptographic library in Cargo.toml\" />\nExiting.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Error {
                message: "Missing required cryptographic library in Cargo.toml".to_string(),
            }
        );
        assert_eq!(
            extract_error(text),
            Some("Missing required cryptographic library in Cargo.toml".to_string())
        );
        assert_eq!(extract_success(text), None);

        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<error"));
        assert_eq!(stripped.trim(), "Blocked:\n\nExiting.");
    }

    #[test]
    fn test_extract_and_strip_blocked_attribute() {
        let text = "Prerequisite missing:\n<blocked reason=\"Dependency not installed\" details=\"Please run npm install\" />\nHalting.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Blocked {
                reason: "Dependency not installed".to_string(),
                details: Some("Please run npm install".to_string()),
            }
        );
        assert_eq!(
            extract_blocked(text),
            Some((
                "Dependency not installed".to_string(),
                Some("Please run npm install".to_string())
            ))
        );

        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<blocked"));
        assert_eq!(stripped.trim(), "Prerequisite missing:\n\nHalting.");
    }

    #[test]
    fn test_extract_and_strip_blocked_paired_with_body() {
        let text = "Blocked:\n<blocked reason=\"Assumption broken\">\nUnderlying service API has changed.\n</blocked>\nStopping.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Blocked {
                reason: "Assumption broken".to_string(),
                details: Some("Underlying service API has changed.".to_string()),
            }
        );
        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<blocked"));
        assert_eq!(stripped.trim(), "Blocked:\n\nStopping.");
    }

    #[test]
    fn test_extract_and_strip_error_paired_with_body() {
        let text = "Inspection failed:\n<error>\nContradiction in architectural requirements: cannot ensure atomic commits.\n</error>\nHalting.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Error {
                message: "Contradiction in architectural requirements: cannot ensure atomic commits.".to_string(),
            }
        );
        let stripped = strip_terminal_outcome_tags(text);
        assert!(!stripped.contains("<error"));
        assert_eq!(stripped.trim(), "Inspection failed:\n\nHalting.");
    }

    #[test]
    fn test_terminal_outcome_ignores_markdown_code() {
        let text = r#"
Example outcome:
```xml
<success summary="Mock success" />
<error message="Mock error" />
```
Inline `<success />` and `<error message="inline" />`.
Real outcome:
<error message="Real blocker" />
"#;
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Error {
                message: "Real blocker".to_string(),
            }
        );
        let stripped = strip_terminal_outcome_tags(text);
        assert!(stripped.contains("<success summary=\"Mock success\" />"));
        assert!(stripped.contains("`<error message=\"inline\" />`"));
        assert!(!stripped.contains("<error message=\"Real blocker\" />"));
    }

    #[test]
    fn code_fences_record_nothing_without_blocks() {
        let fences = CodeFences::find("plain text\nwith `inline` spans");
        assert!(fences.intervals.is_empty());
        assert!(!fences.is_inside(0));
        assert!(!fences.is_inside(11));
    }

    #[test]
    fn code_fences_record_single_block() {
        let text = "intro\n```\ncode\n```\noutro";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(6, 19)]);
        assert!(!fences.is_inside(5));
        assert!(fences.is_inside(6));
        assert!(fences.is_inside(18));
        assert!(!fences.is_inside(19));
    }

    #[test]
    fn code_fences_record_multiple_blocks_with_both_markers() {
        let text = "```\na\n```\nmid\n~~~\nb\n~~~\ntail";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(0, 10), (14, 24)]);
        assert!(fences.is_inside(0));
        assert!(!fences.is_inside(10));
        assert!(!fences.is_inside(13));
        assert!(fences.is_inside(14));
        assert!(fences.is_inside(23));
        assert!(!fences.is_inside(24));
    }

    #[test]
    fn code_fences_extend_unclosed_block_to_end() {
        let text = "note\n```\nstill open";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(5, text.len())]);
        assert!(fences.is_inside(5));
        assert!(fences.is_inside(text.len() - 1));
        assert!(!fences.is_inside(text.len()));
    }

    #[test]
    fn code_fences_require_matching_closing_marker() {
        let text = "````\n```\n````\n";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(0, text.len())]);
        assert!(fences.is_inside(6));
        assert!(fences.is_inside(9));
        assert!(!fences.is_inside(text.len()));
    }

    #[test]
    fn code_fences_ignore_other_marker_character() {
        let text = "```\n~~~~\n```\n";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(0, 13)]);
        assert!(fences.is_inside(4));
        assert!(!fences.is_inside(13));
    }

    #[test]
    fn code_fences_allow_at_most_three_spaces_of_indent() {
        let text = "   ```\nx\n   ```";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(0, text.len())]);
        assert!(fences.is_inside(text.len() - 1));

        let deep = "    ```\nx\n    ```";
        let deep_fences = CodeFences::find(deep);
        assert!(deep_fences.intervals.is_empty());
        assert!(!deep_fences.is_inside(5));
    }

    #[test]
    fn code_fences_reject_closing_line_with_trailing_content() {
        let text = "```\n```rust\n```\n";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(0, 16)]);
    }

    #[test]
    fn code_fences_handle_multibyte_offsets() {
        let text = "привет\n```\nкод\n```";
        let fences = CodeFences::find(text);
        assert_eq!(fences.intervals, vec![(13, text.len())]);
        assert!(!fences.is_inside(12));
        assert!(fences.is_inside(13));
        assert!(fences.is_inside(text.len() - 1));
    }

    #[test]
    fn markdown_code_combines_fences_and_inline_spans() {
        let text = "inline `x` here\n```\nfenced\n```";
        let fences = CodeFences::find(text);
        let fenced_pos = text.find("fenced").unwrap();

        assert!(is_inside_markdown_code_with_fences(text, 8, &fences));
        assert!(!fences.is_inside(8));
        assert!(is_inside_markdown_code_with_fences(text, fenced_pos, &fences));
        assert!(!is_inside_markdown_code_with_fences(text, 0, &fences));
    }
}
