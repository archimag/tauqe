use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use struson::reader::{JsonReader, JsonStreamReader};
use tauqe_protocol::{EditFileOp, EditFileStatus, StructuredChangeProposal};

use super::{normalize_hunk, resolve_path, EditStreamFilter, StagingSimulator};
use crate::providers::StreamEvent;

pub mod decode;
pub mod field;

use decode::{extract_incomplete_change_op_and_path, StringFieldCursor};
pub use decode::{extract_streamed_string_field, find_string_field_value_start};
pub use field::JsonFieldStreamFilter;

/// State-machine streaming parser for Structured Output JSON edits using struson.
///
/// It streams the `message` field on the fly as `StreamEvent::TextDelta` while
/// deserializing and memory-validating completed `changes[i]` elements, emitting
/// `EditFileStarted`, `EditHunk`, and `EditFileDone`. Arbitrary field order is supported.
///
/// The filter synthesizes the same event chain the XML stream filter produces:
/// one `EditStarted`, one `EditFileStarted` per file, sequential `EditHunk`
/// entries, and a single `EditFileDone` per file carrying the total hunks. The
/// JSON-specific detail that the `changes` array only becomes readable once its
/// elements close is fully encapsulated here, so the user-facing stream is
/// indistinguishable from the XML experience.
pub struct JsonStreamFilter {
    /// None = undecided; Some(true) = provider returned plain text (not JSON), stream as-is.
    passthrough: Option<bool>,
    buffer: String,
    message_cursor: StringFieldCursor,
    message_finished: bool,
    processed_changes_count: usize,
    changes_finished: bool,
    has_emitted_edit_started: bool,
    editable_paths: Vec<String>,
    staging: StagingSimulator,
    /// Number of hunks already emitted for each file path.
    file_hunk_counts: HashMap<String, usize>,
    /// Path of the file whose `EditFileDone` is still pending, if any.
    open_file_path: Option<String>,
    /// Path reported by the pending file's `EditFileDone` (differs for moves).
    open_file_display: String,
}

