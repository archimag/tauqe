//! Control tags of the XML edit protocol that are not code edits:
//! `<context_request>`, `<doc_request>`, `<user_language>`, `<verify>` and `<plan>`.

mod context;
mod docs;
mod language;
mod outcome;
mod plan;
mod scratchpad;
mod verify;

pub(crate) use context::next_context_request_tag;
pub(crate) use docs::next_doc_request_tag;
pub(crate) use language::next_user_language_tag;
pub(crate) use outcome::{next_error_tag, next_success_tag};
pub(crate) use plan::next_plan_tag;
pub(crate) use scratchpad::next_scratchpad_tag;
pub(crate) use verify::next_verify_tag;

pub(crate) use context::parse_context_request_tags;
pub(crate) use docs::parse_doc_request_tags;

pub use context::{parse_context_drop_tags, strip_context_drop_tags, strip_context_request_tags};
pub use docs::strip_doc_request_tags;
pub use language::{extract_user_language, strip_user_language_tags};
pub use outcome::{
    extract_blocked, extract_error, extract_success, extract_terminal_outcome, strip_blocked_tags,
    strip_error_tags, strip_success_tags, strip_terminal_outcome_tags, TerminalOutcome,
};
pub use plan::{parse_plan_tags_with_diagnostics, strip_plan_tags, ParsedPlanTag};
pub use scratchpad::{extract_scratchpad, strip_scratchpad_tags};
pub use verify::{
    extract_verify_request, strip_verify_tags, VerifyOnSuccess, VerifyRequest, VerifyTarget,
};

/// Locates the next complete tag starting with `open` (e.g. `<verify`), either
/// self-closing or paired with an immediately following closing tag. The optional
/// turn-marker suffix is accepted. Returns the byte range `(start, end)`.
/// Incomplete tags yield None.
fn next_simple_tag(text: &str, open: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(open) {
        let start = from + rel;
        let after = start + open.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            if is_inside_markdown_code(text, start) {
                from = after;
                continue;
            }
            let gt = rest.find('>')?;
            let mut end = after + idx + gt + 1;
            if !rest[..gt].trim_end().ends_with('/') {
                let close_tag = format!("</{}>", tag_name);
                let after_open = &text[end..];
                let trimmed = after_open.trim_start();
                if trimmed.starts_with(&close_tag) {
                    end += (after_open.len() - trimmed.len()) + close_tag.len();
                }
            }
            return Some((start, end));
        }
        from = after;
    }
    None
}

fn next_control_tag(text: &str, open: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(open) {
        let start = from + rel;
        let after = start + open.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            if is_inside_markdown_code(text, start) {
                from = after;
                continue;
            }
            let gt = rest.find('>')?;
            let end_open = after + idx + gt + 1;
            if rest[..gt].trim_end().ends_with('/') {
                return Some((start, end_open));
            }
            let close_tag = format!("</{}>", tag_name);
            if let Some(close_offset) = text[end_open..].find(&close_tag) {
                let end = end_open + close_offset + close_tag.len();
                return Some((start, end));
            } else {
                return Some((start, end_open));
            }
        }
        from = after;
    }
    None
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

/// Returns true if byte offset `target_pos` in `text` is inside a markdown fenced code block (``` or ~~~).
fn is_inside_code_fence(text: &str, target_pos: usize) -> bool {
    let mut in_fence: Option<(char, usize)> = None;
    let mut line_start = 0;

    while line_start < text.len() {
        let (line, next_line_start) = match text[line_start..].find('\n') {
            Some(nl) => (&text[line_start..line_start + nl], line_start + nl + 1),
            None => (&text[line_start..], text.len()),
        };
        let line_trimmed_cr = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line_trimmed_cr.trim_start();
        let leading_spaces = line_trimmed_cr.len() - trimmed.len();

        let is_fence_line = if leading_spaces <= 3 {
            match trimmed.chars().next() {
                Some(ch @ ('`' | '~')) => {
                    let count = trimmed.chars().take_while(|&c| c == ch).count();
                    if count >= 3 {
                        Some((ch, count))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        if let Some((ch, count)) = is_fence_line {
            if let Some((active_ch, active_count)) = in_fence {
                if ch == active_ch && count >= active_count {
                    if target_pos >= line_start && target_pos < next_line_start {
                        return true;
                    }
                    in_fence = None;
                    line_start = next_line_start;
                    continue;
                }
            } else {
                if target_pos >= line_start && target_pos < next_line_start {
                    return true;
                }
                in_fence = Some((ch, count));
                line_start = next_line_start;
                continue;
            }
        }

        if in_fence.is_some() && target_pos >= line_start && target_pos < next_line_start {
            return true;
        }

        if line_start > target_pos {
            break;
        }

        line_start = next_line_start;
    }

    in_fence.is_some() && target_pos >= line_start
}

/// Returns true if byte offset `target_pos` in `text` is inside a markdown code block (fenced or inline).
fn is_inside_markdown_code(text: &str, target_pos: usize) -> bool {
    is_inside_code_fence(text, target_pos) || is_inside_inline_code(text, target_pos)
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
}
