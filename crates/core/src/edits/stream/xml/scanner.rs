//! Low-level XML tag boundary, hunk, and attribute scanning helpers.
//!
//! These routines operate purely on the accumulated stream buffer: they locate
//! edit-block boundaries, delimit hunks, extract tag attributes, and compute
//! safe text emission limits. Keeping them separate leaves all state
//! transitions to [`XmlStreamFilter`](super::XmlStreamFilter).

use crate::edits::stream::normalize_hunk;

/// Re-activates unmarked `<scratchpad>` tags that normalization protected as
/// literal text. Working memory is never user-facing, so the stream filter must
/// suppress it even when the model omits the turn-marker suffix. The protection
/// prefix is produced by [`normalize_marked_xml_incremental`](crate::edits::protocol::xml::normalize_marked_xml_incremental).
pub(super) fn activate_unmarked_scratchpad(normalized: &str, marker: &str) -> String {
    let protection = format!("\u{e000}{}\u{e001}", marker);
    if !normalized.contains(&protection) {
        return normalized.to_string();
    }
    normalized
        .replace(&format!("{}/scratchpad", protection), "</scratchpad")
        .replace(&format!("{}scratchpad", protection), "<scratchpad")
}

pub(super) fn find_next_control_tag(buf: &str) -> Option<(usize, usize)> {
    let mut candidates = Vec::new();
    if let Some(cr) = crate::edits::protocol::xml::next_context_request_tag(buf) {
        candidates.push(cr);
    }
    if let Some(ul) = crate::edits::protocol::xml::next_user_language_tag(buf) {
        candidates.push(ul);
    }
    if let Some(vf) = crate::edits::protocol::xml::next_verify_tag(buf) {
        candidates.push(vf);
    }
    if let Some(dr) = crate::edits::protocol::xml::tags::next_doc_request_tag(buf) {
        candidates.push(dr);
    }
    if let Some(pl) = crate::edits::protocol::xml::tags::next_plan_tag(buf) {
        candidates.push(pl);
    }
    if let Some(sc) = crate::edits::protocol::xml::tags::next_scratchpad_tag(buf) {
        candidates.push(sc);
    }
    if let Some(suc) = crate::edits::protocol::xml::tags::next_success_tag(buf) {
        candidates.push(suc);
    }
    if let Some(err) = crate::edits::protocol::xml::tags::next_error_tag(buf) {
        candidates.push(err);
    }
    if let Some(blk) = crate::edits::protocol::xml::tags::next_blocked_tag(buf) {
        candidates.push(blk);
    }
    candidates.into_iter().min_by_key(|(start, _)| *start)
}

pub(super) fn safe_text_emit_len(buf: &str) -> usize {
    const CONTROL_PREFIXES: &[&str] = &[
        "<tauqe_edits",
        "<edit",
        "<create",
        "<delete",
        "<move",
        "<overwrite",
        "<context_request",
        "<doc_request",
        "<user_language",
        "<verify",
        "<plan",
        "<scratchpad",
        "<success",
        "<error",
        "<blocked",
    ];
    let mut limit = buf.len();
    if let Some(last_lt) = buf.rfind('<') {
        let trailing = &buf[last_lt..];
        for prefix in CONTROL_PREFIXES {
            if prefix.starts_with(trailing) {
                limit = limit.min(last_lt);
            }
        }
    }
    for prefix in CONTROL_PREFIXES {
        if let Some(idx) = buf.rfind(prefix) {
            let rest = &buf[idx + prefix.len()..];
            let boundary = rest
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/' || c == '_' || c == '-');
            if boundary {
                let is_closed = match *prefix {
                    "<user_language" => crate::edits::protocol::xml::next_user_language_tag(&buf[idx..]).is_some(),
                    "<verify" => crate::edits::protocol::xml::next_verify_tag(&buf[idx..]).is_some(),
                    "<context_request" => crate::edits::protocol::xml::next_context_request_tag(&buf[idx..]).is_some(),
                    "<doc_request" => crate::edits::protocol::xml::tags::next_doc_request_tag(&buf[idx..]).is_some(),
                    "<plan" => crate::edits::protocol::xml::tags::next_plan_tag(&buf[idx..]).is_some(),
                    "<scratchpad" => crate::edits::protocol::xml::tags::next_scratchpad_tag(&buf[idx..]).is_some(),
                    "<success" => crate::edits::protocol::xml::tags::next_success_tag(&buf[idx..]).is_some(),
                    "<error" => crate::edits::protocol::xml::tags::next_error_tag(&buf[idx..]).is_some(),
                    "<blocked" => crate::edits::protocol::xml::tags::next_blocked_tag(&buf[idx..]).is_some(),
                    _ => rest.contains('>'),
                };
                if !is_closed {
                    limit = limit.min(idx);
                }
            }
        }
    }
    limit
}

