//! Edit-block parsing and tag scanning for the XML edit protocol.
//!
//! This module turns raw edit markup into ordered [`EditOperation`] values and
//! provides the low-level tag scanner shared with conversational-text cleaning.
//! The wrapper container, the file blocks, and the hunks are named by the
//! capability registry and resolved once per process, so no scanner here
//! re-declares a tag spelling of its own.

use std::sync::OnceLock;

use tauqe_protocol::{EditFileOp, EditOperation, LlmCapability};

use super::engine::TagCloseMode;
use crate::capabilities::CapabilityRegistry;
use crate::edits::protocol::utils::{normalize_content, resolve_target_path_for_op};

/// One whole-file block of the code-modification contour.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileBlockTag {
    /// Base name of the block.
    pub(crate) name: &'static str,
    /// Operation the block realizes.
    pub(crate) op: EditFileOp,
    /// Whether the block takes effect only when its header names a target.
    ///
    /// A spelling shared with a hunk block must carry an explicit target path,
    /// otherwise a hunk opener or a tag merely quoted in prose would open a
    /// whole-file block.
    pub(crate) requires_target: bool,
}

/// Tag vocabulary of the code-modification contour.
#[derive(Debug, Default)]
pub(crate) struct FileEditTags {
    /// Wrapper container names: the canonical tag, then its accepted aliases.
    pub(crate) containers: Vec<&'static str>,
    /// File blocks accepted inside a container, in declaration order.
    pub(crate) file_blocks: Vec<FileBlockTag>,
    /// Hunk block names in grammar order: the search block first, then the
    /// replacement block and its accepted spellings.
    pub(crate) hunks: Vec<&'static str>,
}

impl FileEditTags {
    /// Iterates every base name that may open an edit construct.
    ///
    /// Containers come first, so a caller that removes blocks in iteration
    /// order removes a wrapper before the blocks it contains.
    pub(crate) fn opening_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.containers
            .iter()
            .copied()
            .chain(self.file_blocks.iter().map(|block| block.name))
    }

    /// Returns the search block name and the replacement block spellings.
    pub(crate) fn hunk_spellings(&self) -> (Option<&'static str>, &[&'static str]) {
        match self.hunks.split_first() {
            Some((search, replacements)) => (Some(*search), replacements),
            None => (None, &[]),
        }
    }
}

static FILE_EDIT_TAGS: OnceLock<FileEditTags> = OnceLock::new();

/// Returns the resolved vocabulary of the code-modification contour.
///
/// The canonical edit capability tag and its aliases name the wrapper
/// containers, the wrapper's declared children name the file blocks, and the
/// children of the hunk-holding block name the hunks. A spelling declared as
/// both a hunk and an edit operation names a file block only when its header
/// carries a target, so a hunk opener never opens a whole-file block.
pub(crate) fn file_edit_tags() -> &'static FileEditTags {
    FILE_EDIT_TAGS.get_or_init(|| {
        let registry = CapabilityRegistry::global();
        let Some(module) = registry.module(LlmCapability::FileEdits) else {
            tracing::error!("the capability registry declares no file edit module");
            return FileEditTags::default();
        };
        let Some(canonical) = module.canonical_tag() else {
            tracing::error!("the file edit module declares no canonical tag");
            return FileEditTags::default();
        };
        let Some(container) = registry.tag_spec(canonical) else {
            tracing::error!(tag = canonical, "the file edit container tag is undeclared");
            return FileEditTags::default();
        };
        let mut containers = vec![canonical];
        containers.extend(module.tag_aliases().iter().copied());
        let mut file_blocks = Vec::new();
        let mut hunks: Vec<&'static str> = Vec::new();
        for &name in container.child_tags {
            match registry.tag_spec(name) {
                Some(spec) if spec.close_mode == TagCloseMode::Container => {
                    if hunks.is_empty() {
                        hunks.extend_from_slice(spec.child_tags);
                    }
                    file_blocks.push(FileBlockTag {
                        name,
                        op: EditFileOp::Replace,
                        requires_target: false,
                    });
                }
                _ => match name.parse::<EditFileOp>() {
                    Ok(op) => file_blocks.push(FileBlockTag {
                        name,
                        op,
                        requires_target: false,
                    }),
                    Err(_) => tracing::warn!(tag = name, "file block realizes no edit operation"),
                },
            }
        }
        for &name in &hunks {
            if let Ok(op) = name.parse::<EditFileOp>() {
                if !file_blocks.iter().any(|block| block.name == name) {
                    file_blocks.push(FileBlockTag {
                        name,
                        op,
                        requires_target: true,
                    });
                }
            }
        }
        FileEditTags {
            containers,
            file_blocks,
            hunks,
        }
    })
}

