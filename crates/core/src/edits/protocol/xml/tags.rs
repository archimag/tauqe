//! Control tags of the XML edit protocol that are not code edits:
//! `<context_request>`, `<doc_request>`, `<user_language>` and `<verify>`.

use tauqe_protocol::ContextAccess;

use super::{extract_attribute, find_base_tag_start};
use crate::edits::protocol::ContextRequest;

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

/// Locates the next complete `<context_request .../>` tag (unsuffixed dialect).
pub(crate) fn next_context_request_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<context_request")
}

/// Locates the next complete `<context_drop .../>` tag (unsuffixed dialect).
pub(crate) fn next_context_drop_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<context_drop")
}

/// Locates the next complete `<doc_request .../>` tag.
pub(crate) fn next_doc_request_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<doc_request")
}

/// Locates the next complete `<verify .../>` or `<verify>...</verify>` tag.
pub(crate) fn next_verify_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<verify")
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

/// Locates the next complete `<plan ...>...</plan>` or `<plan .../>` tag.
pub(crate) fn next_plan_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<plan";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
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
            let close_offset = text[end_open..].find(&close_tag)?;
            let end = end_open + close_offset + close_tag.len();
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Locates the next complete `<user_language>...</user_language>` or self-closing tag.
pub(crate) fn next_user_language_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<user_language";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
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
            let close_offset = text[end_open..].find(&close_tag)?;
            let end = end_open + close_offset + close_tag.len();
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Extracts user language tag content if present (e.g. `<user_language>Russian</user_language>`
/// or `<user_language lang="ru"/>`).
pub fn extract_user_language(text: &str) -> Option<String> {
    let (start, tag_name) = find_base_tag_start(text, "user_language")?;
    let slice = &text[start..];
    let open_end = slice.find('>')?;
    let header = &slice[..open_end + 1];
    if let Some(val) = extract_attribute(header, "value")
        .or_else(|| extract_attribute(header, "lang"))
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() && trimmed.len() < 50 && !trimmed.contains('\n') {
            return Some(trimmed.to_string());
        }
    }
    let close_tag = format!("</{}>", tag_name);
    let close_start = slice.find(&close_tag)?;
    if close_start >= open_end {
        let content = slice[open_end + 1..close_start].trim();
        if !content.is_empty() && content.len() < 50 && !content.contains('\n') {
            return Some(content.to_string());
        }
    }
    None
}

/// Removes all user_language tags from text.
pub fn strip_user_language_tags(text: &str) -> String {
    let mut result = text.to_string();
    while let Some((start, tag_name)) = find_base_tag_start(&result, "user_language") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = result[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            result.replace_range(start..end, "");
        } else if let Some(gt_offset) = result[start..].find('>') {
            let end = start + gt_offset + 1;
            result.replace_range(start..end, "");
        } else {
            result.replace_range(start.., "");
            break;
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyTarget {
    All,
    Check,
    Test,
    Clippy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOnSuccess {
    Silent,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    pub target: VerifyTarget,
    pub on_success: VerifyOnSuccess,
}

/// Extracts verification request if specified by the model.
pub fn extract_verify_request(text: &str) -> Option<VerifyRequest> {
    let (start, end) = next_verify_tag(text)?;
    let tag = &text[start..end];
    let header_end = tag.find('>').unwrap_or(tag.len());
    let header = &tag[..header_end];

    let target = match extract_attribute(header, "target").as_deref() {
        Some("check") => VerifyTarget::Check,
        Some("test") => VerifyTarget::Test,
        Some("clippy") => VerifyTarget::Clippy,
        _ => VerifyTarget::All,
    };

    let on_success = match extract_attribute(header, "on_success").as_deref() {
        Some("silent") => VerifyOnSuccess::Silent,
        _ => VerifyOnSuccess::Report,
    };

    Some(VerifyRequest { target, on_success })
}

/// Removes all verify tags from text.
pub fn strip_verify_tags(text: &str) -> String {
    strip_tags(text, next_verify_tag)
}

/// Removes all context request tags from text.
pub fn strip_context_request_tags(text: &str) -> String {
    strip_tags(text, next_context_request_tag)
}

/// Removes all context drop tags from text.
pub fn strip_context_drop_tags(text: &str) -> String {
    strip_tags(text, next_context_drop_tag)
}

/// Removes all documentation request tags from text.
pub fn strip_doc_request_tags(text: &str) -> String {
    strip_tags(text, next_doc_request_tag)
}

/// Removes all plan tags from text.
pub fn strip_plan_tags(text: &str) -> String {
    strip_tags(text, next_plan_tag)
}

/// Locates the next complete simple control tag by opening prefix (e.g. `<success`, `<error`),
/// supporting self-closing `<tag .../>` or paired `<tag ...>...</tag>`.
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

/// Locates the next complete `<scratchpad ...>...</scratchpad>` or `<scratchpad .../>` tag.
pub(crate) fn next_scratchpad_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<scratchpad")
}

/// Extracts ephemeral scratchpad content from model output if present.
pub fn extract_scratchpad(text: &str) -> Option<String> {
    let mut notes = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_scratchpad_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let tag_name = header[1..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect::<String>();
        let close_tag = format!("</{}>", tag_name);

        let mut note = None;
        if !is_self_closing && tag.ends_with(&close_tag) {
            let body = tag[header_end + 1..tag.len() - close_tag.len()].trim();
            if !body.is_empty() {
                note = Some(body.to_string());
            }
        }
        if note.is_none() {
            if let Some(attr_note) = extract_attribute(header, "note") {
                let trimmed = attr_note.trim();
                if !trimmed.is_empty() {
                    note = Some(trimmed.to_string());
                }
            }
        }
        if let Some(n) = note {
            notes.push(n);
        }
        pos = abs_end;
    }
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n\n"))
    }
}

/// Removes all scratchpad tags and enclosed content from text.
pub fn strip_scratchpad_tags(text: &str) -> String {
    strip_tags(text, next_scratchpad_tag)
}

/// Universal terminal outcome emitted by the model at the conclusion of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success { summary: Option<String> },
    Error { message: String },
}

