use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use tauqe_protocol::{EditFileOp, EditFileStatus, EditOperation, StructuredChangeProposal};

use super::{
    hunk_event, normalize_hunk, resolve_path, EditStreamFilter, HunkCoordinates, OwnedAnchor,
    StagingSimulator, TRUNCATED_EDIT_DIAGNOSTIC,
};
use crate::edits::staging::AnchorEdit;
use crate::providers::StreamEvent;

pub mod decode;
pub mod field;

use decode::StringFieldCursor;
pub use decode::{extract_streamed_string_field, find_string_field_value_start};
pub use field::JsonFieldStreamFilter;

/// Bytes of an element header that the early preview may inspect.
///
/// The operation and the path precede the payload in the schema, so a small
/// window names the file. The cap keeps the preview a constant-cost probe: a
/// header longer than the window is reported when its element completes instead
/// of being re-probed on every delta.
const ELEMENT_HEADER_PEEK_LIMIT: usize = 4096;

/// State-machine streaming parser for Structured Output JSON edits.
///
/// It streams the `message` field on the fly as `StreamEvent::TextDelta` while
/// lexing the `changes` array forward and memory-validating every completed
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
/// Every scan is incremental: the lexer carries the offsets it has already
/// classified, so a long generated response costs work proportional to the
/// appended deltas rather than to the accumulated payload.
pub struct JsonStreamFilter {
    /// None = undecided; Some(true) = provider returned plain text (not JSON), stream as-is.
    passthrough: Option<bool>,
    buffer: String,
    message_cursor: StringFieldCursor,
    message_finished: bool,
    /// Forward lexer over the `changes` array of the payload.
    lex: ChangesLex,
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
    /// Element whose header the early preview has already reported.
    peeked_element: Option<usize>,
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
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            passthrough: None,
            buffer: String::new(),
            message_cursor: StringFieldCursor::new(),
            message_finished: false,
            lex: ChangesLex::default(),
            has_emitted_edit_started: false,
            editable_paths,
            staging: StagingSimulator::new(HashMap::new(), repo_root),
            file_hunk_counts: HashMap::new(),
            open_file_path: None,
            open_file_display: String::new(),
            anchor_batches: HashMap::new(),
            peeked_element: None,
            open_file_unterminated: false,
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
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

    /// Deserializes every element the lexer has completed since the last call.
    ///
    /// An element range is delimited by its own braces, so it is final the
    /// moment it is reported: the rest of the payload never has to be re-read to
    /// validate it, and an element that already closed is never lexed again. A
    /// closed element that names no change entry is reported as malformed rather
    /// than dropped, because the early preview may already have announced the
    /// file its header names, and only a terminal event can correct that.
    fn scan_new_changes(&mut self) -> Vec<CompletedChange> {
        let ranges = self.lex.scan(&self.buffer);
        let mut completed = Vec::with_capacity(ranges.len());
        for range in ranges {
            let Some(element) = self.buffer.get(range) else {
                continue;
            };
            match serde_json::from_str::<StructuredChangeProposal>(element) {
                Ok(change) => completed.push(CompletedChange::Parsed(change)),
                Err(error) => {
                    let (op, path) = peek_element_header(element);
                    let change = MalformedChange {
                        op,
                        path,
                        diagnostic: format!("Malformed changes entry: {error}"),
                    };
                    completed.push(CompletedChange::Malformed(change));
                }
            }
        }
        completed
    }

    /// Reports every element the lexer has completed since the last call.
    ///
    /// Both call sites of `scan_new_changes` dispatch through here, so a closed
    /// element always reaches exactly one terminal report whatever payload it
    /// carried.
    fn drain_completed_changes(&mut self, events: &mut Vec<StreamEvent>) {
        for completed in self.scan_new_changes() {
            match completed {
                CompletedChange::Parsed(change) => self.process_completed_change(&change, events),
                CompletedChange::Malformed(change) => self.reject_malformed_change(change, events),
            }
        }
    }