/// Reports whether the text contains any XML edit tag.
pub fn has_xml_edit_tags(text: &str) -> bool {
    file_edit_tags()
        .opening_names()
        .any(|name| text.contains(&format!("<{}", name)))
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
        let Some((block, offset)) = find_next_edit_tag(remaining) else {
            break;
        };
        let tag_start = pos + offset;
        let tag_slice = &text[tag_start..];
        let open_tag_end = tag_slice
            .find('>')
            .ok_or_else(|| format!("Unterminated opening tag at offset {}", tag_start))?;
        let header = &tag_slice[..open_tag_end];
        let closing_tag = format!("</{}>", block.name);
        let self_closing = header.trim_end().ends_with('/');

        match block.op {
            EditFileOp::Replace => {
                let path_attr = extract_attribute(header, "path")
                    .ok_or_else(|| format!("Missing 'path' attribute in tag: {}", header))?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                let tags = file_edit_tags();
                let (search, replacements) = tags.hunk_spellings();
                let search = search.ok_or_else(|| {
                    format!(
                        "The edit block for file '{}' declares no search block",
                        target_path
                    )
                })?;

                let body = &tag_slice[open_tag_end + 1..];
                let mut inner_pos = 0;
                let mut found_any = false;

                while let Some(search_start) = find_tag_start(&body[inner_pos..], search) {
                    if let Some(close_idx) = body[inner_pos..].find(&closing_tag) {
                        if close_idx < search_start {
                            break;
                        }
                    }

                    if let Some((_, next_edit_start)) = find_next_edit_tag(&body[inner_pos..]) {
                        if next_edit_start < search_start {
                            break;
                        }
                    }

                    let (search_content, after_search) = find_tag(&body[inner_pos..], search)
                        .ok_or_else(|| {
                            format!("Malformed <{}> tag in file '{}'", search, target_path)
                        })?;
                    let search_abs_end = inner_pos + after_search;
                    let remaining_inner = &body[search_abs_end..];

                    let (replace_content, after_replace) = replacements
                        .iter()
                        .find_map(|name| find_tag(remaining_inner, name))
                        .ok_or_else(|| {
                            format!(
                                "Missing replacement block after <{}> in file '{}'",
                                search, target_path
                            )
                        })?;

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
                        "Tag for file '{}' must contain <{}> and a replacement block",
                        target_path, search
                    ));
                }

                let close_offset = body[inner_pos..].find(&closing_tag).ok_or_else(|| {
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
            EditFileOp::Create | EditFileOp::Overwrite => {
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!(
                        "Missing 'path' attribute in <{}> tag: {}",
                        block.name, header
                    )
                })?;
                let target_path = resolve_target_path_for_op(
                    &path_attr,
                    editable_paths,
                    block.op == EditFileOp::Create,
                )?;

                let close_idx = tag_slice.find(&closing_tag).ok_or_else(|| {
                    format!("Missing {} tag for file '{}'", closing_tag, target_path)
                })?;

                let content = normalize_content(&tag_slice[open_tag_end + 1..close_idx]);
                if block.op == EditFileOp::Create {
                    edits.push(EditOperation::Create {
                        path: target_path,
                        content,
                    });
                } else {
                    edits.push(EditOperation::Overwrite {
                        path: target_path,
                        content,
                    });
                }

                pos = tag_start + close_idx + closing_tag.len();
            }
            EditFileOp::Move => {
                let from_attr = extract_attribute(header, "from")
                    .or_else(|| extract_attribute(header, "path"))
                    .ok_or_else(|| {
                        format!(
                            "Missing 'from' or 'path' attribute in <{}> tag: {}",
                            block.name, header
                        )
                    })?;
                let to_attr = extract_attribute(header, "to").ok_or_else(|| {
                    format!("Missing 'to' attribute in <{}> tag: {}", block.name, header)
                })?;

                let target_from = resolve_target_path_for_op(&from_attr, editable_paths, false)?;
                let target_to = resolve_target_path_for_op(&to_attr, editable_paths, true)?;

                pos = if self_closing {
                    tag_start + open_tag_end + 1
                } else if let Some(close_idx) = tag_slice.find(&closing_tag) {
                    tag_start + close_idx + closing_tag.len()
                } else {
                    tag_start + open_tag_end + 1
                };

                edits.push(EditOperation::Move {
                    from: target_from,
                    to: target_to,
                });
            }
            EditFileOp::Delete => {
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!(
                        "Missing 'path' attribute in <{}> tag: {}",
                        block.name, header
                    )
                })?;
                let target_path = resolve_target_path_for_op(&path_attr, editable_paths, false)?;

                pos = if self_closing {
                    tag_start + open_tag_end + 1
                } else if let Some(close_idx) = tag_slice.find(&closing_tag) {
                    tag_start + close_idx + closing_tag.len()
                } else {
                    tag_start + open_tag_end + 1
                };

                edits.push(EditOperation::Delete { path: target_path });
            }
        }
    }

    Ok(edits)
}

/// Locates the earliest file block that opens an edit construct.
fn find_next_edit_tag(text: &str) -> Option<(FileBlockTag, usize)> {
    let mut earliest: Option<(FileBlockTag, usize)> = None;
    for block in &file_edit_tags().file_blocks {
        let Some(idx) = find_tag_start(text, block.name) else {
            continue;
        };
        if block.requires_target && !header_names_target(text, idx) {
            continue;
        }
        if earliest.is_none_or(|(_, current)| idx < current) {
            earliest = Some((*block, idx));
        }
    }
    earliest
}

/// Reports whether the tag header opening at `tag_start` names a target path.
fn header_names_target(text: &str, tag_start: usize) -> bool {
    text[tag_start..].find('>').is_some_and(|end| {
        let header = &text[tag_start..tag_start + end];
        header.contains("path") || header.contains("from")
    })
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
    // 1. Try finding the summary attribute of an edit container.
    for container in &file_edit_tags().containers {
        let Some((idx, _)) = find_base_tag_start(text, container) else {
            continue;
        };
        let Some(end) = text[idx..].find('>') else {
            continue;
        };
        if let Some(summary) = extract_attribute(&text[idx..idx + end], "summary") {
            if !summary.trim().is_empty() {
                return Some(summary.trim().to_string());
            }
        }
    }

    // 2. Try finding <summary>...</summary> tag
    find_tag(text, "summary").and_then(|(content, _)| {
        let clean = content.trim();
        (!clean.is_empty()).then(|| clean.to_string())
    })
}
