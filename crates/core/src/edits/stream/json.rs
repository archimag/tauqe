use std::collections::HashMap;
use std::path::PathBuf;

use crate::capabilities::PreviewRoute;
#[cfg(test)]
use crate::capabilities::PreviewRouteKind;
use crate::edits::EditCreate;
use crate::edits::EditDelete;
use crate::edits::EditFileOp;
use crate::edits::EditFileStatus;
use crate::edits::EditLineAnchor;
use crate::edits::EditMove;
use crate::edits::EditMoveDir;
use crate::edits::EditOperation;
use crate::edits::EditOverwrite;
use crate::edits::EditReplace;
use crate::edits::StructuredChangeProposal;

use super::{
    hunk_event, normalize_hunk, resolve_path, EditStreamFilter, HunkCoordinates, OwnedAnchor,
    StagingSimulator, TRUNCATED_EDIT_DIAGNOSTIC,
};
use crate::edits::staging::AnchorEdit;
use crate::providers::StreamEvent;

pub(crate) mod decode;
pub mod events;
pub mod field;
pub mod string;

#[cfg(test)]
use events::key_path;
use events::{PreviewEvents, PreviewState, PreviewUpdate};
pub use field::JsonFieldStreamFilter;

/// Speculative Structured Output JSON edit preview driven by json_skimmer.
///
/// It streams permitted `message` fields as `StreamEvent::TextDelta` while
/// routing the `changes` array and memory-validating every completed
/// element, emitting `EditFileStarted`, `EditHunk`, and `EditFileDone`.
/// Arbitrary field order is supported.
///
/// The filter synthesizes the same event chain the XML stream filter produces:
/// one `EditStarted`, one `EditFileStarted` per file, sequential `EditHunk`
/// entries, and a single `EditFileDone` per file carrying the total hunks. The
/// JSON-specific detail that the `changes` array only becomes readable once its
/// elements close is fully encapsulated here, so the user-facing stream is
/// indistinguishable from the XML experience.
///
/// The event router classifies each byte once. It does not search prose or
/// markdown wrappers for JSON; the buffer only serves completed element ranges.
/// The original response is decoded independently by the final serde decoder:
/// neither a previewed hunk nor CandidateEnd authorizes an edit.
pub struct JsonStreamFilter {
    buffer: String,
    preview: PreviewEvents,
    has_emitted_edit_started: bool,
    editable_paths: Vec<String>,
    staging: StagingSimulator,
    /// Number of hunks already emitted for each file path.
    file_hunk_counts: HashMap<String, usize>,
    /// Path of the file whose `EditFileDone` is still pending, if any.
    open_file_path: Option<String>,
    /// Path reported by the pending file's `EditFileDone` (differs for moves).
    open_file_display: String,
    /// Anchored ranges each file has already addressed.
    ///
    /// The entries of one file arrive separately while the array streams, so the
    /// accumulated ranges are re-applied to the original snapshot as one batch
    /// instead of being applied one after another to already mutated text.
    anchor_batches: HashMap<String, Vec<OwnedAnchor>>,
    /// Start of the header whose element has not closed yet, including a
    /// subsequent element that happens to address the already open file.
    pending_element: Option<usize>,
    /// Whether the open file was announced by a header peek whose element has
    /// not completed yet.
    ///
    /// The peek reports a file before its payload arrives, so the terminal event
    /// of such a file must know whether the element ever closed: an element the
    /// response left unterminated is dropped by the authoritative decoder, and
    /// the preview reports the truncation instead of certifying it.
    open_file_unterminated: bool,
}