pub(super) fn find_any_edit_start(buf: &str) -> Option<usize> {
    let mut min_idx = None;

    if let Some(idx) = buf.find("<tauqe_edits") {
        let after = &buf[idx + "<tauqe_edits".len()..];
        if after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '_' || c == '-') {
            min_idx = Some(idx);
        }
    }

    for tag in &["edit", "create", "overwrite", "delete", "move"] {
        let pat = format!("<{}", tag);
        let mut from = 0;
        while let Some(idx) = buf[from..].find(&pat) {
            let abs_idx = from + idx;
            let after = &buf[abs_idx + pat.len()..];
            let mut is_tag_match = false;
            if after.starts_with(char::is_whitespace) {
                is_tag_match = true;
            } else if after.starts_with(['_', '-']) {
                if let Some(ws_idx) = after.find(char::is_whitespace) {
                    let suffix = &after[1..ws_idx];
                    if suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
                        is_tag_match = true;
                    }
                }
            }

            if is_tag_match {
                if let Some(gt_idx) = buf[abs_idx..].find('>') {
                    let header = &buf[abs_idx..abs_idx + gt_idx + 1];
                    let has_req_attr = if *tag == "move" {
                        header.contains("from=") || header.contains("path=")
                    } else {
                        header.contains("path=")
                    };
                    if has_req_attr {
                        min_idx = Some(min_idx.map_or(abs_idx, |m: usize| m.min(abs_idx)));
                        break;
                    }
                }
            }
            from = abs_idx + pat.len();
        }
    }
    min_idx
}

pub(super) fn find_file_tag_start(buf: &str) -> Option<(String, usize)> {
    let mut candidates = Vec::new();
    for tag in &["edit", "create", "overwrite", "delete", "replace", "move"] {
        let pat = format!("<{}", tag);
        let mut from = 0;
        while let Some(idx) = buf[from..].find(&pat) {
            let abs_idx = from + idx;
            let after = &buf[abs_idx + pat.len()..];
            if let Some(c) = after.chars().next() {
                if c.is_whitespace() || c == '>' {
                    candidates.push((tag.to_string(), abs_idx));
                    break;
                }
            }
            from = abs_idx + pat.len();
        }
    }
    candidates.into_iter().min_by_key(|(_, idx)| *idx)
}

pub(super) fn find_next_hunk(buf: &str) -> Option<(String, String, usize)> {
    let search_start_pat = "<search>";
    let search_end_pat = "</search>";
    let s_start = buf.find(search_start_pat)?;
    let s_end = buf[s_start + search_start_pat.len()..].find(search_end_pat)?;
    let old_text_raw =
        &buf[s_start + search_start_pat.len()..s_start + search_start_pat.len() + s_end];

    let after_search = s_start + search_start_pat.len() + s_end + search_end_pat.len();
    let rest = &buf[after_search..];

    let (replace_start_pat, replace_end_pat) = if rest.contains("<replace>") {
        ("<replace>", "</replace>")
    } else if rest.contains("<with>") {
        ("<with>", "</with>")
    } else {
        return None;
    };

    let r_start = rest.find(replace_start_pat)?;
    let r_end = rest[r_start + replace_start_pat.len()..].find(replace_end_pat)?;
    let new_text_raw =
        &rest[r_start + replace_start_pat.len()..r_start + replace_start_pat.len() + r_end];

    let abs_end = after_search + r_start + replace_start_pat.len() + r_end + replace_end_pat.len();

    Some((
        normalize_hunk(old_text_raw),
        normalize_hunk(new_text_raw),
        abs_end,
    ))
}

pub(super) fn extract_attr(header: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(pos) = header[search_from..].find(attr) {
        let abs_pos = search_from + pos;
        let after_attr = &header[abs_pos + attr.len()..];
        let trimmed = after_attr.trim_start();
        if let Some(after_eq) = trimmed.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            let quote = after_eq.chars().next()?;
            if quote == '"' || quote == '\'' {
                let rest = &after_eq[quote.len_utf8()..];
                let end_quote = rest.find(quote)?;
                return Some(rest[..end_quote].trim().trim_matches('`').to_string());
            }
        }
        search_from = abs_pos + attr.len();
    }
    None
}
