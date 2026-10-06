use std::collections::HashMap;

use serde::Deserialize;
use struson::reader::{JsonReader, JsonStreamReader};
use workbench_protocol::StructuredChangeProposal;

use crate::model::gateway::StreamEvent;

/// State-machine streaming parser for Structured Output JSON edits using struson.
///
/// It streams the `message` field on the fly as `StreamEvent::TextDelta` while
/// deserializing and memory-validating completed `changes[i]` elements, emitting
/// `EditFileStarted`, `EditHunk`, and `EditFileDone`. Arbitrary field order is supported.
pub struct JsonStreamFilter {
    /// None = undecided; Some(true) = provider returned plain text (not JSON), stream as-is.
    passthrough: Option<bool>,
    buffer: String,
    message_streamed_bytes: usize,
    message_finished: bool,
    processed_changes_count: usize,
    has_emitted_edit_started: bool,
    started_change_index: Option<usize>,
    editable_paths: Vec<String>,
    repo_root: std::path::PathBuf,
    staged_contents: HashMap<String, String>,
}

impl JsonStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: std::path::PathBuf) -> Self {
        Self {
            passthrough: None,
            buffer: String::new(),
            message_streamed_bytes: 0,
            message_finished: false,
            processed_changes_count: 0,
            has_emitted_edit_started: false,
            started_change_index: None,
            editable_paths,
            repo_root,
            staged_contents: HashMap::new(),
        }
    }

    pub fn with_staged_contents(mut self, staged_contents: HashMap<String, String>) -> Self {
        self.staged_contents = staged_contents;
        self
    }

    /// Process an incoming chunk of model text and generate corresponding `StreamEvent`s.
    pub fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
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

        // 1. Stream message field deltas on the fly
        if !self.message_finished {
            let (delta, new_streamed, finished) =
                extract_streamed_message(&self.buffer, self.message_streamed_bytes);
            if let Some(d) = delta {
                if !d.is_empty() {
                    events.push(StreamEvent::TextDelta(d));
                }
            }
            self.message_streamed_bytes = new_streamed;
            self.message_finished = finished;
        }

        // 2. Parse top-level JSON and extract completed changes[i] via struson
        let mut new_changes = Vec::new();
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
                            }
                        } else {
                            break 'outer;
                        }
                    } else if reader.skip_value().is_err() {
                        // Value for this property is not fully received yet
                        break 'outer;
                    }
                }
                // Do not explicitly call reader.end_object() as the streaming document
                // may still be incomplete; JsonStreamReader drops safely without panicking.
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
        if self.started_change_index != Some(self.processed_changes_count) {
            if let Some((op, path)) =
                extract_incomplete_change_op_and_path(&self.buffer, self.processed_changes_count)
            {
                if !self.has_emitted_edit_started {
                    events.push(StreamEvent::EditStarted);
                    self.has_emitted_edit_started = true;
                }
                let is_create = op.eq_ignore_ascii_case("create");
                let resolved_path = resolve_path(&path, &self.editable_paths, is_create);
                events.push(StreamEvent::EditFileStarted {
                    path: resolved_path,
                    op_type: op.to_lowercase(),
                });
                self.started_change_index = Some(self.processed_changes_count);
            }
        }

        events
    }

    /// Flush any remaining buffered message text when stream ends.
    pub fn finish(self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.passthrough == Some(true) {
            return events;
        }
        if !self.message_finished {
            let (delta, _, _) = extract_streamed_message(&self.buffer, self.message_streamed_bytes);
            if let Some(d) = delta {
                if !d.is_empty() {
                    events.push(StreamEvent::TextDelta(d));
                }
            }
        }
        events
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

        let is_create = change.op.eq_ignore_ascii_case("create");
        let resolved_path = resolve_path(&change.path, &self.editable_paths, is_create);
        let op_type = change.op.to_lowercase();

        if !already_started {
            events.push(StreamEvent::EditFileStarted {
                path: resolved_path.clone(),
                op_type: op_type.clone(),
            });
        }

        match op_type.as_str() {
            "create" => {
                let content = normalize_hunk(change.content.as_deref().unwrap_or_default());
                events.push(StreamEvent::EditHunk {
                    path: resolved_path.clone(),
                    hunk_index: 0,
                    old_text: String::new(),
                    new_text: content.clone(),
                });
                self.staged_contents.insert(resolved_path.clone(), content);
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 1,
                });
            }
            "delete" => {
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 0,
                });
            }
            _ => {
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

                let val_res = self.validate_hunk_in_memory(&resolved_path, &old_text, &new_text);
                match val_res {
                    Ok(()) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: "ok".to_string(),
                            error: None,
                            hunks_count: 1,
                        });
                    }
                    Err(err_msg) => {
                        events.push(StreamEvent::EditFileDone {
                            path: resolved_path,
                            status: "error".to_string(),
                            error: Some(err_msg),
                            hunks_count: 1,
                        });
                    }
                }
            }
        }
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