    /// Completes the announced file with the defect of an element that names no change.
    ///
    /// The element closed, so nothing about it is truncated: the early preview
    /// opened the file whose header it could already read, while the payload the
    /// element delivered names no change entry at all. Completing that file from
    /// the truncation branch would claim the response ended inside an edit that
    /// arrived complete, so the file is completed with the defect its payload
    /// actually has, which is the entry the staging engine would refuse.
    fn reject_malformed_change(&mut self, change: MalformedChange, events: &mut Vec<StreamEvent>) {
        let MalformedChange {
            op,
            path,
            diagnostic,
        } = change;
        // The element closed, so the file its header announced is not truncated.
        self.open_file_unterminated = false;
        let (Some(op), Some(path)) = (op, path) else {
            // An element that names no file was never announced, so no terminal
            // event of the preview has to be corrected.
            tracing::warn!(%diagnostic, "discarding a malformed changes entry");
            return;
        };
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }
        let resolved = resolve_path(&path, &self.editable_paths, op == EditFileOp::Create);
        self.open_file(&resolved, op, events);
        let hunks_count = self.next_hunk_index(&resolved);
        self.fail_open_file(resolved, diagnostic, hunks_count, events);
    }

    /// Reports the file named by the element that is still streaming in.
    ///
    /// The operation and the path precede the payload in the schema, so the
    /// preview can open the file before its content arrives. The probe inspects
    /// at most [`ELEMENT_HEADER_PEEK_LIMIT`] bytes once per element, so the early
    /// report costs a bounded amount instead of a rescan of the payload on every
    /// delta.
    fn peek_incomplete_element(&mut self, events: &mut Vec<StreamEvent>) {
        let Some(start) = self.lex.element_start else {
            return;
        };
        if self.peeked_element == Some(start) {
            return;
        }
        let window_end = self
            .buffer
            .len()
            .min(start.saturating_add(ELEMENT_HEADER_PEEK_LIMIT));
        let window_end = floor_char_boundary(&self.buffer, window_end);
        let peeked = self.buffer.get(start..window_end).map(peek_element_header);
        let Some((op, path)) = peeked else {
            return;
        };
        let (Some(op), Some(path)) = (op, path) else {
            // A header the window cannot fully inspect is reported when the
            // element completes instead of being probed again on every delta.
            if window_end.saturating_sub(start) >= ELEMENT_HEADER_PEEK_LIMIT {
                self.peeked_element = Some(start);
            }
            return;
        };
        self.peeked_element = Some(start);
        let resolved = resolve_path(&path, &self.editable_paths, op == EditFileOp::Create);
        if self.open_file_path.as_deref() == Some(resolved.as_str()) {
            return;
        }
        self.flush_open_file(events);
        if !self.has_emitted_edit_started {
            events.push(StreamEvent::EditStarted);
            self.has_emitted_edit_started = true;
        }
        events.push(StreamEvent::EditFileStarted {
            path: resolved.clone(),
            op_type: op,
        });
        self.open_file_path = Some(resolved.clone());
        self.open_file_display = resolved;
        // The element is still streaming, so the file it announces is confirmed
        // only when `process_completed_change` sees the element close.
        self.open_file_unterminated = true;
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

        let operation = match change.to_edit_operation() {
            Ok(operation) => operation,
            Err(message) => {
                let is_create = change.op == EditFileOp::Create;
                let resolved = resolve_path(&change.path, &self.editable_paths, is_create);
                self.open_file(&resolved, change.op, events);
                let hunks_count = self.next_hunk_index(&resolved);
                self.fail_open_file(resolved, message, hunks_count, events);
                return;
            }
        };

        match operation {
            EditOperation::Create { path, content } => {
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
            EditOperation::Overwrite { path, content } => {
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
            EditOperation::Delete { path } => {
                let resolved = resolve_path(&path, &self.editable_paths, false);
                self.open_file(&resolved, EditFileOp::Delete, events);
                // No hunk content; the pending EditFileDone reports zero hunks.
                self.file_hunk_counts.entry(resolved).or_insert(0);
            }
            EditOperation::Move { from, to } => {
                let resolved_from = resolve_path(&from, &self.editable_paths, false);
                let resolved_to = resolve_path(&to, &self.editable_paths, true);
                self.open_file(&resolved_from, EditFileOp::Move, events);
                self.staging.apply_move(&resolved_from, &resolved_to);
                self.open_file_display = format!("{} -> {}", resolved_from, resolved_to);
                self.file_hunk_counts.entry(resolved_from).or_insert(0);
            }
            EditOperation::MoveDir { from, to } => {
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
            EditOperation::Replace {
                path,
                old_text,
                new_text,
            } => {
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
            EditOperation::LineAnchor {
                path,
                start,
                end,
                content,
            } => {
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
/// One element the forward lexer has completed.
///
/// The element already closed, so its payload is final: it either deserializes
/// into a change entry or names a payload no change entry accepts. The second
/// case is a defect of the payload, never a truncation of the response, so it
/// must not reach the terminal event of the early preview as one.
enum CompletedChange {
    /// The element deserialized into a change entry.
    Parsed(StructuredChangeProposal),
    /// The element closed but its payload names no change entry.
    Malformed(MalformedChange),
}

/// A completed `changes` element whose payload names no change entry.
struct MalformedChange {
    /// Operation the element declares before its payload, when it declares one.
    op: Option<EditFileOp>,
    /// Path the element declares before its payload, when it declares one.
    path: Option<String>,
    /// Diagnostic of the rejected payload.
    diagnostic: String,
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

        // 2. Lex the changes array forward and report the elements that closed.
        //    The lexer resumes where the previous chunk stopped, so a long payload
        //    is never re-read from its start.
        self.drain_completed_changes(&mut events);
        if self.lex.is_done() {
            self.flush_open_file(&mut events);
        }

        // 3. Early report of the file named by the still-incomplete element.
        self.peek_incomplete_element(&mut events);

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
        self.drain_completed_changes(&mut events);
        self.flush_open_file(&mut events);
        events
    }

    fn reset(&mut self) {
        self.passthrough = None;
        self.buffer.clear();
        self.message_cursor = StringFieldCursor::new();
        self.message_finished = false;
        self.lex = ChangesLex::default();
        self.has_emitted_edit_started = false;
        self.file_hunk_counts.clear();
        self.open_file_path = None;
        self.open_file_display.clear();
        self.anchor_batches.clear();
        self.peeked_element = None;
        self.open_file_unterminated = false;
        self.staging.reset();
    }
}

/// What the incremental payload lexer is currently looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LexPhase {
    /// Scanning the top-level object for the `changes` array.
    #[default]
    Header,
    /// The `changes` key has arrived; the opener of its value is still to be read.
    ArrayOpener,
    /// Walking the elements of the `changes` array.
    Elements,
    /// The array has closed, or the payload declares none.
    Done,
}

/// Incremental lexer over the `changes` array of a structured payload.
///
/// The lexer never re-reads a byte it has already classified. Every offset it
/// keeps is absolute and only moves forward with the appended provider deltas:
/// it locates the array opener once, tracks the brace depth and the string state
/// of the element under construction, and reports each element as soon as its
/// closing brace arrives. The per-chunk cost therefore follows the appended
/// bytes instead of the accumulated payload length.
/// One container the header scan has entered.
#[derive(Debug, Clone, Copy)]
enum HeaderContainer {
    /// A JSON object; `expect_key` marks the next string as a key.
    Object { expect_key: bool },
    /// A JSON array.
    Array,
}

#[derive(Debug, Default)]
struct ChangesLex {
    /// What the lexer is currently looking for.
    phase: LexPhase,
    /// Absolute offset of the next byte to classify.
    pos: usize,
    /// Whether the header scan has reached the opening `{` of the payload.
    started: bool,
    /// Nesting depth relative to the container the phase tracks.
    depth: usize,
    /// Containers open in the header scan, innermost last.
    containers: Vec<HeaderContainer>,
    /// Absolute offset of the opening quote of the string being scanned.
    string_start: usize,
    /// Absolute offset from which an in-progress string body is resumed.
    string_scan_from: usize,
    /// Whether the string scan stopped right after a lone escape byte.
    string_escaped: bool,
    /// Whether the lexer is inside a string literal.
    in_string: bool,
    /// Elements: absolute offset of the opening `{` of the current element.
    element_start: Option<usize>,
}

impl ChangesLex {
    /// Returns the byte ranges of the elements completed by the latest bytes.
    ///
    /// Each range is one balanced JSON object delimited by its own braces, so the
    /// caller can deserialize it in place without touching the rest of the
    /// payload.
    fn scan(&mut self, buffer: &str) -> Vec<Range<usize>> {
        let bytes = buffer.as_bytes();
        let mut completed = Vec::new();
        if self.phase == LexPhase::Header && !self.started {
            match buffer.get(self.pos..).and_then(|rest| rest.find('{')) {
                Some(offset) => {
                    self.pos += offset;
                    self.started = true;
                }
                None => return completed,
            }
        }
        while self.pos < bytes.len() {
            if self.in_string {
                if !self.advance_string(bytes) {
                    return completed;
                }
                self.in_string = false;
                self.after_string(buffer);
                continue;
            }
            match self.phase {
                LexPhase::Header => self.step_header(bytes),
                LexPhase::ArrayOpener => match resolve_array_opener(buffer, self.pos) {
                    OpenerOutcome::Found(open) => {
                        self.phase = LexPhase::Elements;
                        self.depth = 0;
                        self.element_start = None;
                        self.pos = open + 1;
                    }
                    OpenerOutcome::NotAnArray => self.phase = LexPhase::Done,
                    OpenerOutcome::Pending => return completed,
                },
                LexPhase::Elements => self.step_elements(bytes, &mut completed),
                LexPhase::Done => return completed,
            }
        }
        completed
    }

    /// Reports whether the lexer has nothing further to scan.
    fn is_done(&self) -> bool {
        self.phase == LexPhase::Done
    }

    /// Classifies one byte of the payload header.
    ///
    /// The scan tracks the containers it enters instead of a bare depth, so the
    /// `changes` key is recognized wherever the surface declares it: at the top
    /// level of a flat response, or inside the operational branch envelope of an
    /// exclusive surface.
    fn step_header(&mut self, bytes: &[u8]) {
        let Some(&byte) = bytes.get(self.pos) else {
            return;
        };
        match byte {
            b'{' => {
                self.containers
                    .push(HeaderContainer::Object { expect_key: true });
                self.pos += 1;
            }
            b'[' => {
                self.containers.push(HeaderContainer::Array);
                self.pos += 1;
            }
            b'}' | b']' => {
                self.containers.pop();
                self.pos += 1;
                if self.containers.is_empty() {
                    self.phase = LexPhase::Done;
                }
            }
            b',' => {
                if let Some(HeaderContainer::Object { expect_key }) = self.containers.last_mut() {
                    *expect_key = true;
                }
                self.pos += 1;
            }
            b'"' => self.begin_string(),
            _ => self.pos += 1,
        }
    }

    /// Classifies one byte of the `changes` array body.
    fn step_elements(&mut self, bytes: &[u8], completed: &mut Vec<Range<usize>>) {
        let Some(&byte) = bytes.get(self.pos) else {
            return;
        };
        match byte {
            b'"' => self.begin_string(),
            b'{' | b'[' => {
                if self.depth == 0 && self.element_start.is_none() {
                    self.element_start = Some(self.pos);
                }
                self.depth += 1;
                self.pos += 1;
            }
            b'}' | b']' => {
                if self.depth == 0 {
                    self.phase = LexPhase::Done;
                } else {
                    self.depth -= 1;
                    if self.depth == 0 {
                        if let Some(start) = self.element_start.take() {
                            completed.push(start..self.pos + 1);
                        }
                    }
                }
                self.pos += 1;
            }
            _ => self.pos += 1,
        }
    }

    /// Enters the string literal that opens at the current position.
    fn begin_string(&mut self) {
        self.in_string = true;
        self.string_start = self.pos;
        self.string_scan_from = self.pos + 1;
        self.string_escaped = false;
        self.pos += 1;
    }

    /// Advances an in-progress string body, reporting whether it closed.
    fn advance_string(&mut self, bytes: &[u8]) -> bool {
        let mut index = self.string_scan_from;
        let mut escaped = self.string_escaped;
        while let Some(&byte) = bytes.get(index) {
            if escaped {
                escaped = false;
                index += 1;
                continue;
            }
            match byte {
                b'\\' => {
                    escaped = true;
                    index += 1;
                }
                b'"' => {
                    self.pos = index + 1;
                    self.string_scan_from = index + 1;
                    self.string_escaped = false;
                    return true;
                }
                _ => index += 1,
            }
        }
        self.string_scan_from = index;
        self.string_escaped = escaped;
        self.pos = index;
        false
    }

    /// Records a completed header string as the key or the value it was.
    fn after_string(&mut self, buffer: &str) {
        if self.phase != LexPhase::Header {
            return;
        }
        let Some(HeaderContainer::Object { expect_key }) = self.containers.last_mut() else {
            return;
        };
        if !*expect_key {
            return;
        }
        *expect_key = false;
        let content = buffer
            .get(self.string_start + 1..self.pos.saturating_sub(1))
            .unwrap_or_default();
        if content == "changes" {
            self.phase = LexPhase::ArrayOpener;
        }
    }
}

/// How the search for the `changes` array opener ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenerOutcome {
    /// The array opens at the returned offset.
    Found(usize),
    /// The `changes` key names a value that is not an array.
    NotAnArray,
    /// The bytes seen so far cannot decide yet.
    Pending,
}

/// Resolves the opener of the value that follows the `changes` key.
///
/// The probe re-examines at most a handful of separator bytes, so a value split
/// across chunks costs a bounded amount instead of a scan of the payload.
fn resolve_array_opener(buffer: &str, from: usize) -> OpenerOutcome {
    let bytes = buffer.as_bytes();
    let mut index = from;
    let mut seen_colon = false;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b if b.is_ascii_whitespace() => index += 1,
            b':' if !seen_colon => {
                seen_colon = true;
                index += 1;
            }
            b'[' if seen_colon => return OpenerOutcome::Found(index),
            _ => return OpenerOutcome::NotAnArray,
        }
        if index.saturating_sub(from) > 256 {
            return OpenerOutcome::NotAnArray;
        }
    }
    OpenerOutcome::Pending
}

/// Reads the `op` and `path` fields an element declares before its payload.
///
/// The walk is depth- and string-aware, so a `path` mentioned inside the payload
/// of another field is never mistaken for the header field itself. Only a string
/// that already closed contributes a value: previewing a path literal that is
/// still streaming in would open the file under a prefix of its final path, and
/// the reported event chain would then depend on how the provider split its
/// deltas.
///
/// A completed element is read through the same walk, so the file an element
/// announces while it streams and the file its defect is reported against are
/// always the ones its own two fields name.
fn peek_element_header(element: &str) -> (Option<EditFileOp>, Option<String>) {
    let bytes = element.as_bytes();
    let mut index = 0;
    let mut depth = 0usize;
    let mut key: Option<String> = None;
    let mut op: Option<EditFileOp> = None;
    let mut path: Option<String> = None;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b'"' => {
                let Some((value, next)) = read_stream_string(element, index) else {
                    break;
                };
                index = next;
                if depth == 1 {
                    match key.take() {
                        None => key = Some(value),
                        Some(name) if name == "op" => op = value.parse::<EditFileOp>().ok(),
                        Some(name) if name == "path" => path = Some(value),
                        Some(_) => {}
                    }
                }
                if op.is_some() && path.is_some() {
                    break;
                }
            }
            _ => index += 1,
        }
    }
    (op, path)
}