impl JsonStreamFilter {
    /// Flat surface for direct filter tests; production supplies the exact
    /// capability projection through `with_routes`.
    #[cfg(test)]
    pub(crate) fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self::with_routes(
            editable_paths,
            repo_root,
            vec![
                PreviewRoute {
                    path: key_path(&["message"]),
                    conditions: Vec::new(),
                    kind: PreviewRouteKind::Text,
                },
                PreviewRoute {
                    path: key_path(&["changes"]),
                    conditions: Vec::new(),
                    kind: PreviewRouteKind::Changes {
                        allowed_ops: vec![
                            EditFileOp::Replace,
                            EditFileOp::Create,
                            EditFileOp::Delete,
                            EditFileOp::Move,
                            EditFileOp::MoveDir,
                            EditFileOp::Overwrite,
                        ],
                    },
                },
            ],
        )
    }

    pub(crate) fn with_routes(
        editable_paths: Vec<String>,
        repo_root: PathBuf,
        routes: Vec<PreviewRoute>,
    ) -> Self {
        Self {
            buffer: String::new(),
            preview: PreviewEvents::with_routes(routes),
            has_emitted_edit_started: false,
            editable_paths,
            staging: StagingSimulator::new(HashMap::new(), repo_root),
            file_hunk_counts: HashMap::new(),
            open_file_path: None,
            open_file_display: String::new(),
            anchor_batches: HashMap::new(),
            pending_element: None,
            open_file_unterminated: false,
        }
    }

    pub(crate) fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staging.set_staged_contents(staged_contents);
        self
    }

    /// Emits the pending `EditFileDone` for the currently open file, if any.
    ///
    /// Buffering the completion until the file changes or the array closes is
    /// single terminal event with the exact hunks count, matching the XML filter.
    ///
    /// The status follows the element rather than the announcement: a file whose
    /// element never completed reaches this flush only from `finish`, and it
    /// reports the truncation instead of a success the turn will not deliver.
    fn flush_open_file(&mut self, events: &mut Vec<StreamEvent>) {
        let unterminated = std::mem::take(&mut self.open_file_unterminated);
        if let Some(open_path) = self.open_file_path.take() {
            let hunks_count = self.file_hunk_counts.remove(&open_path).unwrap_or(0);
            let display = std::mem::take(&mut self.open_file_display);
            let final_path = if display.is_empty() {
                open_path
            } else {
                display
            };
            let (status, error) = if unterminated {
                (
                    EditFileStatus::Error,
                    Some(TRUNCATED_EDIT_DIAGNOSTIC.to_string()),
                )
            } else {
                (EditFileStatus::Ok, None)
            };
            events.push(StreamEvent::EditFileDone {
                path: final_path,
                status,
                error,
                hunks_count,
            });
        }
    }

    /// A closed entry can be malformed despite its already announced header.
    /// Its header is supplied by the router; no second header parser is needed.
    fn reject_malformed_change(
        &mut self,
        op: Option<String>,
        path: Option<String>,
        diagnostic: String,
        events: &mut Vec<StreamEvent>,
    ) {
        let (Some(op), Some(path)) = (op.and_then(|op| op.parse::<EditFileOp>().ok()), path) else {
            tracing::warn!(%diagnostic, "discarding a malformed changes entry");
            return;
        };
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }
        let resolved = resolve_path(&path, &self.editable_paths, op == EditFileOp::Create);
        // A malformed element closed; its earlier announcement is no longer
        // pending, even when it followed another edit of the same file.
        self.open_file_unterminated = false;
        self.open_file(&resolved, op, events);
        let hunks_count = self.next_hunk_index(&resolved);
        self.fail_open_file(resolved, diagnostic, hunks_count, events);
    }

    fn announce_header(
        &mut self,
        start: usize,
        op: EditFileOp,
        path: &str,
        events: &mut Vec<StreamEvent>,
    ) {
        self.pending_element = Some(start);
        self.open_file_unterminated = true;
        let resolved = resolve_path(path, &self.editable_paths, op == EditFileOp::Create);
        if self.open_file_path.as_deref() == Some(resolved.as_str()) {
            return;
        }
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }
        // The preceding file is complete when a different file's header arrives.
        self.open_file_unterminated = false;
        self.open_file(&resolved, op, events);
        self.open_file_unterminated = true;
    }

    fn handle_update(&mut self, update: PreviewUpdate, events: &mut Vec<StreamEvent>) {
        match update {
            PreviewUpdate::Text(text) => events.push(StreamEvent::TextDelta(text)),
            PreviewUpdate::Header {
                start, op, path, ..
            } => {
                if let Ok(op) = op.parse() {
                    self.announce_header(start, op, &path, events);
                }
            }
            PreviewUpdate::Change {
                start,
                end,
                op,
                path,
                ..
            } => {
                if self.pending_element == Some(start) {
                    self.pending_element = None;
                    self.open_file_unterminated = false;
                }
                let Some(element) = self.buffer.get(start..end) else {
                    return;
                };
                match serde_json::from_str::<StructuredChangeProposal>(element) {
                    Ok(change) => self.process_completed_change(&change, events),
                    Err(error) => self.reject_malformed_change(
                        op,
                        path,
                        format!("Malformed changes entry: {error}"),
                        events,
                    ),
                }
            }
            PreviewUpdate::ArrayEnd { .. } => {
                if self.pending_element.is_none() {
                    self.flush_open_file(events);
                }
            }
            PreviewUpdate::Invalid => {
                if self.open_file_path.is_some() {
                    self.open_file_unterminated = true;
                    self.flush_open_file(events);
                }
            }
            PreviewUpdate::Kind(_) => {}
        }
    }

    /// Opens `path` as the file the following hunks belong to.
    ///
    /// Completion of the previous file is buffered until the file changes, so
    /// several operations against one file collapse into a single terminal event
    /// carrying the exact hunks count, matching the XML filter.
    fn open_file(&mut self, path: &str, op: EditFileOp, events: &mut Vec<StreamEvent>) {
        if self.open_file_path.as_deref() == Some(path) {
            return;
        }
        self.flush_open_file(events);
        events.push(StreamEvent::EditFileStarted {
            path: path.to_string(),
            op_type: op,
        });
        self.open_file_path = Some(path.to_string());
        self.open_file_display = path.to_string();
    }

    /// Returns the index the next hunk of `path` is reported under.
    fn next_hunk_index(&self, path: &str) -> usize {
        self.file_hunk_counts.get(path).copied().unwrap_or(0)
    }

    /// Completes the open file with a rejection the staging engine shares.
    ///
    /// The preview reports exactly the entries the staging engine would refuse,
    /// so a change set the harness accepts never fails here and the diagnostic
    /// is the one the retrying event will show.
    fn fail_open_file(
        &mut self,
        path: String,
        message: String,
        hunks_count: usize,
        events: &mut Vec<StreamEvent>,
    ) {
        events.push(StreamEvent::EditFileDone {
            path: path.clone(),
            status: EditFileStatus::Error,
            error: Some(message),
            hunks_count,
        });
        self.file_hunk_counts.remove(&path);
        self.open_file_path = None;
        self.open_file_display.clear();
        self.open_file_unterminated = false;
    }

    /// Applies one completed `changes[i]` element and emits the corresponding
    /// hunk events, folding completion into the pending file.
    ///
    /// The shape of an edit is decided by the authoritative converter
    /// [`StructuredChangeProposal::to_edit_operation`], never re-derived from the
    /// individual fields the entry arrived with. An anchored replacement is
    /// therefore previewed as the line range it names instead of a
    /// search/replace block with an empty needle, and a malformed entry is
    /// reported with the diagnostic the staging engine would produce.
    fn process_completed_change(
        &mut self,
        change: &StructuredChangeProposal,
        events: &mut Vec<StreamEvent>,
    ) {
        // The element closed, so the file its header announced is confirmed and
        // its terminal event reports a delivered edit rather than truncation.
        self.open_file_unterminated = false;
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }

        let op = change.parsed_op().unwrap_or(EditFileOp::Replace);
        let operation = match change.to_edit_operation() {
            Ok(operation) => operation,
            Err(message) => {
                let is_create = op == EditFileOp::Create;
                let resolved = resolve_path(&change.path, &self.editable_paths, is_create);
                self.open_file(&resolved, op, events);
                let hunks_count = self.next_hunk_index(&resolved);
                self.fail_open_file(resolved, message, hunks_count, events);
                return;
            }
        };

        match operation {
            EditOperation::Create(EditCreate { path, content }) => {
                let resolved = resolve_path(&path, &self.editable_paths, true);
                self.open_file(&resolved, EditFileOp::Create, events);
                let hunk_index = self.next_hunk_index(&resolved);
                let content = normalize_hunk(&content);
                // A created file has no old side, so the hunk names only the
                // lines the payload introduces.
                events.push(hunk_event(
                    &resolved,
                    hunk_index,
                    HunkCoordinates::whole_file(None, &content),
                    String::new(),
                    content.clone(),
                ));
                self.staging.overwrite(&resolved, content);
                self.file_hunk_counts.insert(resolved, hunk_index + 1);
            }
            EditOperation::Overwrite(EditOverwrite { path, content }) => {
                let resolved = resolve_path(&path, &self.editable_paths, false);
                self.open_file(&resolved, EditFileOp::Overwrite, events);
                let hunk_index = self.next_hunk_index(&resolved);
                let new_content = normalize_hunk(&content);
                match self.staging.current_content(&resolved) {
                    Ok(old_text) => {
                        // A whole-file payload replaces the file from its first
                        // line to its last one, so both sides are counted from
                        // the content the preview received.
                        events.push(hunk_event(
                            &resolved,
                            hunk_index,
                            HunkCoordinates::whole_file(Some(&old_text), &new_content),
                            old_text,
                            new_content.clone(),
                        ));
                        self.staging.overwrite(&resolved, new_content);
                        self.file_hunk_counts.insert(resolved, hunk_index + 1);
                    }
                    Err(message) => self.fail_open_file(resolved, message, hunk_index, events),
                }
            }
            EditOperation::Delete(EditDelete { path }) => {
                let resolved = resolve_path(&path, &self.editable_paths, false);
                self.open_file(&resolved, EditFileOp::Delete, events);
                // No hunk content; the pending EditFileDone reports zero hunks.
                self.file_hunk_counts.entry(resolved).or_insert(0);
            }
            EditOperation::Move(EditMove { from, to }) => {
                let resolved_from = resolve_path(&from, &self.editable_paths, false);
                let resolved_to = resolve_path(&to, &self.editable_paths, true);
                self.open_file(&resolved_from, EditFileOp::Move, events);
                self.staging.apply_move(&resolved_from, &resolved_to);
                self.open_file_display = format!("{} -> {}", resolved_from, resolved_to);
                self.file_hunk_counts.entry(resolved_from).or_insert(0);
            }
            EditOperation::MoveDir(EditMoveDir { from, to }) => {
                let resolved_from = resolve_path(&from, &self.editable_paths, false);
                let resolved_to = resolve_path(&to, &self.editable_paths, true);
                self.open_file(&resolved_from, EditFileOp::MoveDir, events);
                // The preview reports the directory pair as one unit: the
                // subtree expands into per-file moves while the change set is
                // staged, so the stream announces the rename instead of
                // inventing files the payload never named.
                self.open_file_display = format!("{} -> {}", resolved_from, resolved_to);
                self.file_hunk_counts.entry(resolved_from).or_insert(0);
            }
            EditOperation::Replace(EditReplace {
                path,
                old_text,
                new_text,
            }) => {
                let resolved = resolve_path(&path, &self.editable_paths, false);
                self.open_file(&resolved, EditFileOp::Replace, events);
                let hunk_index = self.next_hunk_index(&resolved);
                let old_text = normalize_hunk(&old_text);
                let new_text = normalize_hunk(&new_text);
                match self.staging.apply_hunk(&resolved, &old_text, &new_text) {
                    Ok(coordinates) => {
                        events.push(hunk_event(
                            &resolved,
                            hunk_index,
                            coordinates,
                            old_text,
                            new_text,
                        ));
                        self.file_hunk_counts.insert(resolved, hunk_index + 1);
                    }
                    Err(message) => self.fail_open_file(resolved, message, hunk_index + 1, events),
                }
            }
            EditOperation::LineAnchor(EditLineAnchor {
                path,
                start,
                end,
                content,
            }) => {
                let resolved = resolve_path(&path, &self.editable_paths, false);
                self.open_file(&resolved, EditFileOp::Replace, events);
                let hunk_index = self.next_hunk_index(&resolved);
                let replacement = normalize_hunk(&content);
                self.anchor_batches
                    .entry(resolved.clone())
                    .or_default()
                    .push(OwnedAnchor {
                        start,
                        end,
                        content: replacement.clone(),
                    });
                // The anchors name the lines the context block rendered, so the
                // ranges of one file are one batch resolved against the original
                // snapshot, exactly as the staging engine applies them.
                let outcome = {
                    let batch: Vec<AnchorEdit<'_>> = self
                        .anchor_batches
                        .get(&resolved)
                        .map(|anchors| anchors.iter().map(OwnedAnchor::as_edit).collect())
                        .unwrap_or_default();
                    self.staging.apply_anchor_batch(&resolved, &batch)
                };
                match outcome {
                    Ok(previews) => {
                        // The snapshot text of the addressed range is the removed
                        // side of the diff, and its coordinates are the lines the
                        // anchors decoded to, so the preview shows an honest
                        // replacement at a real place instead of a bare addition.
                        if let Some(preview) = previews.last() {
                            events.push(hunk_event(
                                &resolved,
                                hunk_index,
                                preview.coordinates.clone(),
                                preview.replaced.clone(),
                                replacement,
                            ));
                            self.file_hunk_counts.insert(resolved, hunk_index + 1);
                        }
                    }
                    Err(message) => self.fail_open_file(resolved, message, hunk_index, events),
                }
            }
        }
    }
}
impl EditStreamFilter for JsonStreamFilter {
    fn exclude_disk_sources(&mut self, paths: std::collections::HashSet<String>) {
        self.staging.excluded_disk_sources = paths;
    }

    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        for update in self.preview.push_updates(chunk.as_bytes()) {
            self.handle_update(update, &mut events);
        }
        events
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let state = self.preview.finish();
        if self.open_file_path.is_some() {
            // A completed element is only a successful preview when the
            // enclosing response also closed. An incomplete final element (or
            // invalid suffix) must never certify the pending file.
            self.open_file_unterminated =
                self.pending_element.is_some() || state != PreviewState::CandidateEnd;
            self.flush_open_file(&mut events);
        }
        events
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.preview.reset();
        self.has_emitted_edit_started = false;
        self.file_hunk_counts.clear();
        self.open_file_path = None;
        self.open_file_display.clear();
        self.anchor_batches.clear();
        self.pending_element = None;
        self.open_file_unterminated = false;
        self.staging.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_paths::scratch_dir;

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
                    ..
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
    fn patch_mode_streams_only_message_and_resets() {
        let mut filter = JsonStreamFilter::with_routes(
            vec![],
            scratch_dir("patch-message"),
            vec![PreviewRoute {
                path: key_path(&["message"]),
                conditions: vec![],
                kind: PreviewRouteKind::Text,
            }],
        );
        for message in ["first", "second"] {
            let input = serde_json::json!({
                "action": {"kind": "edit", "patch": "*** Begin Patch\n*** End Patch",
                "changes": [{"op":"create","path":"unapproved.rs","content":"discard"}]},
                "message": message,
            })
            .to_string();
            let mut text = String::new();
            for byte in input.chars() {
                for event in filter.push_chunk(byte.encode_utf8(&mut [0; 4])) {
                    match event {
                        StreamEvent::TextDelta(delta) => text.push_str(&delta),
                        other => panic!("unexpected event: {other:?}"),
                    }
                }
            }
            assert_eq!(text, message);
            filter.reset();
        }
        assert!(Box::new(filter).finish().is_empty());
    }

