use std::collections::HashMap;
use std::path::PathBuf;

use super::{current_file_content, normalize_hunk, resolve_path, EditStreamFilter};
use crate::providers::StreamEvent;

/// State-machine streaming parser for XML edits.
///
/// It intercepts XML edit blocks and emits semantic `StreamEvent::Edit*` events
/// while forwarding regular conversational text as `StreamEvent::TextDelta`.
pub struct XmlStreamFilter {
    buffer: String,
    marker: Option<String>,
    raw_buffer: String,
    normalized_bytes: usize,
    in_edits_block: bool,
    current_file: Option<CurrentStreamingFile>,
    file_hunks_count: usize,
    editable_paths: Vec<String>,
    repo_root: PathBuf,
    staged_contents: HashMap<String, String>,
}

struct CurrentStreamingFile {
    path: String,
    op_type: String,
}

impl XmlStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            buffer: String::new(),
            marker: None,
            raw_buffer: String::new(),
            normalized_bytes: 0,
            in_edits_block: false,
            current_file: None,
            file_hunks_count: 0,
            editable_paths,
            repo_root,
            staged_contents: HashMap::new(),
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staged_contents = staged_contents;
        self
    }

    /// Sets the active XML marker before any chunks are processed.
    pub fn with_marker(mut self, marker: Option<String>) -> Self {
        assert!(self.raw_buffer.is_empty() && self.buffer.is_empty());
        if let Some(value) = &marker {
            assert!(!value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
        self.marker = marker;
        self
    }

    fn restore_content(&self, text: &str) -> String {
        match &self.marker {
            Some(marker) => crate::edits::protocol::xml::restore_xml_literals(text, marker),
            None => text.to_string(),
        }
    }

    fn restore_events(&self, events: &mut [StreamEvent]) {
        for event in events {
            match event {
                StreamEvent::TextDelta(text) => *text = self.restore_content(text),
                StreamEvent::EditFileStarted { path, .. }
                | StreamEvent::EditFileDone { path, .. } => {
                    *path = self.restore_content(path);
                }
                StreamEvent::EditHunk { path, old_text, new_text, .. } => {
                    *path = self.restore_content(path);
                    *old_text = self.restore_content(old_text);
                    *new_text = self.restore_content(new_text);
                }
                _ => {}
            }
        }
    }

    fn push_normalized_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        loop {
            if !self.in_edits_block {
                if let Some((ctl_start, ctl_end)) = find_next_control_tag(&self.buffer) {
                    let edit_first = find_any_edit_start(&self.buffer)
                        .is_some_and(|edit_idx| edit_idx < ctl_start);
                    if !edit_first {
                        let text_part = self.buffer[..ctl_start].to_string();
                        self.buffer.drain(..ctl_end);
                        if !text_part.is_empty() {
                            events.push(StreamEvent::TextDelta(text_part));
                        }
                        continue;
                    }
                }

                if let Some(tag_idx) = find_any_edit_start(&self.buffer) {
                    if tag_idx > 0 {
                        let text_part = self.buffer[..tag_idx].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..tag_idx);
                    }

                    if self.buffer.starts_with("<tauqe_edits>")
                        || self.buffer.starts_with("<tauqe_edits")
                    {
                        if let Some(close_tag_bracket) = self.buffer.find('>') {
                            self.buffer.drain(..=close_tag_bracket);
                            self.in_edits_block = true;
                            events.push(StreamEvent::EditStarted);
                            continue;
                        } else {
                            break;
                        }
                    } else {
                        self.in_edits_block = true;
                        events.push(StreamEvent::EditStarted);
                        continue;
                    }
                } else {
                    let emit_len = safe_text_emit_len(&self.buffer);
                    if emit_len > 0 {
                        let text_part = self.buffer[..emit_len].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..emit_len);
                    }
                    break;
                }
            }

            if self.current_file.is_none() {
                let next_file = find_file_tag_start(&self.buffer);
                if let Some(close_wb) = self.buffer.find("</tauqe_edits>") {
                    if next_file.as_ref().is_none_or(|(_, start)| close_wb < *start) {
                        self.buffer.drain(..close_wb + "</tauqe_edits>".len());
                        self.in_edits_block = false;
                        continue;
                    }
                }

                if let Some((tag_name, tag_start)) = next_file {
                    if let Some(close_bracket) = self.buffer[tag_start..].find('>') {
                        let header = &self.buffer[tag_start..tag_start + close_bracket + 1];
                        let is_self_closing = header.ends_with("/>");

                        if tag_name == "move" {
                            let from_attr = extract_attr(header, "from")
                                .or_else(|| extract_attr(header, "path"));
                            let to_attr = extract_attr(header, "to");
                            if let (Some(from), Some(to)) = (from_attr, to_attr) {
                                let resolved_from = resolve_path(&from, &self.editable_paths, false);
                                let resolved_to = resolve_path(&to, &self.editable_paths, true);
                                let display_path = format!("{} -> {}", resolved_from, resolved_to);
                                events.push(StreamEvent::EditFileStarted {
                                    path: display_path.clone(),
                                    op_type: "move".to_string(),
                                });
                                self.buffer.drain(..tag_start + close_bracket + 1);
                                if let Some(content) = self.staged_contents.remove(&resolved_from) {
                                    self.staged_contents.insert(resolved_to.clone(), content);
                                } else {
                                    let full_from = self.repo_root.join(&resolved_from);
                                    if let Ok(content) = std::fs::read_to_string(&full_from) {
                                        self.staged_contents.insert(resolved_to.clone(), content);
                                    }
                                }
                                events.push(StreamEvent::EditFileDone {
                                    path: display_path,
                                    status: "ok".to_string(),
                                    error: None,
                                    hunks_count: 0,
                                });
                                continue;
                            } else {
                                self.buffer.drain(..tag_start + close_bracket + 1);
                                continue;
                            }
                        }

                        if let Some(path_attr) = extract_attr(header, "path") {
                            let is_create = tag_name == "create";
                            let resolved = resolve_path(&path_attr, &self.editable_paths, is_create);
                            let op_type = match tag_name.as_str() {
                                "create" => "create".to_string(),
                                "overwrite" => "overwrite".to_string(),
                                "delete" => "delete".to_string(),
                                _ => "replace".to_string(),
                            };

                            events.push(StreamEvent::EditFileStarted {
                                path: resolved.clone(),
                                op_type: op_type.clone(),
                            });

                            self.buffer.drain(..tag_start + close_bracket + 1);
                            self.file_hunks_count = 0;

                            if is_self_closing || tag_name == "delete" {
                                events.push(StreamEvent::EditFileDone {
                                    path: resolved,
                                    status: "ok".to_string(),
                                    error: None,
                                    hunks_count: 0,
                                });
                                continue;
                            } else {
                                self.current_file = Some(CurrentStreamingFile {
                                    path: resolved,
                                    op_type,
                                });
                                continue;
                            }
                        } else {
                            self.buffer.drain(..tag_start + close_bracket + 1);
                            continue;
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            if let Some((curr_path, curr_op_type)) = self
                .current_file
                .as_ref()
                .map(|c| (c.path.clone(), c.op_type.clone()))
            {
                let close_tag = match curr_op_type.as_str() {
                    "create" => "</create>",
                    "overwrite" => "</overwrite>",
                    _ => "</edit>",
                };

                if curr_op_type == "create" || curr_op_type == "overwrite" {
                    if let Some(close_pos) = self.buffer.find(close_tag) {
                        let content = self.buffer[..close_pos].to_string();
                        self.buffer.drain(..close_pos + close_tag.len());

                        let norm_content = self.restore_content(&normalize_hunk(&content));

                        let old_text = if curr_op_type == "overwrite" {
                            match current_file_content(
                                &self.staged_contents,
                                &self.repo_root,
                                &curr_path,
                            ) {
                                Ok(text) => text,
                                Err(err_msg) => {
                                    events.push(StreamEvent::EditFileDone {
                                        path: curr_path.clone(),
                                        status: "error".to_string(),
                                        error: Some(err_msg),
                                        hunks_count: 0,
                                    });
                                    self.current_file = None;
                                    continue;
                                }
                            }
                        } else {
                            String::new()
                        };

                        events.push(StreamEvent::EditHunk {
                            path: curr_path.clone(),
                            hunk_index: 0,
                            old_text,
                            new_text: norm_content.clone(),
                        });

                        self.staged_contents.insert(curr_path.clone(), norm_content);

                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: "ok".to_string(),
                            error: None,
                            hunks_count: 1,
                        });

                        self.current_file = None;
                        continue;
                    } else {
                        break;
                    }
                } else {
                    if let Some((old_text, new_text, hunk_end)) = find_next_hunk(&self.buffer) {
                        let old_text = self.restore_content(&old_text);
                        let new_text = self.restore_content(&new_text);
                        let hunk_idx = self.file_hunks_count;
                        self.file_hunks_count += 1;

                        let val_res =
                            self.validate_hunk_in_memory(&curr_path, &old_text, &new_text);

                        events.push(StreamEvent::EditHunk {
                            path: curr_path.clone(),
                            hunk_index: hunk_idx,
                            old_text,
                            new_text,
                        });

                        self.buffer.drain(..hunk_end);

                        if let Err(err_msg) = val_res {
                            events.push(StreamEvent::EditFileDone {
                                path: curr_path,
                                status: "error".to_string(),
                                error: Some(err_msg),
                                hunks_count: self.file_hunks_count,
                            });
                            self.current_file = None;
                            continue;
                        }
                        continue;
                    }

                    if let Some(close_pos) = self.buffer.find(close_tag) {
                        self.buffer.drain(..close_pos + close_tag.len());
                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: "ok".to_string(),
                            error: None,
                            hunks_count: self.file_hunks_count,
                        });
                        self.current_file = None;
                        continue;
                    } else {
                        break;
                    }
                }
            }
        }

        events
    }

    fn validate_hunk_in_memory(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<(), String> {
        let current_content = if let Some(content) = self.staged_contents.get(path) {
            content.clone()
        } else {
            let full_path = self.repo_root.join(path);
            std::fs::read_to_string(&full_path)
                .map_err(|e| format!("Cannot read file '{}': {}", path, e))?
        };

        let matches: Vec<(usize, &str)> = current_content.match_indices(old_text).collect();
        if matches.is_empty() {
            return Err("Search block not found in file".to_string());
        }
        if matches.len() > 1 {
            return Err(format!(
                "Search block matches {} times (ambiguous)",
                matches.len()
            ));
        }

        let (idx, _) = matches[0];
        let mut updated = String::new();
        updated.push_str(&current_content[..idx]);
        updated.push_str(new_text);
        updated.push_str(&current_content[idx + old_text.len()..]);
        self.staged_contents.insert(path.to_string(), updated);

        Ok(())
    }
}

