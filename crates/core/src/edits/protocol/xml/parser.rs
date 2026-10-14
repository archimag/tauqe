//! Edit-block parsing and tag scanning for the XML edit protocol.
//!
//! This module turns raw `<tauqe_edits>` markup into ordered
//! [`EditOperation`] values and provides the low-level tag scanner shared with
//! conversational-text cleaning.

use tauqe_protocol::EditOperation;

use crate::edits::protocol::utils::{normalize_content, resolve_target_path_for_op};

/// Reports whether the text contains any XML edit tag.
pub fn has_xml_edit_tags(text: &str) -> bool {
    text.contains("<edit")
        || text.contains("<replace")
        || text.contains("<create")
        || text.contains("<delete")
        || text.contains("<move")
        || text.contains("<overwrite")
        || text.contains("<tauqe_edits")
}

/// Parses XML edit blocks into ordered edit operations.
pub(super) fn parse_xml_edits(
    text: &str,
    editable_paths: &[String],
) -> Result<Vec<EditOperation>, String> {
    let mut edits = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let remaining = &text[pos..];

        let next_tag = find_next_edit_tag(remaining);
        let (tag_type, offset) = match next_tag {
            Some(res) => res,
            None => break,
        };

        let tag_start = pos + offset;
        let tag_slice = &text[tag_start..];

        match tag_type {
            TagType::Edit | TagType::Replace => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated opening tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path")
                    .ok_or_else(|| format!("Missing 'path' attribute in tag: {}", header))?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                let closing_tag = if tag_type == TagType::Edit {
                    "</edit>"
                } else {
                    "</replace>"
                };

                let body = &tag_slice[open_tag_end + 1..];
                let mut inner_pos = 0;
                let mut found_any = false;

                while let Some(search_start) = find_tag_start(&body[inner_pos..], "search") {
                    if let Some(c_idx) = body[inner_pos..].find(closing_tag) {
                        if c_idx < search_start {
                            break;
                        }
                    }

                    if let Some((_, next_edit_start)) = find_next_edit_tag(&body[inner_pos..]) {
                        if next_edit_start < search_start {
                            break;
                        }
                    }

                    let (search_content, after_search) = find_tag(&body[inner_pos..], "search")
                        .ok_or_else(|| {
                            format!(
                                "Malformed <search> tag in file '{}'",
                                target_path
                            )
                        })?;
                    let search_abs_end = inner_pos + after_search;
                    let remaining_inner = &body[search_abs_end..];

                    let (replace_content, after_replace) =
                        if let Some(res) = find_tag(remaining_inner, "replace") {
                            res
                        } else if let Some(res) = find_tag(remaining_inner, "with") {
                            res
                        } else {
                            return Err(format!(
                                "Missing <replace> or <with> tag after <search> in file '{}'",
                                target_path
                            ));
                        };

                    edits.push(EditOperation::Replace {
                        path: target_path.clone(),
                        old_text: normalize_content(search_content),
                        new_text: normalize_content(replace_content),
                    });

                    found_any = true;
                    inner_pos = search_abs_end + after_replace;
                }

                if !found_any {
                    return Err(format!(
                        "Tag for file '{}' must contain <search> and <replace> blocks",
                        target_path
                    ));
                }

                let close_offset = body[inner_pos..].find(closing_tag).ok_or_else(|| {
                    format!(
                        "Missing closing tag '{}' for file '{}'",
                        closing_tag, target_path
                    )
                })?;

                if let Some((_, next_edit_start)) = find_next_edit_tag(&body[inner_pos..]) {
                    if next_edit_start < close_offset {
                        return Err(format!(
                            "Missing closing tag '{}' for file '{}'",
                            closing_tag, target_path
                        ));
                    }
                }

                pos = tag_start + open_tag_end + 1 + inner_pos + close_offset + closing_tag.len();
            }
            TagType::Create => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <create> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <create> tag: {}", header)
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, true)?;

                let close_idx = tag_slice
                    .find("</create>")
                    .ok_or_else(|| format!("Missing </create> tag for file '{}'", target_path))?;

                let content = &tag_slice[open_tag_end + 1..close_idx];
                edits.push(EditOperation::Create {
                    path: target_path,
                    content: normalize_content(content),
                });

                pos = tag_start + close_idx + "</create>".len();
            }
            TagType::Overwrite => {
                let open_tag_end = tag_slice.find('>').ok_or_else(|| {
                    format!("Unterminated <overwrite> tag at offset {}", tag_start)
                })?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <overwrite> tag: {}", header)
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                let close_idx = tag_slice.find("</overwrite>").ok_or_else(|| {
                    format!("Missing </overwrite> tag for file '{}'", target_path)
                })?;

                let content = &tag_slice[open_tag_end + 1..close_idx];
                edits.push(EditOperation::Overwrite {
                    path: target_path,
                    content: normalize_content(content),
                });

                pos = tag_start + close_idx + "</overwrite>".len();
            }
            TagType::Move => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <move> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let from_attr = extract_attribute(header, "from")
                    .or_else(|| extract_attribute(header, "path"))
                    .ok_or_else(|| format!("Missing 'from' or 'path' attribute in <move> tag: {}", header))?;
                let to_attr = extract_attribute(header, "to")
                    .ok_or_else(|| format!("Missing 'to' attribute in <move> tag: {}", header))?;

                let target_from = resolve_target_path_for_op(&from_attr, editable_paths, false)?;
                let target_to = resolve_target_path_for_op(&to_attr, editable_paths, true)?;

                if header.ends_with('/') {
                    pos = tag_start + open_tag_end + 1;
                } else if let Some(close_idx) = tag_slice.find("</move>") {
                    pos = tag_start + close_idx + "</move>".len();
                } else {
                    pos = tag_start + open_tag_end + 1;
                }

                edits.push(EditOperation::Move {
                    from: target_from,
                    to: target_to,
                });
            }
            TagType::Delete => {
                let open_tag_end = tag_slice
                    .find('>')
                    .ok_or_else(|| format!("Unterminated <delete> tag at offset {}", tag_start))?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <delete> tag: {}", header)
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                if header.ends_with('/') {
                    pos = tag_start + open_tag_end + 1;
                } else if let Some(close_idx) = tag_slice.find("</delete>") {
                    pos = tag_start + close_idx + "</delete>".len();
                } else {
                    pos = tag_start + open_tag_end + 1;
                }

                edits.push(EditOperation::Delete { path: target_path });
            }
        }
    }

    Ok(edits)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TagType {
    Edit,
    Replace,
    Create,
    Delete,
    Move,
    Overwrite,
}

