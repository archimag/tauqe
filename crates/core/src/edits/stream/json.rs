use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use struson::reader::{JsonReader, JsonStreamReader};
use tauqe_protocol::{EditFileOp, EditFileStatus, StructuredChangeProposal};

use super::{normalize_hunk, resolve_path, EditStreamFilter, StagingSimulator};
use crate::providers::StreamEvent;

pub mod decode;
pub mod field;

pub use decode::{extract_streamed_string_field, find_string_field_value_start};
pub use field::JsonFieldStreamFilter;
use decode::{
    decode_json_string_body_incremental, extract_incomplete_change_op_and_path,
    find_json_string_end_from,
};

/// State-machine streaming parser for Structured Output JSON edits using struson.
///
/// It streams the `message` field on the fly as `StreamEvent::TextDelta` while
/// deserializing and memory-validating completed `changes[i]` elements, emitting
/// `EditFileStarted`, `EditHunk`, and `EditFileDone`. Arbitrary field order is supported.
pub struct JsonStreamFilter {
    /// None = undecided; Some(true) = provider returned plain text (not JSON), stream as-is.
    passthrough: Option<bool>,
    buffer: String,
    message_value_start: Option<usize>,
    message_raw_decoded_pos: usize,
    message_end_scanned_pos: usize,
    message_finished: bool,
    processed_changes_count: usize,
    changes_finished: bool,
    has_emitted_edit_started: bool,
    started_change_index: Option<usize>,
    editable_paths: Vec<String>,
    staging: StagingSimulator,
}

impl JsonStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            passthrough: None,
            buffer: String::new(),
            message_value_start: None,
            message_raw_decoded_pos: 0,
            message_end_scanned_pos: 0,
            message_finished: false,
            processed_changes_count: 0,
            changes_finished: false,
            has_emitted_edit_started: false,
            started_change_index: None,
            editable_paths,
            staging: StagingSimulator::new(HashMap::new(), repo_root),
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staging.set_staged_contents(staged_contents);
        self
    }

    fn handle_completed_change(
        &mut self,
        change: &StructuredChangeProposal,
        already_started: bool,
        events: &mut Vec<StreamEvent>,
    ) {
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }

        let op = change.op;
        let is_create = op == EditFileOp::Create;
        let resolved_path = resolve_path(&change.path, &self.editable_paths, is_create);

        if !already_started {
            events.push(StreamEvent::EditFileStarted {
                path: resolved_path.clone(),
                op_type: op,
            });
        }

        match op {
            EditFileOp::Create => {
                let content = normalize_hunk(change.content.as_deref().unwrap_or_default());
                events.push(StreamEvent::EditHunk {
                    path: resolved_path.clone(),
                    hunk_index: 0,
                    old_text: String::new(),
                    new_text: content.clone(),
                });
                self.staging.overwrite(&resolved_path, content);
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: EditFileStatus::Ok,
                    error: None,
                    hunks_count: 1,
                });
            }
            EditFileOp::Overwrite => {
                let new_content = normalize_hunk(change.content.as_deref().unwrap_or_default());
                match self.staging.current_content(&resolved_path) {
                    Ok(old_text) => {
                        events.push(StreamEvent::EditHunk {
                            path: resolved_path.clone(),
                            hunk_index: 0,
                            old_text,
                            new_text: new_content.clone(),
                        });
                        self.staging.overwrite(&resolved_path, new_content);
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: EditFileStatus::Ok,
                            error: None,
                            hunks_count: 1,
                        });
                    }
                    Err(err_msg) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: EditFileStatus::Error,
                            error: Some(err_msg),
                            hunks_count: 0,
                        });
                    }
                }
            }
            EditFileOp::Delete => {
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: EditFileStatus::Ok,
                    error: None,
                    hunks_count: 0,
                });
            }
            EditFileOp::Move => {
                let to_raw = change.to.as_deref().unwrap_or_default();
                let resolved_to = resolve_path(to_raw, &self.editable_paths, true);
                let display_path = format!("{} -> {}", resolved_path, resolved_to);
                self.staging.apply_move(&resolved_path, &resolved_to);
                events.push(StreamEvent::EditFileDone {
                    path: display_path,
                    status: EditFileStatus::Ok,
                    error: None,
                    hunks_count: 0,
                });
            }
            EditFileOp::Replace => {
                let old_text = normalize_hunk(change.old_text.as_deref().unwrap_or_default());
                let new_text = normalize_hunk(
                    change
                        .new_text
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .or(change.content.as_deref())
                        .unwrap_or_default(),
                );

                events.push(StreamEvent::EditHunk {
                    path: resolved_path.clone(),
                    hunk_index: 0,
                    old_text: old_text.clone(),
                    new_text: new_text.clone(),
                });

                let val_res = self.staging.apply_hunk(&resolved_path, &old_text, &new_text);
                match val_res {
                    Ok(()) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: EditFileStatus::Ok,
                            error: None,
                            hunks_count: 1,
                        });
                    }
                    Err(err_msg) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: EditFileStatus::Error,
                            error: Some(err_msg),
                            hunks_count: 1,
                        });
                    }
                }
            }
        }
    }
}

