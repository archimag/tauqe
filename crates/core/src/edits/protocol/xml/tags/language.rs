//! Parsers for the `<user_language>` control tag.

use super::{is_inside_markdown_code, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

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
    let (start, end) = next_user_language_tag(text)?;
    let tag = &text[start..end];
    let open_end = tag.find('>')?;
    let header = &tag[..=open_end];
    if let Some(val) = extract_attribute(header, "value")
        .or_else(|| extract_attribute(header, "lang"))
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() && trimmed.len() < 50 && !trimmed.contains('\n') {
            return Some(trimmed.to_string());
        }
    }
    let body_start = open_end + 1;
    let close_start = tag[body_start..].find("</")?;
    let content = tag[body_start..body_start + close_start].trim();
    if !content.is_empty() && content.len() < 50 && !content.contains('\n') {
        return Some(content.to_string());
    }
    None
}

/// Removes all user_language tags from text.
pub fn strip_user_language_tags(text: &str) -> String {
    strip_tags(text, next_user_language_tag)
}
