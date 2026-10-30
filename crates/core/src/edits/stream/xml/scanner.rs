//! Low-level XML tag boundary, hunk, and attribute scanning helpers.
//!
//! These routines operate purely on the accumulated stream buffer: they locate
//! edit-block boundaries, delimit hunks, and extract tag attributes. Every edit
//! tag spelling they look for is resolved
//! from the capability registry, so no scanner carries a literal edit tag of
//! its own. Keeping them separate leaves all state transitions to
//! [`XmlStreamFilter`](super::XmlStreamFilter).

use crate::edits::protocol::xml::{file_edit_tags, FileBlockTag};
use crate::edits::stream::normalize_hunk;

/// Re-activates unmarked hidden control tags that normalization protected as
/// literal text, so the stream filter still suppresses them from the
/// user-facing surface when the model omits the turn-marker suffix.
///
/// The implementation lives in the shared tag engine beside
/// `strip_all_control_tags`, so the streaming contour and the final cleaner
/// derive from one implementation and hide exactly the same tags.
pub(super) use crate::edits::protocol::xml::engine::activate_unmarked_hidden_tags;

/// Reports whether the buffer opens with an edit container tag.
pub(super) fn starts_with_edit_container(buf: &str) -> bool {
    file_edit_tags()
        .containers
        .iter()
        .any(|name| buf.starts_with(&format!("<{}", name)))
}

/// Locates the earliest closing tag of an edit container.
pub(super) fn find_container_close(buf: &str) -> Option<(usize, usize)> {
    let mut earliest: Option<(usize, usize)> = None;
    for name in &file_edit_tags().containers {
        let close = format!("</{}>", name);
        if let Some(idx) = buf.find(&close) {
            if earliest.is_none_or(|(current, _)| idx < current) {
                earliest = Some((idx, close.len()));
            }
        }
    }
    earliest
}

/// Finds the next opening of `name` that forms a tag header.
///
/// A candidate must follow the base name with a tag boundary. When
/// `requires_target` is set, the header must also name a target or source path,
/// so a tag merely quoted in prose never starts an edit.
fn find_named_tag(buf: &str, name: &str, requires_target: bool) -> Option<usize> {
    let pattern = format!("<{}", name);
    let mut from = 0;
    while let Some(offset) = buf[from..].find(&pattern) {
        let start = from + offset;
        let after = &buf[start + pattern.len()..];
        let boundary = after
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/' || c == '_' || c == '-');
        if boundary {
            if let Some(end) = buf[start..].find('>') {
                let header = &buf[start..start + end + 1];
                if !requires_target || header.contains("path=") || header.contains("from=") {
                    return Some(start);
                }
            }
        }
        from = start + pattern.len();
    }
    None
}

/// Locates the earliest file block that opens inside an edit container.
pub(super) fn find_file_tag_start(buf: &str) -> Option<(&'static FileBlockTag, usize)> {
    let mut earliest: Option<(&'static FileBlockTag, usize)> = None;
    for block in &file_edit_tags().file_blocks {
        if let Some(idx) = find_named_tag(buf, block.name, true) {
            if earliest.is_none_or(|(_, current)| idx < current) {
                earliest = Some((block, idx));
            }
        }
    }
    earliest
}

/// Locates the next complete hunk of a hunk-holding file block.
///
/// The search block name and the accepted replacement spellings come from the
/// resolved edit vocabulary, so a hunk is delimited exactly as the decoder
/// reads it.
pub(super) fn find_next_hunk(buf: &str) -> Option<(String, String, usize)> {
    let tags = file_edit_tags();
    let (search, replacements) = tags.hunk_spellings();
    let search = search?;

    let search_open = format!("<{}>", search);
    let search_close = format!("</{}>", search);
    let s_start = buf.find(&search_open)?;
    let s_end = buf[s_start + search_open.len()..].find(&search_close)?;
    let old_text_raw = &buf[s_start + search_open.len()..s_start + search_open.len() + s_end];

    let after_search = s_start + search_open.len() + s_end + search_close.len();
    let rest = &buf[after_search..];

    let (offset, name) = replacements
        .iter()
        .filter_map(|name| {
            let open = format!("<{}>", name);
            rest.find(&open).map(|offset| (offset, *name))
        })
        .min_by_key(|(offset, _)| *offset)?;

    let replace_open = format!("<{}>", name);
    let replace_close = format!("</{}>", name);
    let r_end = rest[offset + replace_open.len()..].find(&replace_close)?;
    let new_text_raw = &rest[offset + replace_open.len()..offset + replace_open.len() + r_end];

    let abs_end = after_search + offset + replace_open.len() + r_end + replace_close.len();

    Some((
        normalize_hunk(old_text_raw),
        normalize_hunk(new_text_raw),
        abs_end,
    ))
}

// Streamed tag headers are decoded with the same quote and attribute-name
// boundary rules as the final XML parser, so the parser owns the logic.
pub(super) use crate::edits::protocol::xml::extract_attribute as extract_attr;