impl EditStreamFilter for JsonStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        // 0. Decide whether the model actually produced JSON; if the provider ignored
        // the schema and returned plain text, stream it to the user unchanged.
        if self.passthrough.is_none() {
            match self.buffer.chars().find(|c| !c.is_whitespace()) {
                None => return events,
                Some(c) => {
                    let plain = c != '{' && c != '`';
                    self.passthrough = Some(plain);
                    if plain {
                        events.push(StreamEvent::TextDelta(self.buffer.clone()));
                        return events;
                    }
                }
            }
        } else if self.passthrough == Some(true) {
            if !chunk.is_empty() {
                events.push(StreamEvent::TextDelta(chunk.to_string()));
            }
            return events;
        }

        // 1. Stream message field deltas on the fly using incremental decoding
        if !self.message_finished {
            if self.message_value_start.is_none() {
                self.message_value_start = find_string_field_value_start(&self.buffer, "message");
            }
            if let Some(val_start) = self.message_value_start {
                let content_slice = &self.buffer[val_start..];
                let (raw_body, is_finished) =
                    match find_json_string_end_from(content_slice, &mut self.message_end_scanned_pos) {
                        Some(end) => (&content_slice[..end], true),
                        None => (content_slice, false),
                    };

                let delta = decode_json_string_body_incremental(
                    raw_body,
                    &mut self.message_raw_decoded_pos,
                );
                if is_finished && self.message_raw_decoded_pos >= raw_body.len() {
                    self.message_finished = true;
                }
                if !delta.is_empty() {
                    events.push(StreamEvent::TextDelta(delta));
                }
            }
        }

        // 2. Parse top-level JSON and extract completed changes[i] via struson
        let mut new_changes = Vec::new();
        let should_scan_changes = !self.changes_finished
            && (chunk.contains('}') || chunk.contains(']') || self.processed_changes_count == 0);

        if should_scan_changes {
            if let Some(brace_idx) = self.buffer.find('{') {
                let json_bytes = &self.buffer.as_bytes()[brace_idx..];
                let mut reader = JsonStreamReader::new(json_bytes);
                if reader.begin_object().is_ok() {
                    'outer: while let Ok(true) = reader.has_next() {
                        let name = match reader.next_name() {
                            Ok(n) => n.to_string(),
                            Err(_) => break 'outer,
                        };

                        if name == "changes" {
                            if reader.begin_array().is_ok() {
                                let mut current_idx = 0;
                                while let Ok(true) = reader.has_next() {
                                    if current_idx < self.processed_changes_count {
                                        if reader.skip_value().is_err() {
                                            break 'outer;
                                        }
                                        current_idx += 1;
                                    } else {
                                        let mut de =
                                            struson::serde::JsonReaderDeserializer::new(&mut reader);
                                        match StructuredChangeProposal::deserialize(&mut de) {
                                            Ok(change) => {
                                                self.processed_changes_count += 1;
                                                current_idx += 1;
                                                new_changes.push(change);
                                            }
                                            Err(_) => {
                                                // Incomplete element in stream; wait for more data
                                                break 'outer;
                                            }
                                        }
                                    }
                                }
                                if let Ok(false) = reader.has_next() {
                                    let _ = reader.end_array();
                                    self.changes_finished = true;
                                }
                            } else {
                                break 'outer;
                            }
                        } else if reader.skip_value().is_err() {
                            // Value for this property is not fully received yet
                            break 'outer;
                        }
                    }
                }
            }
        }

        let start_processed = self
            .processed_changes_count
            .saturating_sub(new_changes.len());
        for (i, change) in new_changes.into_iter().enumerate() {
            let change_idx = start_processed + i;
            let already_started = self.started_change_index == Some(change_idx);
            self.handle_completed_change(&change, already_started, &mut events);
            if already_started {
                self.started_change_index = None;
            }
        }

        // 3. Early detection of active file in incomplete change
        if self.started_change_index != Some(self.processed_changes_count) && !self.changes_finished {
            if let Some((op, path)) =
                extract_incomplete_change_op_and_path(&self.buffer, self.processed_changes_count)
            {
                if !self.has_emitted_edit_started {
                    events.push(StreamEvent::EditStarted);
                    self.has_emitted_edit_started = true;
                }
                let is_create = op == EditFileOp::Create;
                let resolved_path = resolve_path(&path, &self.editable_paths, is_create);
                events.push(StreamEvent::EditFileStarted {
                    path: resolved_path,
                    op_type: op,
                });
                self.started_change_index = Some(self.processed_changes_count);
            }
        }

        events
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.passthrough == Some(true) {
            return events;
        }
        if !self.message_finished {
            if let Some(val_start) = self.message_value_start {
                let content_slice = &self.buffer[val_start..];
                let delta = decode_json_string_body_incremental(
                    content_slice,
                    &mut self.message_raw_decoded_pos,
                );
                if !delta.is_empty() {
                    events.push(StreamEvent::TextDelta(delta));
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_early_detection_active_file_streaming() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec!["crates/foo.rs".to_string()], root);
        let chunk1 = r#"{"message":"Hello","changes":[{"op":"replace","path":"crates/foo.rs""#;
        let events1 = filter.push_chunk(chunk1);
        assert!(events1
            .iter()
            .any(|e| matches!(e, StreamEvent::EditStarted)));
        assert!(events1.iter().any(|e| matches!(e, StreamEvent::EditFileStarted { path, op_type } if path == "crates/foo.rs" && *op_type == EditFileOp::Replace)));
        assert!(!events1
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileDone { .. })));

        let chunk2 = r#", "old_text":"", "new_text":"", "content":""}]}"#;
        let events2 = filter.push_chunk(chunk2);
        assert!(!events2
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileStarted { .. })));
        assert!(events2
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileDone { .. })));
    }

    #[test]
    fn test_json_stream_filter_partial_chunks_no_panic() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec!["test.rs".to_string()], root);

        let full_json = r#"{"message":"Rust is great","changes":[],"context_requests":[],"suggested_actions":[]}"#;
        let mut events = Vec::new();
        for c in full_json.chars() {
            let s = c.to_string();
            events.extend(filter.push_chunk(&s));
        }
        events.extend(Box::new(filter).finish());
        let mut text = String::new();
        for ev in events {
            if let StreamEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
        assert_eq!(text, "Rust is great");
    }

    #[test]
    fn test_message_after_changes_streams_both() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec![], root);
        let json = r#"{"changes":[{"op":"create","path":"a.rs","old_text":"","new_text":"","content":"let m = \"message\";\n"}],"message":"Done here","context_requests":[],"suggested_actions":[]}"#;
        let mut events = Vec::new();
        for c in json.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|e| {
                if let StreamEvent::TextDelta(d) = e {
                    Some(d.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "Done here");
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileStarted { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileDone { .. })));
    }

    #[test]
    fn test_json_stream_filter_markdown_wrapper_no_panic() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec!["test.rs".to_string()], root);

        let wrapped = "```json\n{\"message\": \"Hello!\", \"changes\": []}\n```";
        let events = filter.push_chunk(wrapped);
        assert!(!events.is_empty());
        let finish_events = Box::new(filter).finish();
        assert!(finish_events.is_empty() || matches!(&finish_events[0], StreamEvent::TextDelta(_)));
    }

    #[test]
    fn test_no_edit_started_for_empty_changes_streams_full_message() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec!["a.rs".to_string()], root);
        let json = r#"{"message":"Nothing to change here.","changes":[]}"#;
        let mut events = Vec::new();
        for c in json.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Nothing to change here.");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::EditStarted)));
        assert!(!events
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileStarted { .. })));
    }

    #[test]
    fn test_early_detection_requires_known_edit_op() {
        let root = std::env::temp_dir();

        // An unrecognized operation in an incomplete element must not start
        // early edit reporting, avoiding a false positive.
        let mut unknown = JsonStreamFilter::new(vec!["a.rs".to_string()], root.clone());
        let chunk = r#"{"changes":[{"op":"frobnicate","path":"a.rs""#;
        let events = unknown.push_chunk(chunk);
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::EditStarted)));
        assert!(!events
            .iter()
            .any(|e| matches!(e, StreamEvent::EditFileStarted { .. })));

        // A recognized operation with a non-empty path still starts early
        // edit reporting.
        let mut known = JsonStreamFilter::new(vec!["a.rs".to_string()], root);
        let chunk = r#"{"changes":[{"op":"create","path":"a.rs""#;
        let events = known.push_chunk(chunk);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::EditStarted)));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::EditFileStarted { path, op_type }
                if path == "a.rs" && *op_type == EditFileOp::Create
        )));
    }

    #[test]
    fn test_json_edit_response_streams_across_all_split_points() {
        let root = std::env::temp_dir();
        let input = r#"{"message":"Hi","changes":[{"op":"replace","path":"a.rs","old_text":"x","new_text":"y"}]}"#;
        let boundaries: Vec<usize> = input
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(input.len()))
            .collect();
        for &split in &boundaries {
            let staged = HashMap::from([("a.rs".to_string(), "x".to_string())]);
            let mut filter = JsonStreamFilter::new(vec!["a.rs".to_string()], root.clone())
                .with_staged_contents(staged);
            let mut events = filter.push_chunk(&input[..split]);
            events.extend(filter.push_chunk(&input[split..]));
            events.extend(Box::new(filter).finish());

            let started = events
                .iter()
                .filter(|e| matches!(e, StreamEvent::EditStarted))
                .count();
            assert_eq!(started, 1, "split {split}");

            let text: String = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::TextDelta(d) => Some(d.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Hi", "split {split}");

            assert!(
                events.iter().any(|e| matches!(
                    e,
                    StreamEvent::EditFileDone { status, .. } if *status == EditFileStatus::Ok
                )),
                "split {split}"
            );
        }
    }

    #[test]
    fn test_json_stream_filter_message_surrogate_pair_emoji() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec![], root);
        let json = r#"{"message":"Hi \uD83D\uDE00!","changes":[]}"#;
        let mut events = Vec::new();
        for c in json.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hi \u{1F600}!");
    }
}