fn find_next_edit_tag(text: &str) -> Option<(TagType, usize)> {
    let mut candidates = Vec::new();

    if let Some(idx) = find_tag_start(text, "edit") {
        candidates.push((TagType::Edit, idx));
    }
    if let Some(idx) = find_tag_start(text, "replace") {
        if let Some(end_bracket) = text[idx..].find('>') {
            let header = &text[idx..idx + end_bracket];
            if header.contains("path") {
                candidates.push((TagType::Replace, idx));
            }
        }
    }
    if let Some(idx) = find_tag_start(text, "create") {
        candidates.push((TagType::Create, idx));
    }
    if let Some(idx) = find_tag_start(text, "overwrite") {
        candidates.push((TagType::Overwrite, idx));
    }
    if let Some(idx) = find_tag_start(text, "delete") {
        candidates.push((TagType::Delete, idx));
    }
    if let Some(idx) = find_tag_start(text, "move") {
        candidates.push((TagType::Move, idx));
    }

    candidates.into_iter().min_by_key(|(_, idx)| *idx)
}

/// Locates an opening tag by base name with support for optional turn marker suffixes (e.g. `<edit_18DC...>`).
/// Returns the start position and the full matched tag name.
pub(crate) fn find_base_tag_start(text: &str, tag_base: &str) -> Option<(usize, String)> {
    let prefix = format!("<{}", tag_base);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&prefix) {
        let abs_idx = search_from + idx;
        let after = &text[abs_idx + prefix.len()..];
        let bytes = after.as_bytes();
        let mut i = 0;
        if i < bytes.len() && (bytes[i] == b'_' || bytes[i] == b'-') {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
        }
        let rest = &after[i..];
        if let Some(c) = rest.chars().next() {
            if c.is_whitespace() || c == '>' || c == '/' {
                let full_tag = text[abs_idx + 1..abs_idx + prefix.len() + i].to_string();
                return Some((abs_idx, full_tag));
            }
        }
        search_from = abs_idx + prefix.len();
    }
    None
}

