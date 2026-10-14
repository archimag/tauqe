use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;

use serde::Deserialize;
use struson::reader::{JsonReader, JsonStreamReader};
use tauqe_protocol::StructuredChangeProposal;

use super::{normalize_hunk, resolve_path, EditStreamFilter, StagingSimulator};
use crate::providers::StreamEvent;

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
    staging: StagingSimulator,
}

impl JsonStreamFilter {
    pub fn new(editable_paths: Vec<String>, repo_root: PathBuf) -> Self {
        Self {
            passthrough: None,
            buffer: String::new(),
            message_streamed_bytes: 0,
            message_finished: false,
            processed_changes_count: 0,
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
                self.staging.overwrite(&resolved_path, content);
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 1,
                });
            }
            "overwrite" => {
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
                            hunks_count: 0,
                        });
                    }
                }
            }
            "delete" => {
                events.push(StreamEvent::EditFileDone {
                    path: resolved_path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 0,
                });
            }
            "move" => {
                let to_raw = change.to.as_deref().unwrap_or_default();
                let resolved_to = resolve_path(to_raw, &self.editable_paths, true);
                let display_path = format!("{} -> {}", resolved_path, resolved_to);
                self.staging.apply_move(&resolved_path, &resolved_to);
                events.push(StreamEvent::EditFileDone {
                    path: display_path,
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

                let val_res = self.staging.apply_hunk(&resolved_path, &old_text, &new_text);
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

    fn finish(self: Box<Self>) -> Vec<StreamEvent> {
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
}

/// Generic streaming filter for a single top-level string field of a JSON
/// Structured Output payload.
///
/// The filter accumulates raw provider chunks, decodes the requested string
/// field (`message`, `summary`, ...) on the fly without leaking JSON syntax to
/// the user, and, once streaming ends, deserializes the full buffered payload
/// into a strictly typed structure `T`. Reusing
/// [`extract_streamed_string_field`] keeps escape decoding, UTF-16 surrogate
/// pairing and code fence tolerance identical to the edit streaming contour.
pub struct JsonFieldStreamFilter<T> {
    target_field: String,
    buffer: String,
    streamed_bytes: usize,
    finished: bool,
    _marker: PhantomData<fn() -> T>,
}

impl<T> JsonFieldStreamFilter<T>
where
    T: for<'de> Deserialize<'de>,
{
    /// Creates a filter that streams the top-level string field `target_field`.
    pub fn new(target_field: impl Into<String>) -> Self {
        Self {
            target_field: target_field.into(),
            buffer: String::new(),
            streamed_bytes: 0,
            finished: false,
            _marker: PhantomData,
        }
    }

    /// Appends a raw chunk and returns the freshly decoded text of the target
    /// field, or `None` when no new readable text is available.
    ///
    /// Empty deltas are filtered out. Once the closing quote of the target
    /// field has streamed in, no further deltas are produced, yet the raw
    /// buffer keeps accumulating so [`finish`](Self::finish) can deserialize
    /// the complete payload.
    pub fn push_chunk(&mut self, chunk: &str) -> Option<String> {
        self.buffer.push_str(chunk);
        if self.finished {
            return None;
        }
        let (delta, new_streamed, finished) =
            extract_streamed_string_field(&self.buffer, &self.target_field, self.streamed_bytes);
        self.streamed_bytes = new_streamed;
        self.finished = finished;
        delta.filter(|text| !text.is_empty())
    }

    /// Returns the accumulated raw JSON buffer received so far.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Returns the accumulated raw JSON buffer received so far, including any
    /// surrounding markdown code fence.
    pub fn raw_buffer(&self) -> &str {
        &self.buffer
    }

    /// Deserializes the accumulated payload into `T`.
    ///
    /// The buffer is first unwrapped from any markdown code fence and trimmed
    /// to the outermost balanced JSON object, so wrapper prose around the
    /// payload does not break decoding.
    pub fn finish(self) -> Result<T, serde_json::Error> {
        let payload = prepare_json_payload(&self.buffer);
        serde_json::from_str(&payload)
    }
}

/// Removes a surrounding markdown code fence from `input`, returning the inner
/// payload. Input without a leading fence is returned trimmed and unchanged.
fn strip_code_fence(input: &str) -> &str {
    let trimmed = input.trim();
    let inner = match trimmed.strip_prefix("```") {
        Some(rest) => match rest.find('\n') {
            Some(newline) => &rest[newline + 1..],
            None => rest,
        },
        None => return trimmed,
    };
    match inner.rfind("```") {
        Some(idx) => inner[..idx].trim(),
        None => inner.trim(),
    }
}

/// Returns the byte offset just past the outermost balanced JSON object in `s`,
/// or `None` when the object is not closed. String and escape state is tracked
/// so braces inside string literals are ignored.
fn balanced_object_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strips markdown fences and trims `buffer` to the outermost balanced JSON
/// object so a strictly typed payload can be deserialized.
fn prepare_json_payload(buffer: &str) -> String {
    let unfenced = strip_code_fence(buffer);
    match unfenced.find('{') {
        Some(start) => {
            let candidate = &unfenced[start..];
            match balanced_object_end(candidate) {
                Some(end) => candidate[..end].to_string(),
                None => candidate.to_string(),
            }
        }
        None => unfenced.to_string(),
    }
}

/// Recognized structured edit operation names.
///
/// Confirms that a partially streamed change element carries a valid,
/// unambiguous edit signature before file edit reporting starts, avoiding
/// false edit starts on malformed or partial operations.
fn is_known_edit_op(op: &str) -> bool {
    const KNOWN_OPS: [&str; 5] = ["create", "overwrite", "delete", "move", "replace"];
    let op = op.trim();
    KNOWN_OPS.iter().any(|known| op.eq_ignore_ascii_case(known))
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
                        if is_known_edit_op(&op) && !trimmed_path.is_empty() {
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

/// Locates the byte offset right after the opening quote of a top-level string value
/// with the given `target_field` name. The scan is depth- and string-aware.
pub fn find_string_field_value_start(buffer: &str, target_field: &str) -> Option<usize> {
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
                    if &bytes[str_start..j] == target_field.as_bytes() {
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

/// Helper function to scan unescaped characters of an arbitrary string property in raw JSON.
pub fn extract_streamed_string_field(
    buffer: &str,
    field_name: &str,
    already_streamed: usize,
) -> (Option<String>, usize, bool) {
    let value_start = match find_string_field_value_start(buffer, field_name) {
        Some(idx) => idx,
        None => return (None, already_streamed, false),
    };

    let content_slice = &buffer[value_start..];

    // Split the raw JSON string body at the first unescaped closing quote. When
    // the quote has not streamed in yet, the body is a partial prefix and
    // `is_finished` stays false.
    let (raw_body, is_finished) = match find_json_string_end(content_slice) {
        Some(end) => (&content_slice[..end], true),
        None => (content_slice, false),
    };

    // Decode the body into UTF-8. Decoding stops before any trailing escape that
    // is not yet complete, so the decoded prefix stays byte-identical between
    // successive chunks. That invariant makes the byte-length delta below exact:
    // every decoded byte is emitted to the stream exactly once.
    let decoded = decode_json_string_body(raw_body);

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

/// Returns the byte offset of the first unescaped `"` in a JSON string body,
/// or `None` while the closing quote has not streamed in yet.
fn find_json_string_end(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Parses exactly four hexadecimal digits into a `u32` code unit.
fn parse_hex4(hex: &[u8]) -> Option<u32> {
    if hex.len() < 4 {
        return None;
    }
    let mut value = 0u32;
    for &byte in &hex[..4] {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            b'A'..=b'F' => u32::from(byte - b'A') + 10,
            _ => return None,
        };
        value = value * 16 + digit;
    }
    Some(value)
}

/// Decodes a JSON string body (the text between the surrounding quotes) into
/// UTF-8, tolerating a body that is still streaming in.
///
/// When the body ends inside an escape sequence, decoding stops before that
/// escape so the caller can wait for the remaining bytes. UTF-16 surrogate
/// pairs (`\uD800`..`\uDBFF` followed by `\uDC00`..`\uDFFF`) are combined into
/// their scalar value; unpaired surrogates and invalid escapes are replaced
/// with U+FFFD so every decoded byte maps deterministically to one code point.
fn decode_json_string_body(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = String::with_capacity(raw.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'\\' {
            match raw[i..].chars().next() {
                Some(ch) => {
                    decoded.push(ch);
                    i += ch.len_utf8();
                }
                None => break,
            }
            continue;
        }

        if i + 1 >= bytes.len() {
            break;
        }

        match bytes[i + 1] {
            b'"' => {
                decoded.push('"');
                i += 2;
            }
            b'\\' => {
                decoded.push('\\');
                i += 2;
            }
            b'/' => {
                decoded.push('/');
                i += 2;
            }
            b'b' => {
                decoded.push('\x08');
                i += 2;
            }
            b'f' => {
                decoded.push('\x0c');
                i += 2;
            }
            b'n' => {
                decoded.push('\n');
                i += 2;
            }
            b'r' => {
                decoded.push('\r');
                i += 2;
            }
            b't' => {
                decoded.push('\t');
                i += 2;
            }
            b'u' => {
                let hex_start = i + 2;
                if hex_start + 4 > bytes.len() {
                    break;
                }
                let code = match parse_hex4(&bytes[hex_start..hex_start + 4]) {
                    Some(code) => code,
                    None => {
                        decoded.push('\u{FFFD}');
                        i = hex_start + 4;
                        continue;
                    }
                };

                if (0xD800..=0xDBFF).contains(&code) {
                    let low_escape = hex_start + 4;
                    if low_escape + 2 > bytes.len() {
                        break;
                    }
                    if bytes[low_escape] != b'\\' || bytes[low_escape + 1] != b'u' {
                        decoded.push('\u{FFFD}');
                        i = low_escape;
                        continue;
                    }
                    let low_hex = low_escape + 2;
                    if low_hex + 4 > bytes.len() {
                        break;
                    }
                    match parse_hex4(&bytes[low_hex..low_hex + 4]) {
                        Some(low) if (0xDC00..=0xDFFF).contains(&low) => {
                            let scalar = 0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            decoded.push(char::from_u32(scalar).unwrap_or('\u{FFFD}'));
                            i = low_hex + 4;
                        }
                        _ => {
                            decoded.push('\u{FFFD}');
                            i = low_escape;
                        }
                    }
                } else if let Some(ch) = char::from_u32(code) {
                    decoded.push(ch);
                    i = hex_start + 4;
                } else {
                    decoded.push('\u{FFFD}');
                    i = hex_start + 4;
                }
            }
            other if other.is_ascii() => {
                decoded.push(char::from(other));
                i += 2;
            }
            _ => {
                decoded.push('\\');
                i += 1;
            }
        }
    }

    decoded
}

/// Helper function to scan unescaped characters of the "message" string property in raw JSON.
pub(crate) fn extract_streamed_message(
    buffer: &str,
    already_streamed: usize,
) -> (Option<String>, usize, bool) {
    extract_streamed_string_field(buffer, "message", already_streamed)
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
    fn test_extract_streamed_string_field_chunked() {
        let json = r#"{"status":"success","explanation":"Hello world!\nLine 2","items":[]}"#;
        let mut buf = String::new();
        let mut streamed = String::new();
        let mut streamed_bytes = 0;
        for c in json.chars() {
            buf.push(c);
            let (delta, new_bytes, _) =
                extract_streamed_string_field(&buf, "explanation", streamed_bytes);
            if let Some(d) = delta {
                streamed.push_str(&d);
            }
            streamed_bytes = new_bytes;
        }
        assert_eq!(streamed, "Hello world!\nLine 2");
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
                if path == "a.rs" && op_type == "create"
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
                    StreamEvent::EditFileDone { status, .. } if status == "ok"
                )),
                "split {split}"
            );
        }
    }

    #[test]
    fn test_extract_streamed_string_field_combines_surrogate_pairs() {
        let json = r#"{"message":"Hi \uD83D\uDE00 there"}"#;
        let mut buf = String::new();
        let mut streamed = String::new();
        let mut streamed_bytes = 0;
        for c in json.chars() {
            buf.push(c);
            let (delta, new_bytes, _) =
                extract_streamed_string_field(&buf, "message", streamed_bytes);
            if let Some(d) = delta {
                streamed.push_str(&d);
            }
            streamed_bytes = new_bytes;
        }
        assert_eq!(streamed, "Hi \u{1F600} there");
    }

    #[test]
    fn test_extract_streamed_string_field_handles_lone_and_invalid_surrogates() {
        // A lone high surrogate, a lone low surrogate and an invalid hex escape
        // must not panic and must decode deterministically as U+FFFD.
        let json = r#"{"message":"a\uD83Db\uDE00c\uZZZZd"}"#;
        let (delta, _, finished) = extract_streamed_string_field(json, "message", 0);
        assert!(finished);
        assert_eq!(
            delta.unwrap_or_default(),
            "a\u{FFFD}b\u{FFFD}c\u{FFFD}d"
        );
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

    #[derive(Deserialize)]
    struct FieldSample {
        message: String,
        count: u32,
    }

    #[test]
    fn json_field_stream_filter_decodes_escapes_char_by_char() {
        let json = r#"{"message":"Hello\nWorld\t\"quoted\"","count":7}"#;
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let mut streamed = String::new();
        for c in json.chars() {
            if let Some(delta) = filter.push_chunk(&c.to_string()) {
                streamed.push_str(&delta);
            }
        }
        assert_eq!(streamed, "Hello\nWorld\t\"quoted\"");

        let decoded = filter.finish().expect("payload must deserialize");
        assert_eq!(decoded.message, "Hello\nWorld\t\"quoted\"");
        assert_eq!(decoded.count, 7);
    }

    #[test]
    fn json_field_stream_filter_combines_utf16_surrogate_pairs() {
        let json = r#"{"message":"Hi \uD83D\uDE00!","count":1}"#;
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let mut streamed = String::new();
        for c in json.chars() {
            if let Some(delta) = filter.push_chunk(&c.to_string()) {
                streamed.push_str(&delta);
            }
        }
        assert_eq!(streamed, "Hi \u{1F600}!");
    }

    #[test]
    fn json_field_stream_filter_ignores_fields_after_target_closes() {
        let json = r#"{"message":"done","other":"leak?","count":2}"#;
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let mut streamed = String::new();
        for c in json.chars() {
            if let Some(delta) = filter.push_chunk(&c.to_string()) {
                streamed.push_str(&delta);
            }
        }
        assert_eq!(streamed, "done");
        // Trailing fields must never bleed into the streamed text.
        assert!(!streamed.contains("leak"));
    }

    #[test]
    fn json_field_stream_filter_finish_strips_code_fence() {
        let fenced = "```json\n{\"message\":\"ok\",\"count\":5}\n```";
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let mut streamed = String::new();
        for c in fenced.chars() {
            if let Some(delta) = filter.push_chunk(&c.to_string()) {
                streamed.push_str(&delta);
            }
        }
        assert_eq!(streamed, "ok");
        let decoded = filter.finish().expect("fenced payload must deserialize");
        assert_eq!(decoded.message, "ok");
        assert_eq!(decoded.count, 5);
    }

    #[test]
    fn json_field_stream_filter_accessors_expose_accumulated_buffer() {
        let json = r#"{"message":"abc","count":9}"#;
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        assert_eq!(filter.buffer(), "");
        assert_eq!(filter.raw_buffer(), "");
        let _ = filter.push_chunk(json);
        assert_eq!(filter.raw_buffer(), json);
        assert_eq!(filter.buffer(), json);
    }
}