impl EditStreamFilter for XmlStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        let normalized = if let Some(marker) = &self.marker {
            self.raw_buffer.push_str(chunk);
            let prefix = crate::edits::protocol::xml::normalize_marked_xml(
                &self.raw_buffer, marker, false,
            );
            let delta = prefix[self.normalized_bytes..].to_string();
            self.normalized_bytes = prefix.len();
            delta
        } else {
            chunk.to_string()
        };
        let mut events = self.push_normalized_chunk(&normalized);
        self.restore_events(&mut events);
        events
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(marker) = &self.marker {
            let normalized = crate::edits::protocol::xml::normalize_marked_xml(
                &self.raw_buffer, marker, true,
            );
            let remaining = normalized[self.normalized_bytes..].to_string();
            events.extend(self.push_normalized_chunk(&remaining));
        }
        if !self.buffer.is_empty() {
            if let Some(curr) = self.current_file.take() {
                events.push(StreamEvent::EditFileDone {
                    path: curr.path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: self.file_hunks_count,
                });
            }
            if !self.buffer.trim().is_empty()
                && (!self.in_edits_block || !self.buffer.contains('<'))
            {
                events.push(StreamEvent::TextDelta(std::mem::take(&mut self.buffer)));
            }
        }
        self.restore_events(&mut events);
        events
    }
}