/// Extracts `(op, path)` for an incomplete element at `target_idx` in `changes` array.
fn extract_incomplete_change_op_and_path(
    buffer: &str,
    target_idx: usize,
) -> Option<(String, String)> {
    let brace_idx = buffer.find('{')?;
    let json_bytes = &buffer.as_bytes()[brace_idx..];
    let mut reader = JsonStreamReader::new(json_bytes);
    reader.begin_object().ok()?;
    while let Ok(true) = reader.has_next() {
        let name = reader.next_name().ok()?;
        if name == "changes" {
            reader.begin_array().ok()?;
            let mut current_idx = 0;
            while let Ok(true) = reader.has_next() {
                if current_idx < target_idx {
                    reader.skip_value().ok()?;
                    current_idx += 1;
                } else if current_idx == target_idx {
                    reader.begin_object().ok()?;
                    let mut op: Option<String> = None;
                    let mut path: Option<String> = None;
                    while let Ok(true) = reader.has_next() {
                        let field_name = match reader.next_name() {
                            Ok(n) => n.to_string(),
                            Err(_) => break,
                        };
                        match field_name.as_str() {
                            "op" => {
                                if let Ok(val) = reader.next_str() {
                                    op = Some(val.to_string());
                                } else {
                                    break;
                                }
                            }
                            "path" => {
                                if let Ok(val) = reader.next_str() {
                                    path = Some(val.to_string());
                                } else {
                                    break;
                                }
                            }
                            _ => {
                                if reader.skip_value().is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    if let (Some(op), Some(path)) = (op, path) {
                        let trimmed_path = path.trim().to_string();
                        if !trimmed_path.is_empty() {
                            return Some((op, trimmed_path));
                        }
                    }
                    return None;
                }
            }
            return None;
        } else {
            reader.skip_value().ok()?;
        }
    }
    None
}

/// Locates the byte offset right after the opening quote of the top-level `"message"`
/// string value. The scan is depth- and string-aware, so `"message"` occurrences nested
/// inside `changes` (or inside string values) are ignored and key order does not matter.
fn find_message_value_start(buffer: &str) -> Option<usize> {
    let bytes = buffer.as_bytes();
    let start = buffer.find('{')?;
    let len = bytes.len();
    let mut i = start;
    let mut depth = 0usize;
    let mut expect_key = false;

    while i < len {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                if depth == 1 {
                    expect_key = true;
                }
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    return None;
                }
            }
            b',' => {
                if depth == 1 {
                    expect_key = true;
                }
                i += 1;
            }
            b'"' => {
                let str_start = i + 1;
                let mut j = str_start;
                let mut closed = false;
                while j < len {
                    match bytes[j] {
                        b'\\' => j += 2,
                        b'"' => {
                            closed = true;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                if !closed {
                    return None;
                }
                if depth == 1 && expect_key {
                    expect_key = false;
                    if &bytes[str_start..j] == b"message" {
                        let mut k = j + 1;
                        while k < len && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k >= len || bytes[k] != b':' {
                            return None;
                        }
                        k += 1;
                        while k < len && bytes[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        if k < len && bytes[k] == b'"' {
                            return Some(k + 1);
                        }
                        return None;
                    }
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Helper function to scan unescaped characters of the "message" string property in raw JSON.
fn extract_streamed_message(
    buffer: &str,
    already_streamed: usize,
) -> (Option<String>, usize, bool) {
    let value_start = match find_message_value_start(buffer) {
        Some(idx) => idx,
        None => return (None, already_streamed, false),
    };

    let content_slice = &buffer[value_start..];
    let mut decoded = String::new();
    let mut chars = content_slice.char_indices().peekable();
    let mut is_finished = false;

    while let Some((_, c)) = chars.next() {
        if c == '\\' {
            if let Some((_, next_c)) = chars.next() {
                match next_c {
                    '"' => decoded.push('"'),
                    '\\' => decoded.push('\\'),
                    '/' => decoded.push('/'),
                    'b' => decoded.push('\x08'),
                    'f' => decoded.push('\x0c'),
                    'n' => decoded.push('\n'),
                    'r' => decoded.push('\r'),
                    't' => decoded.push('\t'),
                    'u' => {
                        let mut hex = String::new();
                        for _ in 0..4 {
                            if let Some((_, h)) = chars.next() {
                                hex.push(h);
                            }
                        }
                        if hex.len() == 4 {
                            if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(code) {
                                    decoded.push(ch);
                                }
                            }
                        }
                    }
                    other => decoded.push(other),
                }
            } else {
                break;
            }
        } else if c == '"' {
            is_finished = true;
            break;
        } else {
            decoded.push(c);
        }
    }

    if decoded.len() > already_streamed && decoded.is_char_boundary(already_streamed) {
        let delta = decoded[already_streamed..].to_string();
        (Some(delta), decoded.len(), is_finished)
    } else if decoded.len() > already_streamed {
        let valid_idx = decoded
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|&idx| idx >= already_streamed)
            .unwrap_or(decoded.len());
        let delta = decoded[valid_idx..].to_string();
        (Some(delta), decoded.len(), is_finished)
    } else {
        (None, already_streamed, is_finished)
    }
}

/// State-machine streaming parser for XML edits.
///
/// It intercepts XML edit blocks and emits semantic `StreamEvent::Edit*` events
/// while forwarding regular conversational text as `StreamEvent::TextDelta`.
pub struct XmlStreamFilter {
    buffer: String,
    in_edits_block: bool,
    current_file: Option<CurrentStreamingFile>,
    file_hunks_count: usize,
    editable_paths: Vec<String>,
    repo_root: std::path::PathBuf,
    // In-memory staged file state for validating consecutive hunks during streaming
    staged_contents: HashMap<String, String>,
}

struct CurrentStreamingFile {
    path: String,
    op_type: String, // "replace", "create", "delete"
}

impl XmlStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: std::path::PathBuf) -> Self {
        Self {
            buffer: String::new(),
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

    /// Process incoming chunk of model text and generate corresponding `StreamEvent`s.
    pub fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        loop {
            if !self.in_edits_block {
                // Check if an edit block starts
                if let Some(tag_idx) = find_any_edit_start(&self.buffer) {
                    if tag_idx > 0 {
                        let text_part = self.buffer[..tag_idx].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..tag_idx);
                    }

                    if self.buffer.starts_with("<workbench_edits>")
                        || self.buffer.starts_with("<workbench_edits")
                    {
                        if let Some(close_tag_bracket) = self.buffer.find('>') {
                            self.buffer.drain(..=close_tag_bracket);
                            self.in_edits_block = true;
                            events.push(StreamEvent::EditStarted);
                            continue;
                        } else {
                            // Incomplete opening tag, wait for more chunks
                            break;
                        }
                    } else {
                        // Tag like <edit path="..."> without wrapper
                        self.in_edits_block = true;
                        events.push(StreamEvent::EditStarted);
                        continue;
                    }
                } else {
                    // Safe to emit text if not holding a partial '<' at the very end
                    let emit_len = safe_text_emit_len(&self.buffer);
                    if emit_len > 0 {
                        let text_part = self.buffer[..emit_len].to_string();
                        events.push(StreamEvent::TextDelta(text_part));
                        self.buffer.drain(..emit_len);
                    }
                    break;
                }
            }

            // Inside edit block
            if self.current_file.is_none() {
                // Check if closing whole workbench_edits block
                if let Some(close_wb) = self.buffer.find("</workbench_edits>") {
                    self.buffer.drain(..close_wb + "</workbench_edits>".len());
                    self.in_edits_block = false;
                    continue;
                }

                // Look for start of a file tag: <edit, <create, <delete
                if let Some((tag_name, tag_start)) = find_file_tag_start(&self.buffer) {
                    if let Some(close_bracket) = self.buffer[tag_start..].find('>') {
                        let header = &self.buffer[tag_start..tag_start + close_bracket + 1];
                        let is_self_closing = header.ends_with("/>");

                        if let Some(path_attr) = extract_attr(header, "path") {
                            let is_create = tag_name == "create";
                            let resolved = resolve_path(&path_attr, &self.editable_paths, is_create);
                            let op_type = match tag_name.as_str() {
                                "create" => "create".to_string(),
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
                            // Missing path attribute
                            self.buffer.drain(..tag_start + close_bracket + 1);
                            continue;
                        }
                    } else {
                        // Incomplete header tag
                        break;
                    }
                } else {
                    // No new file tag found yet
                    break;
                }
            }

            // Processing content of current file
            if let Some((curr_path, curr_op_type)) = self
                .current_file
                .as_ref()
                .map(|c| (c.path.clone(), c.op_type.clone()))
            {
                let close_tag = if curr_op_type == "create" {
                    "</create>"
                } else {
                    "</edit>"
                };

                if curr_op_type == "create" {
                    if let Some(close_pos) = self.buffer.find(close_tag) {
                        let content = self.buffer[..close_pos].to_string();
                        self.buffer.drain(..close_pos + close_tag.len());

                        let norm_content = normalize_hunk(&content);

                        events.push(StreamEvent::EditHunk {
                            path: curr_path.clone(),
                            hunk_index: 0,
                            old_text: String::new(),
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
                    // Replace mode: check for completed <search>...</search> <replace>...</replace>
                    if let Some((old_text, new_text, hunk_end)) = find_next_hunk(&self.buffer) {
                        let hunk_idx = self.file_hunks_count;
                        self.file_hunks_count += 1;

                        // Memory validation of this hunk against target file content
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

                    // Check if file closed
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

    /// Flush any remaining buffered text when stream ends
    pub fn finish(mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            if let Some(curr) = self.current_file.take() {
                events.push(StreamEvent::EditFileDone {
                    path: curr.path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: self.file_hunks_count,
                });
            }
            if !self.buffer.trim().is_empty() && !self.buffer.contains('<') {
                events.push(StreamEvent::TextDelta(self.buffer));
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

fn safe_text_emit_len(buf: &str) -> usize {
    if let Some(last_lt) = buf.rfind('<') {
        let trailing = &buf[last_lt..];
        if "<workbench_edits>".starts_with(trailing)
            || "<edit".starts_with(trailing)
            || "<create".starts_with(trailing)
            || "<delete".starts_with(trailing)
        {
            return last_lt;
        }
    }
    buf.len()
}

fn find_any_edit_start(buf: &str) -> Option<usize> {
    let mut min_idx = None;
    for prefix in &["<workbench_edits", "<edit", "<create", "<delete"] {
        if let Some(idx) = buf.find(prefix) {
            min_idx = Some(min_idx.map_or(idx, |m: usize| m.min(idx)));
        }
    }
    min_idx
}

fn find_file_tag_start(buf: &str) -> Option<(String, usize)> {
    let mut candidates = Vec::new();
    for tag in &["edit", "create", "delete", "replace"] {
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
        if trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
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

fn resolve_path(candidate: &str, editable_paths: &[String], is_create: bool) -> String {
    crate::edits::protocol::utils::resolve_target_path_for_op(candidate, editable_paths, is_create)
        .unwrap_or_else(|_| {
            candidate
                .trim()
                .trim_matches('`')
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string()
        })
}

fn normalize_hunk(raw: &str) -> String {
    let s = if let Some(stripped) = raw.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = raw.strip_prefix('\n') {
        stripped
    } else {
        raw
    };

    if let Some(last_nl) = s.rfind('\n') {
        let trailing = &s[last_nl + 1..];
        if trailing.chars().all(|c| c == ' ' || c == '\t') {
            return s[..=last_nl].to_string();
        }
    }
    s.to_string()
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
        assert!(events1.iter().any(|e| matches!(e, StreamEvent::EditFileStarted { path, op_type } if path == "crates/foo.rs" && op_type == "replace")));
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

        // Feed chunks byte by byte / token by token without crashing
        let full_json = r#"{"message":"Rust is great","changes":[],"context_requests":[],"suggested_actions":[]}"#;
        for c in full_json.chars() {
            let s = c.to_string();
            let _ = filter.push_chunk(&s);
        }
        let finish_events = filter.finish();
        let mut text = String::new();
        for ev in finish_events {
            if let StreamEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
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
        events.extend(filter.finish());
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
        let finish_events = filter.finish();
        assert!(finish_events.is_empty() || matches!(&finish_events[0], StreamEvent::TextDelta(_)));
    }
}
