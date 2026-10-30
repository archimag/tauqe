//! Control tags of the XML edit protocol that are not code edits: the
//! discovery requests (`<context_request>`, `<repo_search>`, `<repo_dirs>`,
//! `<repo_map>`, `<dir_list>`), documentation requests, verification, plan
//! blocks, and the turn-scoped scratchpad.

mod context;
mod dir_list;
mod dirs;
mod docs;
mod language;
mod outcome;
mod plan;
mod scratchpad;
mod search;
mod verify;

use super::engine;

pub(crate) use context::parse_context_request_tags;
pub(crate) use dir_list::parse_dir_list_tags;
pub(crate) use dirs::parse_repo_dirs_tags;
pub(crate) use docs::parse_doc_request_tags;
pub(crate) use search::parse_repo_search_tags;

pub use context::strip_context_request_tags;
pub use dirs::strip_repo_dirs_tags;
pub use docs::strip_doc_request_tags;
pub use language::{extract_user_language, strip_user_language_tags};
pub use outcome::{
    extract_blocked, extract_error, extract_skip, extract_success, extract_terminal_outcome,
    strip_blocked_tags, strip_error_tags, strip_skip_tags, strip_success_tags,
    strip_terminal_outcome_tags, TerminalOutcome,
};
pub use plan::{parse_plan_tags_with_diagnostics, strip_plan_tags, ParsedPlanItem, ParsedPlanTag};
pub use scratchpad::{extract_scratchpad, strip_scratchpad_tags};
pub use search::strip_repo_search_tags;
pub use verify::{
    extract_verify_request, strip_verify_tags, VerifyOnSuccess, VerifyRequest, VerifyTarget,
};

/// Resolves the registry entry of the `<repo_map>` control tag.
fn repo_map_spec() -> Option<&'static engine::XmlTagSpec> {
    engine::tag_spec("repo_map")
}

/// Removes all targeted repository map tags from text.
pub fn strip_repo_map_tags(text: &str) -> String {
    match repo_map_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

/// Extracts targeted repository map requests (`<repo_map path="..." />`) in order.
///
/// A missing `path` attribute yields an empty string, which names the whole
/// repository. Marked occurrences inside markdown code are ignored by the tag
/// engine, so a quoted example never widens what the model can see.
pub(crate) fn parse_repo_map_tags(text: &str) -> Vec<String> {
    match repo_map_spec() {
        Some(spec) => engine::iter_tags(text, spec)
            .map(|tag| tag.attr("path").unwrap_or_default())
            .collect(),
        None => Vec::new(),
    }
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

/// Where a byte offset sits relative to the inline code spans of its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineCode {
    /// The offset is outside every inline code span.
    Outside,
    /// The offset is inside a span whose extent is already settled.
    Inside,
    /// The offset is inside a span the still growing line may yet close.
    Unsettled,
}

/// Returns true if byte offset `target_pos` in `text` is inside an inline markdown code span (`...`).
pub(crate) fn is_inside_inline_code(text: &str, target_pos: usize) -> bool {
    !matches!(inline_code_span(text, target_pos), InlineCode::Outside)
}

/// Classifies byte offset `target_pos` against the inline code spans of its line.
///
/// A span without a closing delimiter is the one case a stream cannot decide yet:
/// a backtick that has not arrived may still turn the offset into code, or settle
/// it back into prose. The line boundary settles it, so a span that reaches the
/// end of a complete line is reported as [`InlineCode::Inside`].
pub(crate) fn inline_code_span(text: &str, target_pos: usize) -> InlineCode {
    let line_start = text[..target_pos].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[target_pos..]
        .find('\n')
        .map_or(text.len(), |i| target_pos + i);
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
                    return InlineCode::Inside;
                }
                i = span_end;
                continue;
            } else {
                if rel_pos >= start_bt {
                    return if line_end < text.len() {
                        InlineCode::Inside
                    } else {
                        InlineCode::Unsettled
                    };
                }
                break;
            }
        }
        i += 1;
    }
    InlineCode::Outside
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
            if let Some(fence_start) =
                fold_fence_line(&mut open, &text[line_start..line_end], line_start)
            {
                intervals.push((fence_start, next_line_start));
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
/// Folds one line into the open-fence state, reporting a fence it just closed.
///
/// `line` excludes its terminating newline. The returned offset is the start of
/// the fence the line closed, so the caller can pair it with its own line
/// boundary. [`CodeFences`] and [`FenceScan`] both fold through this routine, so
/// a streamed block is delimited exactly as a complete text would be.
fn fold_fence_line(
    open: &mut Option<(char, usize, usize)>,
    line: &str,
    line_start: usize,
) -> Option<usize> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let marker = if indent <= 3 {
        fence_marker(trimmed)
    } else {
        None
    };
    match (*open, marker) {
        (Some((open_ch, open_len, fence_start)), Some((ch, len, rest)))
            if ch == open_ch && len >= open_len && rest.trim().is_empty() =>
        {
            *open = None;
            Some(fence_start)
        }
        (None, Some((ch, len, _))) => {
            *open = Some((ch, len, line_start));
            None
        }
        _ => None,
    }
}

