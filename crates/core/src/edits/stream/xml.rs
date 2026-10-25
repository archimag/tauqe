use std::collections::HashMap;
use std::path::PathBuf;

use tauqe_protocol::{EditFileOp, EditFileStatus};

use super::{normalize_hunk, resolve_path, EditStreamFilter, StagingSimulator};
use crate::edits::protocol::xml::engine::OpenTag;
use crate::edits::protocol::xml::{is_valid_turn_marker, FileBlockTag};
use crate::providers::StreamEvent;

mod scanner;

use scanner::{
    activate_unmarked_hidden_tags, extract_attr, find_any_edit_start, find_container_close,
    find_file_tag_start, find_next_control_tag, find_next_hunk, safe_text_emit_len,
    starts_with_edit_container,
};

/// State-machine streaming parser for XML edits.
///
/// It intercepts XML edit blocks and emits semantic `StreamEvent::Edit*` events
/// while forwarding regular conversational text as `StreamEvent::TextDelta`.
pub struct XmlStreamFilter {
    buffer: String,
    marker: Option<String>,
    raw_buffer: String,
    marker_stack: Vec<OpenTag>,
    in_edits_block: bool,
    current_file: Option<CurrentStreamingFile>,
    file_hunks_count: usize,
    editable_paths: Vec<String>,
    staging: StagingSimulator,
}

struct CurrentStreamingFile {
    path: String,
    /// File block that opened the file, naming its closing tag and operation.
    block: &'static FileBlockTag,
}