fn find_next_control_tag(buf: &str) -> Option<(usize, usize)> {
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
    candidates.into_iter().min_by_key(|(start, _)| *start)
}

fn safe_text_emit_len(buf: &str) -> usize {
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

fn find_any_edit_start(buf: &str) -> Option<usize> {
    let mut min_idx = None;
    for prefix in &["<tauqe_edits", "<edit", "<create", "<overwrite", "<delete", "<move"] {
        if let Some(idx) = buf.find(prefix) {
            min_idx = Some(min_idx.map_or(idx, |m: usize| m.min(idx)));
        }
    }
    min_idx
}

fn find_file_tag_start(buf: &str) -> Option<(String, usize)> {
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

fn find_next_hunk(buf: &str) -> Option<(String, String, usize)> {
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

fn extract_attr(header: &str, attr: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_doc_request_tags_hidden_from_stream_text() {
        let input = "Check docs.\n<doc_request topic=\"shortcuts\" />\nReady";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Check docs.\n\nReady");
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::EditStarted)));
    }

    #[test]
    fn test_context_request_tags_hidden_from_stream_text() {
        let input = "Need files.\n<context_request path=\"a.rs\" access=\"editable\" />\nDone";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Need files.\n\nDone");
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::EditStarted)));
    }

    #[test]
    fn test_user_language_and_verify_tags_hidden_from_stream_text() {
        let marker = "M456";
        let input = "Hello!\n<user_language_M456>Russian</user_language_M456>\n<verify_M456 target=\"all\" on_success=\"silent\" />\nHere is my explanation.";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some(marker.to_string()));
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello!\n\n\nHere is my explanation.");
        assert!(!text.contains("user_language"));
        assert!(!text.contains("verify"));
    }

    #[test]
    fn test_xml_marker_preserves_source_and_matches_final_parser() {
        let marker = "K7Q2ZX";
        let input = "Hi\n<tauqe_edits_K7Q2ZX summary=\"Update\">\n<edit_K7Q2ZX path=\"xml.rs\">\n<search_K7Q2ZX>\nlet s = \"</search>\";\n</search_K7Q2ZX>\n<replace_K7Q2ZX>\nlet s = \"<create>\";\n</replace_K7Q2ZX>\n</edit_K7Q2ZX>\n</tauqe_edits_K7Q2ZX>\nBye";
        let paths = vec!["xml.rs".to_string()];
        let proto = crate::edits::protocol::MarkedXmlEditProtocol::new(marker);
        let parsed = crate::edits::EditProtocol::parse_output(&proto, input, &paths);
        let (expected_old, expected_new) = match parsed {
            tauqe_protocol::ModelResult::Edit { edits, error: None, .. } => {
                match edits.into_iter().next().unwrap() {
                    tauqe_protocol::EditOperation::Replace { old_text, new_text, .. } => {
                        (old_text, new_text)
                    }
                    _ => panic!("Expected replacement"),
                }
            }
            _ => panic!("Expected valid edit"),
        };

        for split in input.char_indices().map(|(idx, _)| idx).chain(std::iter::once(input.len())) {
            let mut filter = XmlStreamFilter::new(paths.clone(), std::env::temp_dir())
                .with_marker(Some(marker.to_string()))
                .with_staged_contents(HashMap::from([
                    ("xml.rs".to_string(), expected_old.clone()),
                ]));
            let mut events = filter.push_chunk(&input[..split]);
            events.extend(filter.push_chunk(&input[split..]));
            events.extend(Box::new(filter).finish());
            let hunks: Vec<_> = events.iter().filter_map(|event| match event {
                StreamEvent::EditHunk { old_text, new_text, .. } => Some((old_text, new_text)),
                _ => None,
            }).collect();
            assert_eq!(hunks, vec![(&expected_old, &expected_new)], "split {split}");
            assert!(events.iter().any(|event| matches!(event,
                StreamEvent::EditFileDone { status, .. } if status == "ok"
            )));
            assert!(!events.iter().any(|event| matches!(event,
                StreamEvent::EditFileDone { status, .. } if status == "error"
            )));
            let text: String = events.iter().filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            }).collect();
            assert_eq!(text, "Hi\n\nBye");
        }
    }

    #[test]
    fn test_xml_marker_soft_matching() {
        let input = "<tauqe_edits-k7q2zy>\n<create_k7q2zx path=\"new.rs\">\nliteral <edit> and </search>\n</create_k7q2zx>\n</tauqe_edits-k7q2zy>";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some("K7Q2ZX".to_string()));
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        assert_eq!(events.iter().filter(|event| matches!(event,
            StreamEvent::EditFileStarted { .. }
        )).count(), 1);
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::EditHunk { new_text, .. }
                if new_text == "literal <edit> and </search>\n"
        )));
    }

    #[test]
    fn test_xml_without_marker_keeps_legacy_tags() {
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let mut events = filter.push_chunk(
            "<tauqe_edits><create path=\"new.rs\">hello</create></tauqe_edits>",
        );
        events.extend(Box::new(filter).finish());
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::EditHunk { new_text, .. } if new_text == "hello"
        )));
    }

    #[test]
    fn test_xml_marker_ignores_unsuffixed_edit_tags_in_conversation() {
        let input = "Literal <edit path=\"a.rs\"> and </edit>, then <";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some("K7Q2ZX".to_string()));
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events.iter().filter_map(|event| match event {
            StreamEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        }).collect();
        assert_eq!(text, input);
        assert!(!events.iter().any(|event| matches!(event, StreamEvent::EditStarted)));
    }
}