impl TerminalOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn summary(&self) -> Option<&str> {
        match self {
            Self::Success { summary } => summary.as_deref(),
            Self::Error { .. } => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Error { message } => Some(message.as_str()),
        }
    }
}

/// Locates the next complete `<success .../>` or `<success>...</success>` tag.
pub(crate) fn next_success_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<success")
}

/// Locates the next complete `<error .../>` or `<error>...</error>` tag.
pub(crate) fn next_error_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<error")
}

/// Extracts `<success ...>` completion signal from model output if present.
pub fn extract_success(text: &str) -> Option<Option<String>> {
    let mut last_summary = None;
    let mut found = false;
    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut summary = extract_attribute(header, "summary")
            .or_else(|| extract_attribute(header, "note"))
            .or_else(|| extract_attribute(header, "message"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if summary.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                summary = Some(body.to_string());
            }
        }
        last_summary = summary;
        found = true;
        pos = abs_end;
    }
    if found {
        Some(last_summary)
    } else {
        None
    }
}

/// Extracts `<error ...>` blocker/failure signal from model output if present.
pub fn extract_error(text: &str) -> Option<String> {
    let mut last_error = None;
    let mut pos = 0;
    while let Some((start, end)) = next_error_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut msg = extract_attribute(header, "message")
            .or_else(|| extract_attribute(header, "reason"))
            .or_else(|| extract_attribute(header, "details"))
            .or_else(|| extract_attribute(header, "note"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if msg.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                msg = Some(body.to_string());
            }
        }
        last_error = Some(msg.unwrap_or_else(|| "Execution error".to_string()));
        pos = abs_end;
    }
    last_error
}

/// Extracts the final terminal outcome (`<success>` or `<error>`) from model output.
pub fn extract_terminal_outcome(text: &str) -> Option<TerminalOutcome> {
    let mut outcomes: Vec<(usize, TerminalOutcome)> = Vec::new();

    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut summary = extract_attribute(header, "summary")
            .or_else(|| extract_attribute(header, "note"))
            .or_else(|| extract_attribute(header, "message"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if summary.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                summary = Some(body.to_string());
            }
        }
        outcomes.push((abs_start, TerminalOutcome::Success { summary }));
        pos = abs_end;
    }

    pos = 0;
    while let Some((start, end)) = next_error_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut msg = extract_attribute(header, "message")
            .or_else(|| extract_attribute(header, "reason"))
            .or_else(|| extract_attribute(header, "details"))
            .or_else(|| extract_attribute(header, "note"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if msg.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                msg = Some(body.to_string());
            }
        }
        outcomes.push((
            abs_start,
            TerminalOutcome::Error {
                message: msg.unwrap_or_else(|| "Execution error".to_string()),
            },
        ));
        pos = abs_end;
    }

    outcomes.sort_by_key(|(start, _)| *start);
    outcomes.pop().map(|(_, outcome)| outcome)
}

/// Removes all `<success>` tags and content from text.
pub fn strip_success_tags(text: &str) -> String {
    strip_tags(text, next_success_tag)
}

/// Removes all `<error>` tags and content from text.
pub fn strip_error_tags(text: &str) -> String {
    strip_tags(text, next_error_tag)
}

/// Removes all `<success>` and `<error>` outcome tags from text.
pub fn strip_terminal_outcome_tags(text: &str) -> String {
    strip_error_tags(&strip_success_tags(text))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlanTag {
    pub action: String,
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub concept: Vec<tauqe_protocol::ConceptItem>,
    pub items: Vec<tauqe_protocol::PlanItem>,
}

fn find_closing_inner_tag(
    slice: &str,
    search_pos: usize,
    base_name: &str,
    open_tag_name: &str,
) -> Option<(usize, usize)> {
    let prefix = format!("</{base_name}");
    let mut pos = search_pos;
    while let Some(rel) = slice[pos..].find(&prefix) {
        let close_start = pos + rel;
        let after_prefix = &slice[close_start + prefix.len()..];
        if let Some(gt) = after_prefix.find('>') {
            let suffix = after_prefix[..gt].trim();
            let open_suffix = &open_tag_name[base_name.len()..];
            let is_match = suffix.is_empty()
                || suffix == open_suffix
                || (open_suffix.is_empty()
                    && !suffix.starts_with("_item")
                    && !suffix.starts_with("-item")
                    && (suffix.starts_with('_') || suffix.starts_with('-'))
                    && suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));

            if is_match {
                let close_end = close_start + prefix.len() + gt + 1;
                return Some((close_start, close_end));
            }
        }
        pos = close_start + prefix.len();
    }
    None
}

fn extract_and_strip_inner_tag(text: &str, base_name: &str) -> (Option<String>, String) {
    if let Some((start, tag_name)) = find_base_tag_start(text, base_name) {
        let slice = &text[start..];
        if let Some(open_end) = slice.find('>') {
            if slice[..open_end].trim_end().ends_with('/') {
                let full_end = start + open_end + 1;
                let mut remaining = String::with_capacity(text.len() - (full_end - start));
                remaining.push_str(&text[..start]);
                remaining.push_str(&text[full_end..]);
                return (None, remaining);
            }
            if let Some((close_start_in_slice, close_end_in_slice)) =
                find_closing_inner_tag(slice, open_end + 1, base_name, &tag_name)
            {
                let content = slice[open_end + 1..close_start_in_slice].trim().to_string();
                let full_end = start + close_end_in_slice;
                let mut remaining = String::with_capacity(text.len() - (full_end - start));
                remaining.push_str(&text[..start]);
                remaining.push_str(&text[full_end..]);
                return (Some(content), remaining);
            }
        }
    }
    (None, text.to_string())
}