impl XmlStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            buffer: String::new(),
            marker: None,
            raw_buffer: String::new(),
            marker_stack: Vec::new(),
            in_edits_block: false,
            current_file: None,
            file_hunks_count: 0,
            editable_paths,
            staging: StagingSimulator::new(HashMap::new(), repo_root),
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staging.set_staged_contents(staged_contents);
        self
    }

    /// Sets the active XML marker before any chunks are processed.
    ///
    /// A late call after streaming has begun is ignored and a malformed marker
    /// is dropped, both with a warning rather than a panic: the stream filter
    /// runs inside the long-lived daemon and must never abort the process.
    pub fn with_marker(mut self, marker: Option<String>) -> Self {
        if !self.raw_buffer.is_empty() || !self.buffer.is_empty() {
            tracing::warn!("ignoring with_marker call after streaming began");
            return self;
        }
        match marker {
            Some(value) if is_valid_turn_marker(&value) => self.marker = Some(value),
            Some(value) => {
                tracing::warn!(marker = %value, "dropping malformed turn marker");
                self.marker = None;
            }
            None => self.marker = None,
        }
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
                StreamEvent::EditHunk {
                    path,
                    old_text,
                    new_text,
                    ..
                } => {
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

                let safe_limit = safe_text_emit_len(&self.buffer);

                if let Some(tag_idx) = find_any_edit_start(&self.buffer) {
                    if safe_limit < self.buffer.len() && tag_idx >= safe_limit {
                        if safe_limit > 0 {
                            let text_part = self.buffer[..safe_limit].to_string();
                            events.push(StreamEvent::TextDelta(text_part));
                            self.buffer.drain(..safe_limit);
                        }
                        break;
                    }

                    if tag_idx > 0 {
                        let text_part = self.buffer[..tag_idx].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..tag_idx);
                    }

                    if starts_with_edit_container(&self.buffer) {
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
                    if safe_limit > 0 {
                        let text_part = self.buffer[..safe_limit].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..safe_limit);
                    }
                    break;
                }
            }

            if self.current_file.is_none() {
                let next_file = find_file_tag_start(&self.buffer);
                if let Some((close_start, close_len)) = find_container_close(&self.buffer) {
                    if next_file
                        .as_ref()
                        .is_none_or(|(_, start)| close_start < *start)
                    {
                        self.buffer.drain(..close_start + close_len);
                        self.in_edits_block = false;
                        continue;
                    }
                }

                if let Some((block, tag_start)) = next_file {
                    if let Some(close_bracket) = self.buffer[tag_start..].find('>') {
                        let header = &self.buffer[tag_start..tag_start + close_bracket + 1];
                        let is_self_closing = header.ends_with("/>");

                        if block.op == EditFileOp::Move {
                            let from_attr = extract_attr(header, "from")
                                .or_else(|| extract_attr(header, "path"));
                            let to_attr = extract_attr(header, "to");
                            if let (Some(from), Some(to)) = (from_attr, to_attr) {
                                let resolved_from =
                                    resolve_path(&from, &self.editable_paths, false);
                                let resolved_to = resolve_path(&to, &self.editable_paths, true);
                                let display_path = format!("{} -> {}", resolved_from, resolved_to);
                                events.push(StreamEvent::EditFileStarted {
                                    path: display_path.clone(),
                                    op_type: EditFileOp::Move,
                                });
                                self.buffer.drain(..tag_start + close_bracket + 1);
                                self.staging.apply_move(&resolved_from, &resolved_to);
                                events.push(StreamEvent::EditFileDone {
                                    path: display_path,
                                    status: EditFileStatus::Ok,
                                    error: None,
                                    hunks_count: 0,
                                });
                            } else {
                                self.buffer.drain(..tag_start + close_bracket + 1);
                            }
                            continue;
                        }

                        if let Some(path_attr) = extract_attr(header, "path") {
                            let is_create = block.op == EditFileOp::Create;
                            let resolved =
                                resolve_path(&path_attr, &self.editable_paths, is_create);

                            events.push(StreamEvent::EditFileStarted {
                                path: resolved.clone(),
                                op_type: block.op,
                            });

                            self.buffer.drain(..tag_start + close_bracket + 1);
                            self.file_hunks_count = 0;

                            if is_self_closing || block.op == EditFileOp::Delete {
                                events.push(StreamEvent::EditFileDone {
                                    path: resolved,
                                    status: EditFileStatus::Ok,
                                    error: None,
                                    hunks_count: 0,
                                });
                            } else {
                                self.current_file = Some(CurrentStreamingFile {
                                    path: resolved,
                                    block,
                                });
                            }
                            continue;
                        }

                        self.buffer.drain(..tag_start + close_bracket + 1);
                        continue;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            if let Some((curr_path, curr_block)) = self
                .current_file
                .as_ref()
                .map(|current| (current.path.clone(), current.block))
            {
                let close_tag = format!("</{}>", curr_block.name);

                if curr_block.op != EditFileOp::Replace {
                    if let Some(close_pos) = self.buffer.find(&close_tag) {
                        let content = self.buffer[..close_pos].to_string();
                        self.buffer.drain(..close_pos + close_tag.len());

                        let norm_content = self.restore_content(&normalize_hunk(&content));

                        let old_text = if curr_block.op == EditFileOp::Overwrite {
                            match self.staging.current_content(&curr_path) {
                                Ok(text) => text,
                                Err(err_msg) => {
                                    events.push(StreamEvent::EditFileDone {
                                        path: curr_path.clone(),
                                        status: EditFileStatus::Error,
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

                        self.staging.overwrite(&curr_path, norm_content);

                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: EditFileStatus::Ok,
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

                        let val_res = self.staging.apply_hunk(&curr_path, &old_text, &new_text);

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
                                status: EditFileStatus::Error,
                                error: Some(err_msg),
                                hunks_count: self.file_hunks_count,
                            });
                            self.current_file = None;
                            continue;
                        }
                        continue;
                    }

                    if let Some(close_pos) = self.buffer.find(&close_tag) {
                        self.buffer.drain(..close_pos + close_tag.len());
                        events.push(StreamEvent::EditFileDone {
                            path: curr_path,
                            status: EditFileStatus::Ok,
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
}

impl EditStreamFilter for XmlStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        let normalized = if let Some(marker) = &self.marker {
            self.raw_buffer.push_str(chunk);
            let delta = crate::edits::protocol::xml::normalize_marked_xml_incremental(
                &mut self.raw_buffer,
                marker,
                &mut self.marker_stack,
                false,
            );
            activate_unmarked_hidden_tags(&delta, marker)
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
            let delta = crate::edits::protocol::xml::normalize_marked_xml_incremental(
                &mut self.raw_buffer,
                marker,
                &mut self.marker_stack,
                true,
            );
            let remaining = activate_unmarked_hidden_tags(&delta, marker);
            events.extend(self.push_normalized_chunk(&remaining));
        }
        if !self.buffer.is_empty() {
            if let Some(curr) = self.current_file.take() {
                events.push(StreamEvent::EditFileDone {
                    path: curr.path,
                    status: EditFileStatus::Ok,
                    error: None,
                    hunks_count: self.file_hunks_count,
                });
            }
            if !self.buffer.trim().is_empty() {
                let cleaned =
                    crate::edits::protocol::xml::extract_conversational_text(&self.buffer);
                let cleaned = self.restore_content(&cleaned);
                if !cleaned.trim().is_empty() {
                    events.push(StreamEvent::TextDelta(cleaned));
                }
                self.buffer.clear();
            }
        }
        self.restore_events(&mut events);
        events
    }
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
    fn test_plan_tags_hidden_from_stream_text() {
        let marker = "M456";
        let input = "Planning.\n<plan_M456 id=\"test\" title=\"T\">\n<item id=\"1\" title=\"Step 1\" />\n</plan_M456>\nReady.";
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
        assert_eq!(text, "Planning.\n\nReady.");
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::EditStarted)));
    }

    #[test]
    fn test_plan_step_done_tags_hidden_from_stream_text() {
        let input = "Step done.\n<success summary=\"All checks pass\" />\nReady";
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
        assert_eq!(text, "Step done.\n\nReady");
        assert!(!text.contains("success"));
    }

    #[test]
    fn test_scratchpad_tags_hidden_from_stream_text() {
        let input = "Intro.\n<scratchpad>\nWorking hypothesis and notes.\n</scratchpad>\nOutro.";
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
        assert_eq!(text, "Intro.\n\nOutro.");
        assert!(!text.contains("Working hypothesis"));
        assert!(!text.contains("scratchpad"));
    }

    #[test]
    fn test_marked_scratchpad_hidden_from_stream_text() {
        let marker = "M999";
        let input = "Intro.\n<scratchpad_M999>\nWorking notes.\n</scratchpad_M999>\nOutro.";
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
        assert_eq!(text, "Intro.\n\nOutro.");
        assert!(!text.contains("Working notes"));
    }

    #[test]
    fn test_marked_stream_suppresses_unmarked_scratchpad() {
        let marker = "M123456";
        let input = "Intro.\n<scratchpad>\nUnmarked working note.\n</scratchpad>\nOutro.";
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
        assert_eq!(text, "Intro.\n\nOutro.");
        assert!(!text.contains("Unmarked working note"));
        assert!(!text.contains("scratchpad"));
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
    fn test_repo_search_tags_hidden_from_stream_text() {
        let input = "Searching symbols.\n<repo_search path=\"src\" regex=\"fn execute\" />\nReady";
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
        assert_eq!(text, "Searching symbols.\n\nReady");
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
        let parsed = proto.parse_output(input, &paths);
        let (expected_old, expected_new) = match parsed {
            tauqe_protocol::ModelResult::Edit { edits, outcome, .. } if outcome.error.is_none() => {
                match edits.into_iter().next().unwrap() {
                    tauqe_protocol::EditOperation::Replace {
                        old_text, new_text, ..
                    } => (old_text, new_text),
                    _ => panic!("Expected replacement"),
                }
            }
            _ => panic!("Expected valid edit"),
        };

        for split in input
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(input.len()))
        {
            let mut filter = XmlStreamFilter::new(paths.clone(), std::env::temp_dir())
                .with_marker(Some(marker.to_string()))
                .with_staged_contents(HashMap::from([(
                    "xml.rs".to_string(),
                    expected_old.clone(),
                )]));
            let mut events = filter.push_chunk(&input[..split]);
            events.extend(filter.push_chunk(&input[split..]));
            events.extend(Box::new(filter).finish());
            let hunks: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::EditHunk {
                        old_text, new_text, ..
                    } => Some((old_text, new_text)),
                    _ => None,
                })
                .collect();
            assert_eq!(hunks, vec![(&expected_old, &expected_new)], "split {split}");
            assert!(events.iter().any(|event| matches!(event,
                StreamEvent::EditFileDone { status, .. } if *status == EditFileStatus::Ok
            )));
            assert!(!events.iter().any(|event| matches!(event,
                StreamEvent::EditFileDone { status, .. } if *status == EditFileStatus::Error
            )));
            let text: String = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Hi\n\nBye");
        }
    }

    #[test]
    fn test_xml_marker_accepts_case_insensitive_exact_match() {
        let input = "<tauqe_edits-k7q2zx>\n<create_K7Q2ZX path=\"new.rs\">\nliteral <edit> and </search>\n</create_K7Q2ZX>\n</tauqe_edits-k7q2zx>";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some("K7Q2ZX".to_string()));
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditFileStarted { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::EditHunk { new_text, .. }
                if new_text == "literal <edit> and </search>\n"
        )));
    }

    #[test]
    fn test_xml_marker_rejects_near_miss_suffix() {
        // A suffix differing from the marker by even one character must be
        // treated as literal content: tolerating near-misses would let
        // resembling source text forge marker-bound protocol tags.
        let input = "<tauqe_edits-k7q2zy>\n<create_k7q2zy path=\"new.rs\">\nliteral\n</create_k7q2zy>\n</tauqe_edits-k7q2zy>";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some("K7Q2ZX".to_string()));
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::EditFileStarted { .. })));
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("tauqe_edits-k7q2zy"));
    }

    #[test]
    fn test_xml_without_marker_keeps_legacy_tags() {
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let mut events =
            filter.push_chunk("<tauqe_edits><create path=\"new.rs\">hello</create></tauqe_edits>");
        events.extend(Box::new(filter).finish());
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::EditHunk { new_text, .. } if new_text == "hello"
        )));
    }

    #[test]
    fn test_stream_plan_with_embedded_edit_mentions_does_not_trigger_edit_started() {
        let input = "Here is the plan:\n<plan id=\"p1\" title=\"P\">\n<item id=\"1\" title=\"Add <edit> helper\" />\n</plan>\nHope this helps!";
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(filter.push_chunk(&ch.to_string()));
        }
        events.extend(Box::new(filter).finish());
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::EditStarted)));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Here is the plan:\n\nHope this helps!");
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
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, input);
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::EditStarted)));
    }

    #[test]
    fn test_with_marker_drops_malformed_marker_without_panicking() {
        // A malformed marker must not panic; it is dropped so legacy tags apply.
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir())
            .with_marker(Some("not a valid marker".to_string()));
        let mut events =
            filter.push_chunk("<tauqe_edits><create path=\"new.rs\">hello</create></tauqe_edits>");
        events.extend(Box::new(filter).finish());
        assert!(events.iter().any(|event| matches!(event,
            StreamEvent::EditHunk { new_text, .. } if new_text == "hello"
        )));
    }

    #[test]
    fn test_with_marker_after_streaming_is_ignored_without_panicking() {
        // A marker arriving after streaming began is ignored, not fatal.
        let mut filter = XmlStreamFilter::new(vec![], std::env::temp_dir());
        let _ = filter.push_chunk("<ed");
        filter = filter.with_marker(Some("LATE123".to_string()));
        let events = Box::new(filter).finish();
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "<ed");
    }
}