fn find_tag_start(text: &str, tag_name: &str) -> Option<usize> {
    find_base_tag_start(text, tag_name).map(|(idx, _)| idx)
}

fn find_tag<'a>(text: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    let open_prefix = format!("<{}", tag);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&open_prefix) {
        let abs_start = search_from + idx;
        let after = &text[abs_start + open_prefix.len()..];
        if let Some(c) = after.chars().next() {
            if c == '>' || c.is_whitespace() {
                if let Some(close_bracket) = after.find('>') {
                    let content_start = abs_start + open_prefix.len() + close_bracket + 1;
                    let close_tag = format!("</{}>", tag);
                    if let Some(end_idx) = text[content_start..].find(&close_tag) {
                        let content = &text[content_start..content_start + end_idx];
                        let next_pos = content_start + end_idx + close_tag.len();
                        return Some((content, next_pos));
                    }
                }
            }
        }
        search_from = abs_start + open_prefix.len();
    }
    None
}

pub(crate) fn extract_attribute(tag_header: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(pos) = tag_header[search_from..].find(attr) {
        let abs_pos = search_from + pos;
        let before_ok = abs_pos == 0
            || tag_header[..abs_pos]
                .chars()
                .last()
                .map(|c| c.is_whitespace() || c == '<')
                .unwrap_or(false);
        let after_attr = &tag_header[abs_pos + attr.len()..];
        let trimmed = after_attr.trim_start();
        if before_ok && trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
            if after_eq.is_empty() {
                return None;
            }
            let quote_char = after_eq.chars().next()?;
            if quote_char == '"' || quote_char == '\'' {
                let rest = &after_eq[quote_char.len_utf8()..];
                let end_quote = rest.find(quote_char)?;
                let val = &rest[..end_quote];
                return Some(val.trim().trim_matches('`').to_string());
            } else if after_eq.starts_with("\\\"") || after_eq.starts_with("\\'") {
                let quote_str = &after_eq[..2];
                let rest = &after_eq[2..];
                if let Some(end_quote) = rest.find(quote_str) {
                    let val = &rest[..end_quote];
                    return Some(val.trim().trim_matches('`').to_string());
                }
            } else {
                let val: String = after_eq
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
                    .collect();
                if !val.is_empty() {
                    return Some(
                        val.trim_matches('`')
                            .trim_matches('\\')
                            .trim_matches('"')
                            .trim_matches('\'')
                            .to_string(),
                    );
                }
            }
        }
        search_from = abs_pos + attr.len();
    }
    None
}

pub(crate) fn extract_summary_from_xml(text: &str) -> Option<String> {
    // 1. Try finding attribute summary="..." in <tauqe_edits ...>
    if let Some(idx) = text.find("<tauqe_edits") {
        if let Some(end) = text[idx..].find('>') {
            let header = &text[idx..idx + end];
            if let Some(s) = extract_attribute(header, "summary") {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
        }
    }

    // 2. Try finding <summary>...</summary> tag
    if let Some((content, _)) = find_tag(text, "summary") {
        let clean = content.trim();
        if !clean.is_empty() {
            return Some(clean.to_string());
        }
    }

    None
}
