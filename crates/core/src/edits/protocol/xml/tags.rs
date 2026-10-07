//! Control tags of the XML edit protocol that are not code edits:
//! `<context_request>`, `<user_language>` and `<verify>`.

use tauqe_protocol::ContextAccess;

use super::{extract_attribute, find_base_tag_start};
use crate::edits::protocol::ContextRequest;

/// Locates the next complete `<context_request .../>` tag (unsuffixed dialect).
/// Returns the byte range `(start, end)` including an immediately following
/// `</context_request>` for the paired form. Incomplete tags yield None.
pub(crate) fn next_context_request_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<context_request";
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

/// Locates the next complete `<verify .../>` or `<verify>...</verify>` tag.
pub(crate) fn next_verify_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<verify";
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
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = next_verify_tag(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Removes all context request tags from text.
pub fn strip_context_request_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = next_context_request_tag(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
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