fn next_item_tag_bounds(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = text[from..].find("<item") {
        let start = from + rel;
        let after = start + "<item".len();
        let bytes = text.as_bytes();
        let mut idx = after;
        while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
        }
        let rest = &text[idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            from = after;
            continue;
        }
        let gt = rest.find('>')?;
        let header_end = idx + gt + 1;
        if rest[..gt].trim_end().ends_with('/') {
            return Some((start, header_end));
        }
        let close_tag = "</item";
        let mut depth = 1;
        let mut search_pos = header_end;
        while search_pos < text.len() {
            let next_open = text[search_pos..].find("<item");
            let next_close = text[search_pos..].find(close_tag);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let tag_header = &text[search_pos + o..];
                    if let Some(hgt) = tag_header.find('>') {
                        if !tag_header[..hgt].trim_end().ends_with('/') {
                            depth += 1;
                        }
                        search_pos += o + hgt + 1;
                    } else {
                        break;
                    }
                }
                (_, Some(c)) => {
                    let close_start = search_pos + c;
                    let close_rest = &text[close_start..];
                    if let Some(close_gt) = close_rest.find('>') {
                        depth -= 1;
                        if depth == 0 {
                            return Some((start, close_start + close_gt + 1));
                        }
                        search_pos = close_start + close_gt + 1;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        return Some((start, header_end));
    }
    None
}

fn next_concept_item_bounds(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while from < text.len() {
        let next_c = text[from..].find("<concept_item");
        let next_i = text[from..].find("<item");
        let (rel, open_len) = match (next_c, next_i) {
            (Some(c), Some(i)) if c <= i => (c, "<concept_item".len()),
            (Some(_), Some(i)) => (i, "<item".len()),
            (Some(c), None) => (c, "<concept_item".len()),
            (None, Some(i)) => (i, "<item".len()),
            (None, None) => break,
        };

        let start = from + rel;
        let after = start + open_len;
        let bytes = text.as_bytes();
        let mut idx = after;
        while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
        }
        let rest = &text[idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            from = after;
            continue;
        }
        let gt = rest.find('>')?;
        let header_end = idx + gt + 1;
        if rest[..gt].trim_end().ends_with('/') {
            return Some((start, header_end));
        }

        let tag_name = &text[start + 1..idx];
        let open_needle = format!("<{tag_name}");
        let close_needle = format!("</{tag_name}");
        let mut depth = 1;
        let mut search_pos = header_end;
        while search_pos < text.len() {
            let next_open = text[search_pos..].find(&open_needle);
            let next_close = text[search_pos..].find(&close_needle);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let tag_header = &text[search_pos + o..];
                    if let Some(hgt) = tag_header.find('>') {
                        if !tag_header[..hgt].trim_end().ends_with('/') {
                            depth += 1;
                        }
                        search_pos += o + hgt + 1;
                    } else {
                        break;
                    }
                }
                (_, Some(c)) => {
                    let close_start = search_pos + c;
                    let close_rest = &text[close_start..];
                    if let Some(close_gt) = close_rest.find('>') {
                        let inside = &close_rest[close_needle.len()..close_gt];
                        if inside.trim().is_empty() {
                            depth -= 1;
                            if depth == 0 {
                                return Some((start, close_start + close_gt + 1));
                            }
                        }
                        search_pos = close_start + close_gt + 1;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        return Some((start, header_end));
    }
    None
}

fn parse_concept_items_with_diagnostics(content: &str) -> (Vec<tauqe_protocol::ConceptItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    let needle_concept = "<concept_item";
    let needle_item = "<item";

    while pos < content.len() {
        let next_c = content[pos..].find(needle_concept);
        let next_i = content[pos..].find(needle_item);
        let (rel, open_len) = match (next_c, next_i) {
            (Some(c), Some(i)) if c <= i => (c, needle_concept.len()),
            (Some(_), Some(i)) => (i, needle_item.len()),
            (Some(c), None) => (c, needle_concept.len()),
            (None, Some(i)) => (i, needle_item.len()),
            (None, None) => break,
        };

        let start = pos + rel;
        let after = start + open_len;
        let bytes = &content.as_bytes()[after..];
        let mut idx = 0;
        while idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-' || bytes[idx].is_ascii_alphanumeric()) {
            idx += 1;
        }
        let rest = &content[after + idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push("Malformed concept item tag: unclosed opening tag".to_string());
            break;
        };
        let header_end = after + idx + gt + 1;
        let header = &content[start..header_end];
        let id = extract_attribute(header, "id").unwrap_or_default().trim().to_string();
        let raw_title = extract_attribute(header, "title").unwrap_or_default().trim().to_string();
        let attr_details = extract_attribute(header, "details")
            .or_else(|| extract_attribute(header, "description"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if rest[..gt].trim_end().ends_with('/') {
            if !id.is_empty() || !raw_title.is_empty() {
                items.push(tauqe_protocol::ConceptItem {
                    id: if id.is_empty() { format!("c-{}", items.len() + 1) } else { id },
                    title: raw_title,
                    details: attr_details,
                    children: Vec::new(),
                });
            }
            pos = header_end;
        } else {
            let tag_name = &content[start + 1..after + idx];
            let open_needle = format!("<{tag_name}");
            let close_needle = format!("</{tag_name}");
            let mut depth = 1;
            let mut search_pos = header_end;
            let mut close_start = None;
            let mut close_end = None;

            while search_pos < content.len() {
                let next_open = content[search_pos..].find(&open_needle);
                let next_close = content[search_pos..].find(&close_needle);

                match (next_open, next_close) {
                    (Some(o), Some(c)) if o < c => {
                        let tag_header = &content[search_pos + o..];
                        if let Some(hgt) = tag_header.find('>') {
                            if !tag_header[..hgt].trim_end().ends_with('/') {
                                depth += 1;
                            }
                            search_pos += o + hgt + 1;
                        } else {
                            break;
                        }
                    }
                    (_, Some(c)) => {
                        let close_tag_start = search_pos + c;
                        let close_rest = &content[close_tag_start..];
                        if let Some(cgt) = close_rest.find('>') {
                            let inside = &close_rest[close_needle.len()..cgt];
                            if inside.trim().is_empty() {
                                depth -= 1;
                                if depth == 0 {
                                    close_start = Some(close_tag_start);
                                    close_end = Some(close_tag_start + cgt + 1);
                                    break;
                                }
                            }
                            search_pos = close_tag_start + cgt + 1;
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }

            if let (Some(cend), Some(cend_full)) = (close_start, close_end) {
                let body = &content[header_end..cend];
                let (children, child_errors) = parse_concept_items_with_diagnostics(body);
                errors.extend(child_errors);
                let text_only = strip_tags(body, next_concept_item_bounds);

                let (inner_title, text_without_title) = extract_and_strip_inner_tag(&text_only, "title");
                let mut title = raw_title;
                if title.is_empty() {
                    if let Some(it) = inner_title {
                        title = it;
                    }
                }

                let (inner_details, text_without_details) = {
                    let (d, rem) = extract_and_strip_inner_tag(&text_without_title, "details");
                    if d.is_some() {
                        (d, rem)
                    } else {
                        extract_and_strip_inner_tag(&text_without_title, "description")
                    }
                };

                let mut details = inner_details.or(attr_details).or_else(|| {
                    let trimmed = text_without_details.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });

                if title.is_empty() {
                    if let Some(d) = details.take() {
                        if let Some((first, rest)) = d.split_once('\n') {
                            title = first.trim().to_string();
                            let rest_trimmed = rest.trim();
                            if !rest_trimmed.is_empty() {
                                details = Some(rest_trimmed.to_string());
                            }
                        } else {
                            title = d;
                        }
                    }
                }

                if !id.is_empty() || !title.is_empty() {
                    items.push(tauqe_protocol::ConceptItem {
                        id: if id.is_empty() { format!("c-{}", items.len() + 1) } else { id.clone() },
                        title: if title.is_empty() { id } else { title },
                        details,
                        children,
                    });
                }
                pos = cend_full;
            } else {
                errors.push(format!("Malformed concept item tag: missing closing tag </{tag_name}>"));
                pos = header_end;
            }
        }
    }

    if items.is_empty() && !content.trim().is_empty() {
        let fallback_items = parse_concept_items_fallback(content);
        if !fallback_items.is_empty() {
            return (fallback_items, errors);
        }
    }

    (items, errors)
}

fn parse_fallback_item_header(text: &str, fallback_index: usize) -> (String, String, Option<String>) {
    let raw = text.trim();
    let mut words = raw.split_whitespace();
    let first_word = words.next().unwrap_or("");
    let clean_first = first_word.trim_matches(['[', ']', '(', ')', ':', ',']);

    let (id, remainder) = if clean_first.starts_with("#c-") || clean_first.starts_with("#c_") {
        let id_val = clean_first.trim_start_matches('#');
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        (id_val.to_string(), rem)
    } else if clean_first.starts_with("c-") || clean_first.starts_with("c_") {
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        (clean_first.to_string(), rem)
    } else if clean_first.starts_with('#')
        && clean_first.strip_prefix('#').unwrap_or("").chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && clean_first.len() > 1
    {
        let id_val = clean_first.trim_start_matches('#');
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        let formatted_id = if id_val.starts_with("c-") {
            id_val.to_string()
        } else {
            format!("c-{}", id_val)
        };
        (formatted_id, rem)
    } else {
        (format!("c-{}", fallback_index), raw)
    };

    let (title, details) = if let Some((t, d)) = remainder.split_once(": ") {
        (t.trim(), Some(d.trim().to_string()))
    } else if let Some((t, d)) = remainder.split_once(" - ") {
        (t.trim(), Some(d.trim().to_string()))
    } else {
        (remainder.trim(), None)
    };

    let title_clean = title.trim_matches(['*', '_', '`']).trim();
    let final_title = if title_clean.is_empty() {
        id.clone()
    } else {
        title_clean.to_string()
    };

    (id, final_title, details.filter(|s| !s.is_empty()))
}

fn parse_concept_items_fallback(content: &str) -> Vec<tauqe_protocol::ConceptItem> {
    let mut items = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut current_item: Option<tauqe_protocol::ConceptItem> = None;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_new_item = if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            Some(rest.trim())
        } else if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            after_digits.strip_prefix(". ").map(|after_dot| after_dot.trim())
        } else if trimmed.starts_with("#c-")
            || trimmed.starts_with("c-")
            || trimmed.starts_with("#c_")
            || trimmed.starts_with("c_")
        {
            Some(trimmed)
        } else {
            None
        };

        if let Some(item_text) = is_new_item {
            if let Some(prev) = current_item.take() {
                items.push(prev);
            }

            let (id, title, inline_details) =
                parse_fallback_item_header(item_text, items.len() + 1);
            current_item = Some(tauqe_protocol::ConceptItem {
                id,
                title,
                details: inline_details,
                children: Vec::new(),
            });
        } else if let Some(ref mut cur) = current_item {
            if let Some(ref mut d) = cur.details {
                d.push('\n');
                d.push_str(trimmed);
            } else {
                cur.details = Some(trimmed.to_string());
            }
        }
    }

    if let Some(last) = current_item {
        items.push(last);
    }

    if items.is_empty() && !content.trim().is_empty() {
        let trimmed = content.trim();
        let (first_line, rest) = match trimmed.split_once('\n') {
            Some((f, r)) => (f.trim(), Some(r.trim())),
            None => (trimmed, None),
        };
        let title = if first_line.chars().count() > 80 {
            let truncated: String = first_line.chars().take(77).collect();
            format!("{}...", truncated)
        } else {
            first_line.to_string()
        };
        let details = match rest {
            Some(r) if !r.is_empty() => Some(r.to_string()),
            _ => {
                if first_line != title {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            }
        };
        items.push(tauqe_protocol::ConceptItem {
            id: "c-1".to_string(),
            title,
            details,
            children: Vec::new(),
        });
    }

    items
}

fn parse_plan_items_with_diagnostics(content: &str) -> (Vec<tauqe_protocol::PlanItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    while let Some(rel) = content[pos..].find("<item") {
        let start = pos + rel;
        let after = start + "<item".len();
        let bytes = &content.as_bytes()[after..];
        let mut idx = 0;
        while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
        }
        let rest = &content[after + idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push("Malformed <item> tag: unclosed opening tag".to_string());
            break;
        };
        let header_end = after + idx + gt + 1;
        let header = &content[start..header_end];
        let id = extract_attribute(header, "id").unwrap_or_default().trim().to_string();
        let raw_title = extract_attribute(header, "title").unwrap_or_default().trim().to_string();
        let status_str = extract_attribute(header, "status").unwrap_or_default();
        let status = match status_str.trim().to_lowercase().as_str() {
            "discussion" | "discuss" | "question" | "?" | "обсуждение" => tauqe_protocol::PlanItemStatus::Discussion,
            "in_progress" | "inprogress" | "doing" => tauqe_protocol::PlanItemStatus::InProgress,
            "done" | "completed" | "finished" => tauqe_protocol::PlanItemStatus::Done,
            "cancelled" | "canceled" => tauqe_protocol::PlanItemStatus::Cancelled,
            "todo" | "pending" | "" => tauqe_protocol::PlanItemStatus::Todo,
            other => {
                errors.push(format!("Unrecognized status '{other}' for item '{}', defaulting to 'todo'", if id.is_empty() { &raw_title } else { &id }));
                tauqe_protocol::PlanItemStatus::Todo
            }
        };

        let attr_details = extract_attribute(header, "details")
            .or_else(|| extract_attribute(header, "description"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let files = extract_attribute(header, "files")
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim().trim_start_matches("./").trim_start_matches('/');
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let concept_refs = extract_attribute(header, "concept_refs")
            .or_else(|| extract_attribute(header, "concept"))
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if rest[..gt].trim_end().ends_with('/') {
            // Self-closing item
            if !id.is_empty() || !raw_title.is_empty() {
                items.push(tauqe_protocol::PlanItem {
                    id: if id.is_empty() { format!("{}", items.len() + 1) } else { id.clone() },
                    title: raw_title,
                    details: attr_details,
                    status,
                    files,
                    concept_refs,
                    children: Vec::new(),
                });
            }
            pos = header_end;
        } else {
            // Paired tag: find matching </item> (supporting optional turn marker suffix)
            let mut depth = 1;
            let mut search_pos = header_end;
            let mut close_start = None;
            let mut close_end = None;

            while search_pos < content.len() {
                let next_open = content[search_pos..].find("<item");
                let next_close = content[search_pos..].find("</item");

                match (next_open, next_close) {
                    (Some(o), Some(c)) if o < c => {
                        let tag_header = &content[search_pos + o..];
                        if let Some(hgt) = tag_header.find('>') {
                            if !tag_header[..hgt].trim_end().ends_with('/') {
                                depth += 1;
                            }
                            search_pos += o + hgt + 1;
                        } else {
                            break;
                        }
                    }
                    (_, Some(c)) => {
                        let close_tag_start = search_pos + c;
                        let close_rest = &content[close_tag_start..];
                        if let Some(cgt) = close_rest.find('>') {
                            depth -= 1;
                            if depth == 0 {
                                close_start = Some(close_tag_start);
                                close_end = Some(close_tag_start + cgt + 1);
                                break;
                            }
                            search_pos = close_tag_start + cgt + 1;
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }

            if let (Some(cend), Some(cend_full)) = (close_start, close_end) {
                let body = &content[header_end..cend];
                let (children, child_errors) = parse_plan_items_with_diagnostics(body);
                errors.extend(child_errors);
                let text_only = strip_tags(body, next_item_tag_bounds);

                let (inner_title, text_without_title) = extract_and_strip_inner_tag(&text_only, "title");
                let mut title = raw_title;
                if title.is_empty() {
                    if let Some(it) = inner_title {
                        title = it;
                    }
                }

                let (inner_details, text_without_details) = {
                    let (d, rem) = extract_and_strip_inner_tag(&text_without_title, "details");
                    if d.is_some() {
                        (d, rem)
                    } else {
                        extract_and_strip_inner_tag(&text_without_title, "description")
                    }
                };

                let mut details = inner_details
                    .or(attr_details)
                    .or_else(|| {
                        let trimmed = text_without_details.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    });

                if title.is_empty() {
                    if let Some(d) = details.take() {
                        if let Some((first, rest)) = d.split_once('\n') {
                            title = first.trim().to_string();
                            let rest_trimmed = rest.trim();
                            if !rest_trimmed.is_empty() {
                                details = Some(rest_trimmed.to_string());
                            }
                        } else {
                            title = d;
                        }
                    }
                }

                if !id.is_empty() || !title.is_empty() {
                    items.push(tauqe_protocol::PlanItem {
                        id: if id.is_empty() { format!("{}", items.len() + 1) } else { id.clone() },
                        title: if title.is_empty() { id } else { title },
                        details,
                        status,
                        files,
                        concept_refs,
                        children,
                    });
                }
                pos = cend_full;
            } else {
                errors.push(format!("Malformed <item> tag: missing closing tag </item> for item '{}'", if id.is_empty() { &raw_title } else { &id }));
                pos = header_end;
            }
        }
    }

    (items, errors)
}

/// Parses `<plan ...>...</plan>` blocks and collects syntax and structural diagnostics.
pub fn parse_plan_tags_with_diagnostics(text: &str) -> (Vec<ParsedPlanTag>, Vec<String>) {
    let mut plans = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    while let Some(rel) = text[pos..].find("<plan") {
        let tag_start = pos + rel;
        let after_plan = tag_start + "<plan".len();
        let bytes = text.as_bytes();
        let mut idx = after_plan;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
                idx += 1;
            }
        }
        let tag_name = &text[tag_start + 1..idx];
        let rest = &text[idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after_plan;
            continue;
        }

        if is_inside_markdown_code(text, tag_start) {
            pos = after_plan;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push(format!("Malformed <{tag_name}> tag: unclosed opening tag"));
            break;
        };

        let header_end = idx + gt + 1;
        let header = &text[tag_start..header_end];
        let is_self_closing = rest[..gt].trim_end().ends_with('/');

        let raw_action_opt = extract_attribute(header, "action");
        let raw_id_opt = extract_attribute(header, "id");
        let raw_title_opt = extract_attribute(header, "title");

        let has_plan_attributes = raw_action_opt.is_some() || raw_id_opt.is_some() || raw_title_opt.is_some();

        let (body, next_pos) = if is_self_closing {
            ("", header_end)
        } else {
            let close_tag = format!("</{tag_name}>");
            if let Some(close_offset) = text[header_end..].find(&close_tag) {
                let body_slice = &text[header_end..header_end + close_offset];
                (body_slice, header_end + close_offset + close_tag.len())
            } else {
                if has_plan_attributes {
                    errors.push(format!("Malformed <{tag_name}> tag: missing closing tag '{close_tag}'"));
                }
                pos = header_end;
                continue;
            }
        };

        let raw_action = raw_action_opt.unwrap_or_else(|| "save".to_string());
        let action = match raw_action.trim().to_lowercase().as_str() {
            "save" | "create" => "save".to_string(),
            "update" | "modify" | "edit" => "update".to_string(),
            "delete" | "remove" => "delete".to_string(),
            other => {
                errors.push(format!("Unknown plan action '{other}' in <{tag_name}> (supported actions: save, update, delete)"));
                other.to_string()
            }
        };

        let id = raw_id_opt
            .unwrap_or_default()
            .trim()
            .trim_matches('\\')
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_string();

        let (concept_block, body_without_concept) = extract_and_strip_inner_tag(body, "concept");
        let (concept, concept_errors) = if let Some(ref c_str) = concept_block {
            parse_concept_items_with_diagnostics(c_str)
        } else {
            (Vec::new(), Vec::new())
        };
        errors.extend(concept_errors);

        let plan_meta = strip_tags(&body_without_concept, next_item_tag_bounds);

        let title = raw_title_opt
            .or_else(|| {
                let (t, _) = extract_and_strip_inner_tag(&plan_meta, "title");
                t
            })
            .map(|s| s.trim().to_string());

        let description = extract_attribute(header, "description")
            .or_else(|| {
                let (d, _) = extract_and_strip_inner_tag(&plan_meta, "description");
                d.or_else(|| {
                    let (s, _) = extract_and_strip_inner_tag(&plan_meta, "summary");
                    s
                })
            });

        let (mut items, item_errors) = parse_plan_items_with_diagnostics(&body_without_concept);
        errors.extend(item_errors);

        if action == "save" {
            for item in &mut items {
                if item.title.is_empty() {
                    item.title = item.id.clone();
                }
            }
        } else if action == "update" && items.is_empty() && title.is_none() && description.is_none() && has_plan_attributes {
            errors.push(format!("Plan update for '{}' contains no items or fields to update", if id.is_empty() { tag_name } else { id.as_str() }));
        }

        if id.is_empty() && items.is_empty() && concept.is_empty() {
            if has_plan_attributes {
                errors.push(format!("Malformed <{tag_name}> tag: both 'id' and items are missing"));
            }
        } else if action == "save" && concept.is_empty() {
            if concept_block.is_some() {
                errors.push(format!(
                    "Plan '{}': <concept> section is present but empty or invalid; define architectural concepts using <concept_item id=\"c-...\" title=\"...\"> or bullet points",
                    if id.is_empty() { tag_name } else { id.as_str() }
                ));
            } else {
                errors.push(format!(
                    "Plan '{}' must contain a <concept> section defining architectural intent and invariants",
                    if id.is_empty() { tag_name } else { id.as_str() }
                ));
            }
        } else {
            plans.push(ParsedPlanTag {
                action,
                id: if id.is_empty() { "default".to_string() } else { id },
                title,
                description,
                concept,
                items,
            });
        }

        pos = next_pos;
    }

    (plans, errors)
}

pub(super) fn parse_context_request_tags(text: &str) -> Vec<ContextRequest> {
    let mut requests = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_context_request_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        if let Some(path) = extract_attribute(header, "path") {
            let path = path
                .trim()
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            if !path.is_empty() {
                let editable = extract_attribute(header, "access")
                    .is_some_and(|a| a.trim().eq_ignore_ascii_case("editable"));
                requests.push(ContextRequest {
                    path,
                    access: if editable {
                        ContextAccess::Editable
                    } else {
                        ContextAccess::ReadOnly
                    },
                });
            }
        }
        pos += end;
    }
    requests
}

pub fn parse_context_drop_tags(text: &str) -> Vec<String> {
    let mut drops = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_context_drop_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        if let Some(path) = extract_attribute(header, "path") {
            let path = path
                .trim()
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            if !path.is_empty() && !drops.contains(&path) {
                drops.push(path);
            }
        }
        pos += end;
    }
    drops
}

/// Returns the requested documentation topics (lowercase); a missing topic means `all`.
pub(super) fn parse_doc_request_tags(text: &str) -> Vec<String> {
    let mut topics = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_doc_request_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let topic = extract_attribute(&tag[..header_end], "topic")
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "all".to_string());
        topics.push(topic);
        pos += end;
    }
    topics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_plan_tags(text: &str) -> Vec<ParsedPlanTag> {
        parse_plan_tags_with_diagnostics(text).0
    }

    #[test]
    fn test_parse_and_strip_context_drop_tags() {
        let text = "Dropping:\n<context_drop path=\"crates/a.rs\" />\nMiddle\n<context_drop_ABC path=\"crates/b.rs\"></context_drop_ABC>\nDone.";
        assert_eq!(
            parse_context_drop_tags(text),
            vec!["crates/a.rs".to_string(), "crates/b.rs".to_string()]
        );
        assert_eq!(
            strip_context_drop_tags(text).trim(),
            "Dropping:\n\nMiddle\n\nDone."
        );
    }

    #[test]
    fn test_parse_doc_request_tags() {
        let text = "Need docs.\n<doc_request topic=\"Interface\" />\n<doc_request_AB12 />";
        assert_eq!(
            parse_doc_request_tags(text),
            vec!["interface".to_string(), "all".to_string()]
        );
    }

    #[test]
    fn test_strip_doc_request_tags() {
        let text = "Intro\n<doc_request topic=\"git\" />\nOutro";
        assert_eq!(strip_doc_request_tags(text).trim(), "Intro\n\nOutro");
    }

    #[test]
    fn test_strip_plan_tags() {
        let text = "Intro\n<plan id=\"test\" title=\"T\">\n<item id=\"1\" title=\"Step 1\" />\n</plan>\nOutro";
        assert_eq!(strip_plan_tags(text).trim(), "Intro\n\nOutro");
    }

    #[test]
    fn test_parse_plan_tags() {
        let text = r#"
Here is our plan:
<plan action="save" id="jwt-auth" title="JWT Auth">
  <summary>Migrate to tokens</summary>
  <concept>
    <concept_item id="c-1" title="Token Security" />
  </concept>
  <item id="1" title="Data models" status="done">Define User and Claims</item>
  <item id="2" title="Handlers" status="in_progress">
    <item id="2.1" title="Refresh handler" status="todo" />
  </item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.action, "save");
        assert_eq!(p.id, "jwt-auth");
        assert_eq!(p.title.as_deref(), Some("JWT Auth"));
        assert_eq!(p.description.as_deref(), Some("Migrate to tokens"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].status, tauqe_protocol::PlanItemStatus::Done);
        assert_eq!(p.items[0].details.as_deref(), Some("Define User and Claims"));
        assert_eq!(p.items[1].children.len(), 1);
        assert_eq!(p.items[1].children[0].id, "2.1");
    }

    #[test]
    fn test_parse_plan_tags_item_attributes_details() {
        let text = r#"
<plan action="save" id="test-attr" title="Test Attr">
  <concept>
    <concept_item id="c-1" title="Attribute Concept" />
  </concept>
  <item id="1" title="Self closing" status="todo" details="Attribute details here" />
  <item id="2" title="Paired tag" status="discussion" details="Paired with attr"></item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].concept.len(), 1);
        assert_eq!(parsed[0].items[0].details.as_deref(), Some("Attribute details here"));
        assert_eq!(parsed[0].items[1].details.as_deref(), Some("Paired with attr"));
    }

    #[test]
    fn test_parse_plan_tags_requires_concept_on_save() {
        let text = r#"
<plan action="save" id="flat-plan" title="Flat Plan">
  <item id="1" title="Step without concept" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty(), "Plan without concept should not be accepted on save");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must contain a <concept> section"));
    }

    #[test]
    fn test_parse_concept_fallback_long_cyrillic_no_panic() {
        let text = r#"
<plan action="save" id="cyrillic-plan" title="Тест">
  <concept>
    Это очень длинная строка с кириллическим текстом, которая превышает лимит восемьдесят символов и раньше вызывала панику
  </concept>
  <item id="1" title="Шаг 1" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].concept.len(), 1);
        assert!(plans[0].concept[0].title.ends_with("..."));
    }

    #[test]
    fn test_parse_plan_tags_with_markdown_concept_fallback() {
        let text = r#"
<plan action="save" id="md-concept" title="Markdown Concept">
  <concept>
    - #c-1: Verification Symmetry
      Preflight verification runs the exact same checks.
    - #c-2 Fail-Early: Abort immediately on broken build.
  </concept>
  <item id="1" title="Execute check" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.concept.len(), 2);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.concept[0].title, "Verification Symmetry");
        assert_eq!(
            p.concept[0].details.as_deref(),
            Some("Preflight verification runs the exact same checks.")
        );
        assert_eq!(p.concept[1].id, "c-2");
        assert_eq!(p.concept[1].title, "Fail-Early");
        assert_eq!(
            p.concept[1].details.as_deref(),
            Some("Abort immediately on broken build.")
        );
    }

    #[test]
    fn test_parse_plan_tags_turn_marker_tolerance_on_inner_tag() {
        let text = r#"
<plan action="save" id="suffixed" title="Suffixed">
  <concept>
    - #c-1 Invariant One
  </concept>
  <item id="1" title="Step 1" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].concept.len(), 1);
        assert_eq!(plans[0].concept[0].id, "c-1");
        assert_eq!(plans[0].concept[0].title, "Invariant One");
    }

    #[test]
    fn test_parse_plan_tags_concept_present_but_empty_diagnostic() {
        let text = r#"
<plan action="save" id="empty-concept" title="Empty Concept">
  <concept>   </concept>
  <item id="1" title="Step" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("is present but empty or invalid"));
    }

    #[test]
    fn test_parse_plan_tags_ignores_bare_mentions() {
        let text = "Here we discuss <plan> in text without closing tags or attributes.";
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn test_next_simple_tag_ignores_code_fences_and_inline_backticks() {
        let text = "Here is an example:\n```xml\n<context_request path=\"fence.rs\" />\n```\nAnd inline `<context_request path=\"inline.rs\" />`.\nReal:\n<context_request path=\"real.rs\" />";
        let reqs = parse_context_request_tags(text);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "real.rs");

        let doc_text = "See `<doc_request topic=\"git\" />`.\n<doc_request topic=\"interface\" />";
        let topics = parse_doc_request_tags(doc_text);
        assert_eq!(topics, vec!["interface".to_string()]);
    }

    #[test]
    fn test_parse_plan_tags_ignores_markdown_fences() {
        let text = r#"
Here is an example plan in markdown:
```markdown
<plan action="save" id="jwt-auth" title="JWT Auth">
  <concept>
    <concept_item id="c-1" title="Ex Concept" />
  </concept>
  <item id="1" title="Example item" />
</plan>
```
And outside markdown:
<plan action="save" id="real-plan" title="Real Plan">
  <concept>
    <concept_item id="c-1" title="Real Concept" />
  </concept>
  <item id="1" title="Real item" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "real-plan");
    }

    #[test]
    fn test_strip_plan_tags_preserves_markdown_fences() {
        let text = "Example:\n```xml\n<plan id=\"example\" title=\"Ex\" />\n```\nReal:\n<plan id=\"real\" title=\"Real\" />\nDone.";
        let stripped = strip_plan_tags(text);
        assert!(stripped.contains("<plan id=\"example\""));
        assert!(!stripped.contains("<plan id=\"real\""));
    }

    #[test]
    fn test_extract_and_strip_scratchpad() {
        let text = r#"
Introductory text
<scratchpad>
Working hypothesis: missing imports in protocol.rs.
Need to inspect protocol/xml/tags.rs.
</scratchpad>
Conversational explanation.
<scratchpad note="Secondary note" />
Final remarks.
"#;
        let extracted = extract_scratchpad(text);
        assert_eq!(
            extracted.as_deref(),
            Some("Working hypothesis: missing imports in protocol.rs.\nNeed to inspect protocol/xml/tags.rs.\n\nSecondary note")
        );

        let stripped = strip_scratchpad_tags(text);
        assert!(!stripped.contains("<scratchpad"));
        assert!(!stripped.contains("Working hypothesis"));
        assert!(stripped.contains("Introductory text"));
        assert!(stripped.contains("Final remarks."));
    }

    #[test]
    fn test_extract_scratchpad_ignores_markdown_code() {
        let text = r#"
Example:
```xml
<scratchpad>
Ignore this code fence note
</scratchpad>
```
Inline `<scratchpad>Ignore inline</scratchpad>`
Real:
<scratchpad>
Real working note
</scratchpad>
"#;
        let extracted = extract_scratchpad(text);
        assert_eq!(extracted.as_deref(), Some("Real working note"));

        let stripped = strip_scratchpad_tags(text);
        assert!(stripped.contains("Ignore this code fence note"));
        assert!(stripped.contains("`<scratchpad>Ignore inline</scratchpad>`"));
        assert!(!stripped.contains("Real working note"));
    }

    #[test]
    fn test_parse_plan_tags_with_suffixed_tags_and_description() {
        let text = r#"
<plan_18DCD1DA6EB762E1 action="save" id="plan-exec" title="Execution Engine">
  <description_18DCD1DA6EB762E1>
    Architectural plan for isolated step execution.
    Contains multiple paragraphs of context.
  </description_18DCD1DA6EB762E1>
  <concept_18DCD1DA6EB762E1>
    <concept_item_18DCD1DA6EB762E1 id="c-1" title="Execution Harness" />
  </concept_18DCD1DA6EB762E1>
  <item_18DCD1DA6EB762E1 id="1" status="done">
    <title_18DCD1DA6EB762E1>Protocol tags</title_18DCD1DA6EB762E1>
    <details_18DCD1DA6EB762E1>Step done marker</details_18DCD1DA6EB762E1>
  </item_18DCD1DA6EB762E1>
  <item_18DCD1DA6EB762E1 id="2" title="UI Integration" status="todo" />
</plan_18DCD1DA6EB762E1>
"#;
        let (parsed, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.id, "plan-exec");
        assert_eq!(p.title.as_deref(), Some("Execution Engine"));
        assert!(p.description.as_ref().unwrap().contains("Architectural plan"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].status, tauqe_protocol::PlanItemStatus::Done);
        assert_eq!(p.items[0].title, "Protocol tags");
        assert_eq!(p.items[1].id, "2");
        assert_eq!(p.items[1].status, tauqe_protocol::PlanItemStatus::Todo);
    }

    #[test]
    fn test_parse_plan_tags_with_concept_and_files() {
        let text = r#"
<plan action="save" id="two-section" title="Two-Section Plan">
  <concept>
    <concept_item id="c-1" title="Architectural intent">
      <details>Core invariant details</details>
      <concept_item id="c-1.1" title="Sub invariant" />
    </concept_item>
  </concept>
  <item id="1" title="Implementation step" status="todo" files="crates/core/src/lib.rs,crates/protocol/src/plan.rs" concept_refs="c-1,c-1.1" />
</plan>
"#;
        let (parsed, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.concept[0].details.as_deref(), Some("Core invariant details"));
        assert_eq!(p.concept[0].children.len(), 1);
        assert_eq!(p.concept[0].children[0].id, "c-1.1");
        assert_eq!(p.items.len(), 1);
        assert_eq!(
            p.items[0].files,
            vec!["crates/core/src/lib.rs", "crates/protocol/src/plan.rs"]
        );
        assert_eq!(p.items[0].concept_refs, vec!["c-1", "c-1.1"]);
    }

    #[test]
    fn test_parse_plan_tags_nested_elements() {
        let text = r#"
<plan action="save" id="rust-hello-world">
  <title>Rust Hello World</title>
  <description>Complete binary creation</description>
  <concept>
    <concept_item id="c-1" title="Binary Entrypoint Invariant" />
  </concept>
  <item id="1" status="todo">
    <title>Create package manifest and binary entrypoint</title>
    <details>Setup Cargo.toml and src/main.rs entrypoint for the application</details>
  </item>
  <item id="2" status="todo">
    <title>Compile and run</title>
    Run cargo run to verify
  </item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.id, "rust-hello-world");
        assert_eq!(p.title.as_deref(), Some("Rust Hello World"));
        assert_eq!(p.description.as_deref(), Some("Complete binary creation"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].title, "Create package manifest and binary entrypoint");
        assert_eq!(p.items[0].details.as_deref(), Some("Setup Cargo.toml and src/main.rs entrypoint for the application"));
        assert_eq!(p.items[1].id, "2");
        assert_eq!(p.items[1].title, "Compile and run");
        assert_eq!(p.items[1].details.as_deref(), Some("Run cargo run to verify"));
    }

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
    fn test_parse_plan_tags_ignores_legacy_item_tier() {
        let text = r#"
<plan action="save" id="tiers" title="Tiers">
  <concept>
    <concept_item id="c-1" title="Tiers Concept" />
  </concept>
  <item id="1" title="Cheap" tier="junior" />
  <item id="2" title="Default" status="todo">Body</item>
  <item id="3" title="Hard" tier="Senior"></item>
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        let items = &plans[0].items;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "1");
        assert_eq!(items[1].id, "2");
        assert_eq!(items[2].id, "3");
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
    }
}