/// Reads the JSON string that opens at `start`.
///
/// The offset one past the closing quote travels with the decoded value. `None`
/// reports a string that is still streaming in: its final value is unknown, so
/// the walk stops there instead of acting on a prefix.
fn read_stream_string(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => {
                if index + 1 >= bytes.len() {
                    break;
                }
                index += 2;
            }
            b'"' => {
                let raw = text.get(start..=index).unwrap_or_default();
                let value = serde_json::from_str::<String>(raw).unwrap_or_default();
                return Some((value, index + 1));
            }
            _ => index += 1,
        }
    }
    None
}

/// Moves `index` back to the nearest scalar boundary of `text`.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    let limit = text.len();
    if index > limit {
        index = limit;
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
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
    fn test_json_stream_filter_markdown_wrapper_no_panic() {
        let root = scratch_dir("json-markdown");
        let mut filter = JsonStreamFilter::new(vec!["test.rs".to_string()], root);

        let wrapped = "```json\n{\"message\": \"Hello!\", \"changes\": []}\n```";
        let events = filter.push_chunk(wrapped);
        assert!(!events.is_empty());
        let finish_events = Box::new(filter).finish();
        assert!(finish_events.is_empty() || matches!(&finish_events[0], StreamEvent::TextDelta(_)));
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
    fn test_backtick_plain_text_falls_back_to_passthrough() {
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
        assert_eq!(streamed, text);
    }

    #[test]
    fn test_short_backtick_plain_text_flushed_on_finish() {
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
        assert_eq!(streamed, text);
    }

    #[test]
    fn test_backtick_plain_text_streams_across_chunks() {
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
        assert_eq!(streamed, text);
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
}