/// Incremental CommonMark fenced-code tracker for a growing stream buffer.
///
/// [`CodeFences`] indexes a complete text once. A stream cannot afford that shape:
/// the buffer grows by one provider delta at a time, so re-indexing the whole
/// buffer per chunk would make every scan quadratic in response length. The
/// tracker therefore keeps only the fence that is still open plus the offset of
/// the trailing line it has not classified yet, and each call folds just the lines
/// appended since the previous one.
#[derive(Debug, Default)]
pub(crate) struct FenceScan {
    /// Blocks that already closed, sorted and disjoint.
    ///
    /// The scan must be able to classify a position an earlier chunk already
    /// passed, so a block that closed later in the same buffer is remembered
    /// instead of being forgotten: only then does the incremental tracker agree
    /// with [`CodeFences`] for every position of the buffer.
    closed: Vec<(usize, usize)>,
    /// The fence still open: character, length, and the offset where it opened.
    open: Option<(char, usize, usize)>,
    /// Offset of the trailing line, whose newline has not arrived yet.
    scanned: usize,
}

impl FenceScan {
    /// Classifies every complete line that starts before `limit`.
    pub(crate) fn advance_to(&mut self, buf: &str, limit: usize) {
        while self.scanned < limit {
            let Some(newline) = buf[self.scanned..].find('\n') else {
                break;
            };
            let line_start = self.scanned;
            let line_end = line_start + newline;
            let next_line_start = line_end + 1;
            if let Some(fence_start) =
                fold_fence_line(&mut self.open, &buf[line_start..line_end], line_start)
            {
                self.closed.push((fence_start, next_line_start));
            }
            self.scanned = next_line_start;
        }
    }

    /// Reports whether `pos` lies in a fenced block of the streamed buffer.
    ///
    /// A position on the trailing, still incomplete line is decided from the bytes
    /// the line already carries: a fence marker needs at most a few of them, and
    /// nothing arriving later can turn such a line back into prose.
    pub(crate) fn is_inside(&self, buf: &str, pos: usize) -> bool {
        if self.open.is_some_and(|(_, _, start)| pos >= start) {
            return true;
        }
        let preceded = self.closed.partition_point(|&(start, _)| start <= pos);
        if preceded
            .checked_sub(1)
            .and_then(|index| self.closed.get(index))
            .is_some_and(|&(_, end)| pos < end)
        {
            return true;
        }
        if pos < self.scanned {
            return false;
        }
        let trailing = buf.get(self.scanned..).unwrap_or_default();
        let trimmed = trailing.trim_start();
        let indent = trailing.len() - trimmed.len();
        indent <= 3 && fence_marker(trimmed).is_some()
    }

    /// Shifts the tracked offsets after `drained` bytes left the buffer.
    pub(crate) fn rebase(&mut self, drained: usize) {
        if drained == 0 {
            return;
        }
        self.scanned = self.scanned.saturating_sub(drained);
        self.open = self
            .open
            .map(|(ch, len, start)| (ch, len, start.saturating_sub(drained)));
        self.closed.retain(|&(_, end)| end > drained);
        for interval in &mut self.closed {
            interval.0 = interval.0.saturating_sub(drained);
            interval.1 -= drained;
        }
    }
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
    fn test_parse_and_strip_repo_map_tags() {
        let text = "Outline needed.\n<repo_map path=\"crates/core\" />\n<repo_map />\nDone.";
        assert_eq!(
            parse_repo_map_tags(text),
            vec!["crates/core".to_string(), String::new()]
        );
        assert_eq!(strip_repo_map_tags(text), "Outline needed.\n\n\nDone.");

        // A marker-suffixed request is still the same control tag.
        assert_eq!(
            parse_repo_map_tags("<repo_map_AB12 path=\"src\" />"),
            vec!["src".to_string()]
        );

        // A fenced example is documentation, not a command.
        assert!(parse_repo_map_tags("```xml\n<repo_map path=\"src\" />\n```").is_empty());
        assert!(!parse_repo_map_tags("no request here").iter().any(|_| true));
    }

    #[test]
    fn test_extract_and_strip_success_self_closing_with_summary() {
        let text =
            "Changes applied.\n<success summary=\"Created models and passed checks\" />\nDone.";
        let outcome = extract_terminal_outcome(text).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Success {
                summary: Some("Created models and passed checks".to_string()),
            }
        );
        assert_eq!(
            extract_success(text),
            Some(Some("Created models and passed checks".to_string()))
        );
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
                message:
                    "Contradiction in architectural requirements: cannot ensure atomic commits."
                        .to_string(),
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
        assert!(is_inside_markdown_code_with_fences(
            text, fenced_pos, &fences
        ));
        assert!(!is_inside_markdown_code_with_fences(text, 0, &fences));
    }
    #[test]
    fn fence_scan_agrees_with_the_one_shot_index() {
        let text = "intro\n```\ncode with <tag>\n```\noutro\n~~~\nopen";
        let fences = CodeFences::find(text);
        let mut scan = FenceScan::default();
        scan.advance_to(text, text.len());
        for pos in 0..text.len() {
            assert_eq!(
                scan.is_inside(text, pos),
                fences.is_inside(pos),
                "disagreement at {pos}"
            );
        }
    }

    #[test]
    fn fence_scan_classifies_only_appended_lines() {
        let mut scan = FenceScan::default();
        let mut buffer = String::from("intro\n`");
        scan.advance_to(&buffer, buffer.len());
        assert!(!scan.is_inside(&buffer, 7));

        // A fence marker split across two chunks still opens its block once the
        // rest of the line arrives.
        buffer.push_str("``\nfenced\n");
        scan.advance_to(&buffer, buffer.len());
        let fenced = buffer.find("fenced").expect("the fenced line");
        assert!(scan.is_inside(&buffer, fenced));
    }
}