impl JsonStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            passthrough: None,
            buffer: String::new(),
            message_cursor: StringFieldCursor::new(),
            message_finished: false,
            processed_changes_count: 0,
            changes_finished: false,
            has_emitted_edit_started: false,
            editable_paths,
            staging: StagingSimulator::new(HashMap::new(), repo_root),
            file_hunk_counts: HashMap::new(),
            open_file_path: None,
            open_file_display: String::new(),
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staging.set_staged_contents(staged_contents);
        self
    }

    /// Emits the pending `EditFileDone` for the currently open file, if any.
    ///
    /// Buffering the completion until the file changes or the array closes is
    /// what lets several `replace` operations against one file collapse into a
    /// single terminal event with the exact hunks count, matching the XML filter.
    fn flush_open_file(&mut self, events: &mut Vec<StreamEvent>) {
        if let Some(open_path) = self.open_file_path.take() {
            let hunks_count = self.file_hunk_counts.remove(&open_path).unwrap_or(0);
            let display = std::mem::take(&mut self.open_file_display);
            let final_path = if display.is_empty() {
                open_path
            } else {
                display
            };
            events.push(StreamEvent::EditFileDone {
                path: final_path,
                status: EditFileStatus::Ok,
                error: None,
                hunks_count,
            });
        }
    }

    /// Reads the top-level `changes` array and deserializes any newly completed
    /// element, advancing `processed_changes_count` and flipping
    /// `changes_finished` once the array closes.
    fn scan_new_changes(&mut self) -> Vec<StructuredChangeProposal> {
        let mut new_changes = Vec::new();
        if self.changes_finished {
            return new_changes;
        }
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
        new_changes
    }

    /// Applies one completed `changes[i]` element and emits the corresponding
    /// hunk events, folding completion into the pending file.
    fn process_completed_change(
        &mut self,
        change: &StructuredChangeProposal,
        events: &mut Vec<StreamEvent>,
    ) {
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }

        let op = change.op;
        let is_create = op == EditFileOp::Create;
        let resolved_path = resolve_path(&change.path, &self.editable_paths, is_create);

        if self.open_file_path.as_deref() != Some(resolved_path.as_str()) {
            self.flush_open_file(events);
            events.push(StreamEvent::EditFileStarted {
                path: resolved_path.clone(),
                op_type: op,
            });
            self.open_file_path = Some(resolved_path.clone());
            self.open_file_display = resolved_path.clone();
        }

        let hunk_index = self
            .file_hunk_counts
            .get(&resolved_path)
            .copied()
            .unwrap_or(0);

        match op {
            EditFileOp::Create => {
                let content = normalize_hunk(change.content.as_deref().unwrap_or_default());
                events.push(StreamEvent::EditHunk {
                    path: resolved_path.clone(),
                    hunk_index,
                    old_text: String::new(),
                    new_text: content.clone(),
                });
                self.staging.overwrite(&resolved_path, content);
                self.file_hunk_counts.insert(resolved_path, hunk_index + 1);
            }
            EditFileOp::Overwrite => {
                let new_content = normalize_hunk(change.content.as_deref().unwrap_or_default());
                match self.staging.current_content(&resolved_path) {
                    Ok(old_text) => {
                        events.push(StreamEvent::EditHunk {
                            path: resolved_path.clone(),
                            hunk_index,
                            old_text,
                            new_text: new_content.clone(),
                        });
                        self.staging.overwrite(&resolved_path, new_content);
                        self.file_hunk_counts.insert(resolved_path, hunk_index + 1);
                    }
                    Err(err_msg) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path.clone(),
                            status: EditFileStatus::Error,
                            error: Some(err_msg),
                            hunks_count: hunk_index,
                        });
                        self.file_hunk_counts.remove(&resolved_path);
                        self.open_file_path = None;
                    }
                }
            }
            EditFileOp::Delete => {
                // No hunk content; the pending EditFileDone reports zero hunks.
                self.file_hunk_counts.entry(resolved_path).or_insert(0);
            }
            EditFileOp::Move => {
                let to_raw = change.to.as_deref().unwrap_or_default();
                let resolved_to = resolve_path(to_raw, &self.editable_paths, true);
                self.staging.apply_move(&resolved_path, &resolved_to);
                self.open_file_display = format!("{} -> {}", resolved_path, resolved_to);
                self.file_hunk_counts.entry(resolved_path).or_insert(0);
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
                    hunk_index,
                    old_text: old_text.clone(),
                    new_text: new_text.clone(),
                });

                match self
                    .staging
                    .apply_hunk(&resolved_path, &old_text, &new_text)
                {
                    Ok(()) => {
                        self.file_hunk_counts.insert(resolved_path, hunk_index + 1);
                    }
                    Err(err_msg) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path.clone(),
                            status: EditFileStatus::Error,
                            error: Some(err_msg),
                            hunks_count: hunk_index + 1,
                        });
                        self.file_hunk_counts.remove(&resolved_path);
                        self.open_file_path = None;
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

        // 0b. The payload began with a backtick or fence but never produced a JSON
        // object. Switch to passthrough so plain text is streamed, not swallowed.
        if self.passthrough == Some(false) && self.buffer.find('{').is_none() {
            if self.buffer.len() > 64 {
                self.passthrough = Some(true);
                events.push(StreamEvent::TextDelta(self.buffer.clone()));
                return events;
            }
            return events;
        }

        // 1. Stream message field deltas on the fly using incremental decoding.
        if !self.message_finished {
            let (delta, finished) =
                extract_streamed_string_field(&self.buffer, "message", &mut self.message_cursor);
            if let Some(d) = delta {
                events.push(StreamEvent::TextDelta(d));
            }
            if finished {
                self.message_finished = true;
            }
        }

        // 2. Parse top-level JSON and extract completed changes[i] via struson.
        let should_scan = !self.changes_finished
            && (chunk.contains('}') || chunk.contains(']') || self.processed_changes_count == 0);
        if should_scan {
            let new_changes = self.scan_new_changes();
            for change in new_changes {
                self.process_completed_change(&change, &mut events);
            }
            if self.changes_finished {
                self.flush_open_file(&mut events);
            }
        }

        // 3. Early detection of the file referenced by the still-incomplete element.
        if !self.changes_finished {
            if let Some((op, path)) =
                extract_incomplete_change_op_and_path(&self.buffer, self.processed_changes_count)
            {
                let is_create = op == EditFileOp::Create;
                let resolved_path = resolve_path(&path, &self.editable_paths, is_create);
                if self.open_file_path.as_deref() != Some(resolved_path.as_str()) {
                    self.flush_open_file(&mut events);
                    if !self.has_emitted_edit_started {
                        events.push(StreamEvent::EditStarted);
                        self.has_emitted_edit_started = true;
                    }
                    events.push(StreamEvent::EditFileStarted {
                        path: resolved_path.clone(),
                        op_type: op,
                    });
                    self.open_file_path = Some(resolved_path.clone());
                    self.open_file_display = resolved_path;
                }
            }
        }

        events
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.passthrough == Some(true) {
            return events;
        }
        // The payload never resolved to a JSON object: it is plain text that
        // merely began with a backtick or fence. Flush it verbatim instead of
        // silently dropping the whole response.
        if self.buffer.find('{').is_none() {
            if !self.buffer.is_empty() {
                events.push(StreamEvent::TextDelta(self.buffer.clone()));
            }
            return events;
        }
        // Drain any residual message text. Decoding stops at the closing quote,
        // so no structural JSON leaks into the TextDelta stream.
        if !self.message_finished {
            let (delta, _) =
                extract_streamed_string_field(&self.buffer, "message", &mut self.message_cursor);
            if let Some(d) = delta {
                events.push(StreamEvent::TextDelta(d));
            }
        }
        // Synthesize any edit events left by a truncated or single-shot payload.
        let new_changes = self.scan_new_changes();
        for change in new_changes {
            self.process_completed_change(&change, &mut events);
        }
        self.flush_open_file(&mut events);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarize(events: &[StreamEvent]) -> (String, Vec<String>) {
        let mut text = String::new();
        let mut edits = Vec::new();
        for event in events {
            match event {
                StreamEvent::TextDelta(d) => text.push_str(d),
                StreamEvent::EditStarted => edits.push("started".to_string()),
                StreamEvent::EditFileStarted { path, op_type } => {
                    edits.push(format!("file_started:{}:{}", path, op_type));
                }
                StreamEvent::EditHunk {
                    path,
                    hunk_index,
                    old_text,
                    new_text,
                } => edits.push(format!(
                    "hunk:{}:{}:{:?}:{:?}",
                    path, hunk_index, old_text, new_text
                )),
                StreamEvent::EditFileDone {
                    path,
                    status,
                    hunks_count,
                    ..
                } => edits.push(format!("file_done:{}:{}:{}", path, status, hunks_count)),
                _ => {}
            }
        }
        (text, edits)
    }

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

    #[test]
    fn test_backtick_plain_text_falls_back_to_passthrough() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec![], root);
        let text = "`foo` is a plain text reply that must not be swallowed by the JSON filter.";

        let mut streamed = String::new();
        for ev in filter.push_chunk(text) {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        for ev in Box::new(filter).finish() {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        assert_eq!(streamed, text);
    }

    #[test]
    fn test_short_backtick_plain_text_flushed_on_finish() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec![], root);
        let text = "`hi`";

        let mut streamed = String::new();
        for ev in filter.push_chunk(text) {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        for ev in Box::new(filter).finish() {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        assert_eq!(streamed, text);
    }

    #[test]
    fn test_backtick_plain_text_streams_across_chunks() {
        let root = std::env::temp_dir();
        let mut filter = JsonStreamFilter::new(vec![], root);
        let text = "`cargo build` produced output that is a plain text answer, not JSON.";

        let mut streamed = String::new();
        for c in text.chars() {
            for ev in filter.push_chunk(&c.to_string()) {
                if let StreamEvent::TextDelta(d) = ev {
                    streamed.push_str(&d);
                }
            }
        }
        for ev in Box::new(filter).finish() {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        assert_eq!(streamed, text);
    }

    #[test]
    fn test_multiple_replaces_same_file_accumulate_hunks() {
        let root = std::env::temp_dir();
        let input = r#"{"message":"Go","changes":[{"op":"replace","path":"a.rs","old_text":"x","new_text":"y"},{"op":"replace","path":"a.rs","old_text":"y","new_text":"z"}]}"#;
        let staged = HashMap::from([("a.rs".to_string(), "x".to_string())]);
        let mut filter =
            JsonStreamFilter::new(vec!["a.rs".to_string()], root).with_staged_contents(staged);

        let mut events = Vec::new();
        for c in input.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());

        // Exactly one EditStarted and one EditFileStarted for the shared file.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::EditStarted))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::EditFileStarted { .. }))
                .count(),
            1
        );

        // Sequential hunk indices within the single file.
        let hunk_indices: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::EditHunk { hunk_index, .. } => Some(*hunk_index),
                _ => None,
            })
            .collect();
        assert_eq!(hunk_indices, vec![0, 1]);

        // A single terminal event carrying the total hunks count.
        let done: Vec<(EditFileStatus, usize)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::EditFileDone {
                    status,
                    hunks_count,
                    ..
                } => Some((*status, *hunks_count)),
                _ => None,
            })
            .collect();
        assert_eq!(done, vec![(EditFileStatus::Ok, 2)]);
    }

    #[test]
    fn test_stream_events_identical_char_by_char_and_whole() {
        let root = std::env::temp_dir();
        let input = r#"{"message":"Hello","changes":[{"op":"replace","path":"a.rs","old_text":"x","new_text":"y"},{"op":"replace","path":"a.rs","old_text":"y","new_text":"z"}]}"#;
        let staged = HashMap::from([("a.rs".to_string(), "x".to_string())]);

        let run = |whole: bool| {
            let mut filter = JsonStreamFilter::new(vec!["a.rs".to_string()], root.clone())
                .with_staged_contents(staged.clone());
            let mut events = Vec::new();
            if whole {
                events.extend(filter.push_chunk(input));
            } else {
                for c in input.chars() {
                    events.extend(filter.push_chunk(&c.to_string()));
                }
            }
            events.extend(Box::new(filter).finish());
            summarize(&events)
        };

        let char_by_char = run(false);
        let whole = run(true);

        assert_eq!(char_by_char.0, "Hello");
        assert_eq!(whole.0, "Hello");
        assert_eq!(char_by_char.1, whole.1);
    }
}