    #[test]
    fn test_early_detection_active_file_streaming() {
        let root = scratch_dir("json-early");
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
        let root = scratch_dir("json-partial");
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
        let root = scratch_dir("json-after-changes");
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
    fn test_json_stream_filter_markdown_wrapper_has_no_preview() {
        let root = scratch_dir("json-markdown");
        let mut filter = JsonStreamFilter::new(vec!["test.rs".to_string()], root);

        let wrapped = "```json\n{\"message\": \"Hello!\", \"changes\": []}\n```";
        let events = filter.push_chunk(wrapped);
        assert!(events.is_empty());
        let finish_events = Box::new(filter).finish();
        assert!(finish_events.is_empty());
    }

    #[test]
    fn test_no_edit_started_for_empty_changes_streams_full_message() {
        let root = scratch_dir("json-no-edits");
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
        let root = scratch_dir("json-known-op");

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
        let root = scratch_dir("json-split-points");
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
        let root = scratch_dir("json-surrogate");
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
    fn test_backtick_plain_text_is_not_previewed() {
        let root = scratch_dir("json-backtick");
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
        assert!(streamed.is_empty());
    }

    #[test]
    fn test_short_backtick_plain_text_is_not_previewed() {
        let root = scratch_dir("json-short-backtick");
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
        assert!(streamed.is_empty());
    }

    #[test]
    fn test_backtick_plain_text_is_not_previewed_across_chunks() {
        let root = scratch_dir("json-backtick-chunks");
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
        assert!(streamed.is_empty());
    }

    #[test]
    fn test_multiple_replaces_same_file_accumulate_hunks() {
        let root = scratch_dir("json-multi-replace");
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
        let root = scratch_dir("json-identical");
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
    /// Builds the anchored replacement entry of one inclusive line range.
    fn anchored_change(path: &str, start: u16, end: u16, content: &str) -> serde_json::Value {
        serde_json::json!({
            "op": "replace",
            "path": path,
            "to": "",
            "start": crate::edits::anchor::encode_anchor(path, start),
            "end": crate::edits::anchor::encode_anchor(path, end),
            "content": content,
        })
    }

    /// Collects every reported hunk as `(index, old_text, new_text)`.
    fn hunks(events: &[StreamEvent]) -> Vec<(usize, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::EditHunk {
                    hunk_index,
                    old_text,
                    new_text,
                    ..
                } => Some((*hunk_index, old_text.clone(), new_text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Collects the terminal event of every file as `(status, hunks_count)`.
    fn completions(events: &[StreamEvent]) -> Vec<(EditFileStatus, usize)> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::EditFileDone {
                    status,
                    hunks_count,
                    ..
                } => Some((*status, *hunks_count)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_anchored_replace_previews_the_range_it_replaces() {
        let path = "crates/foo.rs";
        let root = scratch_dir("json-anchor-preview");
        let mut filter = JsonStreamFilter::new(vec![path.to_string()], root).with_staged_contents(
            HashMap::from([(path.to_string(), "alpha\nbeta\ngamma\n".to_string())]),
        );
        let input = serde_json::json!({
            "message": "Rewrite the middle line",
            "changes": [anchored_change(path, 1, 1, "BETA\n")],
        })
        .to_string();

        let mut events = filter.push_chunk(&input);
        assert_eq!(
            filter
                .staging
                .current_content(path)
                .expect("staged content"),
            "alpha\nBETA\ngamma\n"
        );
        events.extend(Box::new(filter).finish());

        // The anchored entry is previewed as the range it names: the replaced
        // snapshot text is the removed side, and no search/replace miss is
        // reported, because the entry never was a search/replace block.
        assert_eq!(
            hunks(&events),
            vec![(0, "beta".to_string(), "BETA\n".to_string())]
        );
        assert_eq!(completions(&events), vec![(EditFileStatus::Ok, 1)]);
        // The anchored range names the second line of the file, so the streamed
        // event states exactly that instead of leaving the coordinates unknown.
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::EditHunk {
                old_start: 2,
                old_lines: 1,
                new_start: 2,
                new_lines: 1,
                context_hint: Some(hint),
                ..
            } if hint == "alpha"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditStarted))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditFileStarted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn test_anchored_entries_of_one_file_apply_as_one_batch() {
        let path = "a.rs";
        let root = scratch_dir("json-anchor-batch");
        let mut filter = JsonStreamFilter::new(vec![path.to_string()], root).with_staged_contents(
            HashMap::from([(path.to_string(), "alpha\nbeta\ngamma\n".to_string())]),
        );
        let input = serde_json::json!({
            "message": "Rewrite two ranges",
            "changes": [
                anchored_change(path, 0, 0, "ALPHA\n"),
                anchored_change(path, 2, 2, "GAMMA\n"),
            ],
        })
        .to_string();

        let mut events = filter.push_chunk(&input);
        assert_eq!(
            filter
                .staging
                .current_content(path)
                .expect("staged content"),
            "ALPHA\nbeta\nGAMMA\n"
        );
        events.extend(Box::new(filter).finish());

        // Both ranges address the snapshot the context block rendered, so the
        // later entry never overwrites the result of the earlier one.
        assert_eq!(
            hunks(&events),
            vec![
                (0, "alpha".to_string(), "ALPHA\n".to_string()),
                (1, "gamma".to_string(), "GAMMA\n".to_string()),
            ]
        );
        assert_eq!(completions(&events), vec![(EditFileStatus::Ok, 2)]);
    }

    #[test]
    fn test_anchored_entry_beyond_the_file_reports_the_staging_rejection() {
        let path = "a.rs";
        let root = scratch_dir("json-anchor-beyond");
        let mut filter = JsonStreamFilter::new(vec![path.to_string()], root).with_staged_contents(
            HashMap::from([(path.to_string(), "alpha\nbeta\n".to_string())]),
        );
        let input = serde_json::json!({
            "message": "Rewrite",
            "changes": [anchored_change(path, 4, 4, "x\n")],
        })
        .to_string();

        let mut events = filter.push_chunk(&input);
        events.extend(Box::new(filter).finish());

        assert!(hunks(&events).is_empty());
        let (status, error) = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::EditFileDone { status, error, .. } => Some((*status, error.clone())),
                _ => None,
            })
            .expect("the file must be completed");
        assert_eq!(status, EditFileStatus::Error);
        let error = error.expect("the rejection must carry its diagnostic");
        assert!(error.contains("names no line"), "{error}");
    }
    #[test]
    fn test_unterminated_changes_element_never_reports_success() {
        let path = "a.rs";
        // The array opens an element that names the file and then ends inside
        // its payload: the element never balances, so the authoritative decoder
        // drops it while the preview already announced the file.
        let truncated = concat!(
            "{\"message\":\"Editing\",\"changes\":[",
            "{\"op\":\"create\",\"path\":\"a.rs\",\"to\":\"\",",
            "\"start\":\"\",\"end\":\"\",\"content\":\"BETA"
        );
        let root = scratch_dir("json-truncated-changes");
        let boundaries: Vec<usize> = truncated
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(truncated.len()))
            .collect();
        for split in boundaries {
            let mut filter = JsonStreamFilter::new(vec![path.to_string()], root.clone());
            let mut events = filter.push_chunk(&truncated[..split]);
            events.extend(filter.push_chunk(&truncated[split..]));
            events.extend(Box::new(filter).finish());

            let terminals: Vec<(EditFileStatus, Option<String>, usize)> = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::EditFileDone {
                        status,
                        error,
                        hunks_count,
                        ..
                    } => Some((*status, error.clone(), *hunks_count)),
                    _ => None,
                })
                .collect();
            assert_eq!(terminals.len(), 1, "split {split}: {terminals:?}");
            let (status, error, hunks_count) = &terminals[0];
            assert_eq!(*status, EditFileStatus::Error, "split {split}");
            assert_eq!(*hunks_count, 0, "split {split}");
            let error = error
                .as_deref()
                .expect("the truncation must carry its diagnostic");
            assert!(error.contains("truncated"), "split {split}: {error}");
        }
    }

    #[test]
    fn test_late_message_field_streams_after_a_large_anchored_change_set() {
        // `message` arrives after the whole `changes` array, so the field lookup
        // runs against a buffer that grows to the size of the change set. It must
        // follow the appended bytes instead of rescanning the array per delta.
        let count = 4_000usize;
        let paths: Vec<String> = (0..count)
            .map(|index| format!("src/file_{index}.rs"))
            .collect();
        let changes: Vec<serde_json::Value> = paths
            .iter()
            .map(|path| anchored_change(path, 0, 0, "ALPHA\n"))
            .collect();
        let input = serde_json::json!({
            "changes": changes,
            "message": "Rewritten",
        })
        .to_string();
        let staged: HashMap<String, String> = paths
            .iter()
            .map(|path| (path.clone(), "alpha\nbeta\n".to_string()))
            .collect();
        let chars: Vec<char> = input.chars().collect();

        let root = scratch_dir("json-late-message");
        let mut filter = JsonStreamFilter::new(paths, root).with_staged_contents(staged);
        let started = std::time::Instant::now();
        let mut events = Vec::new();
        for chunk in chars.chunks(5) {
            let text: String = chunk.iter().collect();
            events.extend(filter.push_chunk(&text));
        }
        events.extend(Box::new(filter).finish());
        let elapsed = started.elapsed();

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Rewritten");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditFileStarted { .. }))
                .count(),
            count
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "streaming a {} byte payload took {elapsed:?}; locating `message` must follow the appended bytes",
            input.len()
        );
    }
    #[test]
    fn test_replacement_reports_the_lines_it_addresses() {
        let path = "a.rs";
        let root = scratch_dir("json-replacement");
        let mut filter = JsonStreamFilter::new(vec![path.to_string()], root).with_staged_contents(
            HashMap::from([(path.to_string(), "alpha\nbeta\ngamma\n".to_string())]),
        );
        let input = serde_json::json!({
            "message": "Rewrite",
            "changes": [{
                "op": "replace",
                "path": path,
                "old_text": "beta\n",
                "new_text": "BETA\nBETA2\n",
            }],
        })
        .to_string();

        let mut events = filter.push_chunk(&input);
        events.extend(Box::new(filter).finish());

        // The replaced text sits on the second line and yields two lines, so the
        // streamed hunk names those coordinates instead of an unknown position.
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::EditHunk {
                    old_start: 2,
                    old_lines: 1,
                    new_start: 2,
                    new_lines: 2,
                    ..
                }
            )),
            "the hunk must carry the resolved coordinates: {events:?}"
        );
    }
    #[test]
    fn test_malformed_changes_element_reports_its_defect_instead_of_a_truncation() {
        let path = "a.rs";
        // The element closes with a balanced payload no change entry accepts.
        // The early preview announces the file from its header, so the terminal
        // event must name the defect of the payload instead of claiming the
        // response ended inside the edit.
        let input = concat!(
            "{\"message\":\"Editing\",\"changes\":[",
            "{\"op\":\"create\",\"path\":\"a.rs\",\"to\":\"\",",
            "\"start\":\"\",\"end\":\"\",\"content\":5}]}"
        );
        let root = scratch_dir("json-malformed-element");
        let boundaries: Vec<usize> = input
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(input.len()))
            .collect();
        for split in boundaries {
            let mut filter = JsonStreamFilter::new(vec![path.to_string()], root.clone());
            let mut events = filter.push_chunk(&input[..split]);
            events.extend(filter.push_chunk(&input[split..]));
            events.extend(Box::new(filter).finish());

            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::EditFileStarted { .. }))
                    .count(),
                1,
                "split {split}"
            );
            let terminals: Vec<(EditFileStatus, Option<String>, usize)> = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::EditFileDone {
                        status,
                        error,
                        hunks_count,
                        ..
                    } => Some((*status, error.clone(), *hunks_count)),
                    _ => None,
                })
                .collect();
            assert_eq!(terminals.len(), 1, "split {split}: {terminals:?}");
            let (status, error, hunks_count) = &terminals[0];
            assert_eq!(*status, EditFileStatus::Error, "split {split}");
            assert_eq!(*hunks_count, 0, "split {split}");
            let error = error
                .as_deref()
                .expect("the rejection must carry its diagnostic");
            assert!(
                !error.contains("truncated"),
                "split {split}: the closed element is no truncation: {error}"
            );
            assert!(
                error.to_lowercase().contains("malformed"),
                "split {split}: the diagnostic must name the payload defect: {error}"
            );
        }
    }
    #[test]
    fn test_nested_branch_changes_stream_the_hunk_events() {
        let path = "a.rs";
        let root = scratch_dir("json-nested-changes");
        let mut filter = JsonStreamFilter::with_routes(
            vec![path.to_string()],
            root,
            crate::capabilities::preview_routes(
                &crate::llm::step::turn_capability_expr(),
                crate::workflow::PatchStrategy::SearchReplace,
            ),
        )
        .with_staged_contents(HashMap::from([(
            path.to_string(),
            "alpha\nbeta\n".to_string(),
        )]));
        // The contract admits the nested changes, but not action.message.
        let input = serde_json::json!({
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "replace",
                    "path": path,
                    "old_text": "beta\n",
                    "new_text": "BETA\n",
                }],
                "message": "Nested edit",
            },
        })
        .to_string();

        let mut events = Vec::new();
        for c in input.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditStarted))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::EditFileStarted { .. }))
                .count(),
            1
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::EditHunk {
                    old_start: 2,
                    old_lines: 1,
                    new_start: 2,
                    new_lines: 1,
                    ..
                }
            )),
            "the nested change must stream its coordinates: {events:?}"
        );
        assert_eq!(completions(&events), vec![(EditFileStatus::Ok, 1)]);
    }

    #[test]
    fn test_error_message_never_becomes_the_response_channel() {
        let path = "a.rs";
        let root = scratch_dir("json-error-message-channel");
        let mut filter = JsonStreamFilter::new(vec![path.to_string()], root);
        // The error channel is its own property: a failing edit round reports
        // `error_message`, and the filter must never stream it as the answer.
        let input = serde_json::json!({
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "",
                "error_message": "Missing dependency",
                "reason": ""
            },
        })
        .to_string();

        let mut events = Vec::new();
        for c in input.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.is_empty(),
            "the error channel must never reach the answer stream: {text}"
        );
    }

    #[test]
    fn test_unadvertised_branch_message_is_not_streamed() {
        let path = "a.rs";
        let root = scratch_dir("json-nested-single-message");
        let mut filter = JsonStreamFilter::with_routes(
            vec![path.to_string()],
            root,
            crate::capabilities::preview_routes(
                &crate::llm::step::turn_capability_expr(),
                crate::workflow::PatchStrategy::SearchReplace,
            ),
        );
        // The created content spells the field name, and the only real `message`
        // key sits inside the branch: only the nested change is previewable,
        // never the branch message or the content of the change set.
        let input = serde_json::json!({
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "create",
                    "path": path,
                    "content": "let message = \"message\";\n",
                }],
                "message": "Created the module",
            },
        })
        .to_string();

        let mut events = Vec::new();
        for c in input.chars() {
            events.extend(filter.push_chunk(&c.to_string()));
        }
        events.extend(Box::new(filter).finish());

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "");
        assert!(!text.contains("let message"), "{text}");
        assert_eq!(completions(&events), vec![(EditFileStatus::Ok, 1)]);
    }

    #[test]
    fn contract_routes_restrict_branches_and_text() {
        use crate::llm::{CapabilityTree, LlmCapability};
        use crate::workflow::PatchStrategy;

        let tree = CapabilityTree::any_of(
            "action",
            [
                CapabilityTree::named_set(
                    "create",
                    [CapabilityTree::leaf(LlmCapability::EditCreate)],
                ),
                CapabilityTree::named_set(
                    "delete",
                    [CapabilityTree::leaf(LlmCapability::EditDelete)],
                ),
            ],
        );
        let routes = crate::capabilities::preview_routes(&tree, PatchStrategy::SearchReplace);
        for (kind, op, expected) in [
            ("create", "create", true),
            ("create", "delete", false),
            ("delete", "delete", true),
            ("delete", "create", false),
        ] {
            let input = format!(
                "{{\"message\":\"hidden\",\"metadata\":{{\"message\":\"hidden\"}},\"action\":{{\"changes\":[{{\"op\":\"{op}\",\"path\":\"a.rs\",\"content\":\"new\"}}],\"kind\":\"{kind}\"}}}}"
            );
            let mut filter = JsonStreamFilter::with_routes(
                vec!["a.rs".into()],
                scratch_dir("json-branch-permissions"),
                routes.clone(),
            );
            let mut events = filter.push_chunk(&input);
            events.extend(Box::new(filter).finish());
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::EditFileStarted { .. })),
                expected,
                "{kind}/{op}: {events:?}"
            );
            assert!(!events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta(_))));
        }
    }

    #[test]
    fn unfinished_second_entry_of_same_file_does_not_complete_successfully() {
        let input = concat!(
            "{\"changes\":[{\"op\":\"create\",\"path\":\"a.rs\",\"content\":\"first\"},",
            "{\"op\":\"create\",\"path\":\"a.rs\",\"content\":\"second"
        );
        let mut filter =
            JsonStreamFilter::new(vec!["a.rs".into()], scratch_dir("json-same-truncated"));
        let mut events = filter.push_chunk(input);
        events.extend(Box::new(filter).finish());
        assert_eq!(hunks(&events).len(), 1);
        assert_eq!(completions(&events), vec![(EditFileStatus::Error, 1)]);
    }

    #[test]
    fn invalid_after_completed_entry_stops_preview_until_reset() {
        let mut filter =
            JsonStreamFilter::new(vec!["a.rs".into()], scratch_dir("json-invalid-reset"));
        let mut events = filter.push_chunk(
            "{\"changes\":[{\"op\":\"create\",\"path\":\"a.rs\",\"content\":\"first\"},!",
        );
        events.extend(
            filter.push_chunk("{\"op\":\"create\",\"path\":\"b.rs\",\"content\":\"ignored\"}]}"),
        );
        assert_eq!(hunks(&events).len(), 1);
        assert_eq!(completions(&events), vec![(EditFileStatus::Error, 1)]);
        filter.reset();
        let mut fresh = filter.push_chunk(
            "{\"message\":\"fresh\",\"changes\":[{\"op\":\"create\",\"path\":\"a.rs\",\"content\":\"new\"}]}"
        );
        fresh.extend(Box::new(filter).finish());
        assert_eq!(summarize(&fresh).0, "fresh");
        assert_eq!(completions(&fresh), vec![(EditFileStatus::Ok, 1)]);
        assert_eq!(hunks(&fresh)[0].0, 0);
    }
}
