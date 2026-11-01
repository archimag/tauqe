//! Low-level JSON scanning and incremental escape decoding.
//!
//! This submodule owns the byte-level helpers shared by the edit-proposal
//! streaming filter (`JsonStreamFilter`) and the generic single-field streaming
//! filter (`JsonFieldStreamFilter`). It locates a top-level string field, splits
//! a still-streaming JSON string body at its closing quote, and decodes JSON
//! escapes — including UTF-16 surrogate pairs — incrementally as provider chunks
//! arrive, without ever slicing a multi-byte character at a non-boundary offset.
//!
//! It is also the single authoritative source for markdown code-fence stripping
//! (`strip_code_fence`) and balanced-object scanning (`balanced_object_end`),
//! reused by every structured-output parser across the engine.

/// Removes a surrounding markdown code fence from `input`, returning the inner
/// payload.
///
/// A ```` ```json ```` fence is preferred over a bare ```` ``` ```` fence, and
/// the fence may be preceded by wrapper prose. Input without a fence is returned
/// trimmed and unchanged. Only single-byte ASCII delimiters are inspected, so
/// multi-byte UTF-8 content is never sliced across a scalar boundary.
///
/// This is the single authoritative fence stripper reused by every
/// structured-output parser.
pub(crate) fn strip_code_fence(input: &str) -> &str {
    let trimmed = input.trim();
    let after = if let Some(idx) = trimmed.find("```json") {
        &trimmed[idx + 7..]
    } else if let Some(idx) = trimmed.find("```") {
        &trimmed[idx + 3..]
    } else {
        return trimmed;
    };
    match after.find("```") {
        Some(end) => after[..end].trim(),
        None => after.trim(),
    }
}

/// Returns the exclusive end offset of the balanced JSON object or array that
/// starts at `start` (an opening `{` or `[`), or `None` when it is not closed.
///
/// String and escape state is tracked so braces inside string literals are
/// ignored. The returned offset follows an ASCII delimiter, so it is always a
/// valid UTF-8 boundary.
pub(crate) fn balanced_object_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Returns the first balanced JSON object substring in `raw`, if any, starting
/// at the first `{` and ending at its matching `}`.
///
/// It tolerates a surrounding markdown code fence or wrapper prose and is the
/// single authoritative "first JSON object" extractor for structured-output
/// parsers; callers deserialize the returned slice themselves.
pub(crate) fn first_json_object_str(raw: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        if let Some(end) = balanced_object_end(raw, start) {
            return Some(&raw[start..end]);
        }
        from = start + 1;
    }
    None
}

/// Strips markdown fences and trims `buffer` to the outermost balanced JSON
/// object so a strictly typed payload can be deserialized.
pub(crate) fn prepare_json_payload(buffer: &str) -> String {
    let unfenced = strip_code_fence(buffer);
    match unfenced.find('{') {
        Some(start) => {
            let candidate = &unfenced[start..];
            match balanced_object_end(candidate, 0) {
                Some(end) => candidate[..end].to_string(),
                None => candidate.to_string(),
            }
        }
        None => unfenced.to_string(),
    }
}

/// Locates the byte offset right after the opening quote of a top-level string
/// value with the given `target_field` name.
///
/// This is the one-shot form of [`FieldLocator`]: it walks a complete payload in
/// one pass. Streaming callers hold a [`StringFieldCursor`], whose locator resumes
/// the same walk across chunks instead of restarting it.
pub fn find_string_field_value_start(buffer: &str, target_field: &str) -> Option<usize> {
    FieldLocator::default().locate(buffer, target_field)
}

/// What the incremental field locator is currently looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LocatePhase {
    /// Scanning for the opening `{` of the payload.
    #[default]
    Header,
    /// Walking the object body for the top-level key that names the field.
    Body,
    /// The key matched; the `:` of its value is still to be read.
    Colon,
    /// The `:` arrived; the opening quote of the value is still to be read.
    Value,
}

/// Incremental locator for a top-level string field of a streaming JSON object.
///
/// The locator never re-reads a byte it has already classified: the scan offset,
/// the brace depth, the key expectation, and the string and escape state all
/// travel between chunks. A field that appears late in the payload — after a large
/// `changes` array, for instance — therefore costs one pass over the whole document
/// instead of one pass per provider delta.
#[derive(Debug, Default)]
struct FieldLocator {
    /// What the locator is currently looking for.
    phase: LocatePhase,
    /// Absolute offset of the next byte to classify.
    pos: usize,
    /// Nesting depth relative to the opening `{` of the payload.
    depth: usize,
    /// Whether the next depth-1 string is an object key.
    expect_key: bool,
    /// Whether the locator is inside a string literal.
    in_string: bool,
    /// Absolute offset of the opening quote of the string being scanned.
    string_start: usize,
    /// Absolute offset from which the in-progress string body is resumed.
    string_scan_from: usize,
    /// Whether the string scan stopped right after a lone escape byte.
    string_escaped: bool,
    /// Offset just past the opening quote of the located value.
    value_start: Option<usize>,
    /// Whether the field is permanently absent from the payload.
    exhausted: bool,
}

impl FieldLocator {
    /// Advances the walk over `buffer` and returns the value offset once located.
    ///
    /// `None` reports that the payload has not delivered the field yet or that it
    /// never will. The walk keeps its progress either way, so the next chunk
    /// resumes from the byte it stopped at.
    fn locate(&mut self, buffer: &str, field_name: &str) -> Option<usize> {
        if self.value_start.is_some() {
            return self.value_start;
        }
        if self.exhausted {
            return None;
        }

        let bytes = buffer.as_bytes();
        if self.phase == LocatePhase::Header {
            // A `{` is a single byte, so the bytes searched once never have to be
            // searched again even when the payload opens after leading prose.
            match buffer.get(self.pos..).and_then(|rest| rest.find('{')) {
                Some(offset) => {
                    self.pos += offset;
                    self.phase = LocatePhase::Body;
                }
                None => {
                    self.pos = buffer.len();
                    return None;
                }
            }
        }

        while self.pos < bytes.len() {
            if self.in_string {
                if !self.advance_string(bytes) {
                    return None;
                }
                self.in_string = false;
                if self.after_string(buffer, field_name) {
                    self.phase = LocatePhase::Colon;
                }
                continue;
            }
            let byte = *bytes.get(self.pos)?;
            match self.phase {
                LocatePhase::Body => match byte {
                    b'"' => self.begin_string(),
                    b'{' | b'[' => {
                        self.depth += 1;
                        if self.depth == 1 {
                            self.expect_key = true;
                        }
                        self.pos += 1;
                    }
                    b'}' | b']' => {
                        self.depth = self.depth.saturating_sub(1);
                        self.pos += 1;
                        if self.depth == 0 {
                            // The object closed without the field: no later chunk
                            // can make it appear.
                            self.exhausted = true;
                            return None;
                        }
                    }
                    b',' => {
                        if self.depth == 1 {
                            self.expect_key = true;
                        }
                        self.pos += 1;
                    }
                    _ => self.pos += 1,
                },
                LocatePhase::Colon => match byte {
                    b':' => {
                        self.phase = LocatePhase::Value;
                        self.pos += 1;
                    }
                    b if b.is_ascii_whitespace() => self.pos += 1,
                    _ => {
                        // The key names a value that is not a plain string, so the
                        // field can never be streamed out of this payload.
                        self.exhausted = true;
                        return None;
                    }
                },
                LocatePhase::Value => match byte {
                    b'"' => {
                        self.value_start = Some(self.pos + 1);
                        return self.value_start;
                    }
                    b if b.is_ascii_whitespace() => self.pos += 1,
                    _ => {
                        self.exhausted = true;
                        return None;
                    }
                },
                // The header is resolved above, before the body walk starts.
                LocatePhase::Header => return None,
            }
        }
        None
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

    /// Records what a completed string was, reporting whether it named the field.
    ///
    /// Only a string that closed at the top level while a key was expected can name
    /// the field, so the same text inside another value never matches.
    fn after_string(&mut self, buffer: &str, field_name: &str) -> bool {
        if self.depth != 1 || !self.expect_key {
            return false;
        }
        self.expect_key = false;
        let body = buffer
            .get(self.string_start.saturating_add(1)..self.pos.saturating_sub(1))
            .unwrap_or_default();
        body == field_name
    }
}

/// Incremental cursor for streaming a single top-level JSON string field.
///
/// It records where the field value starts and how far the field lookup, the
/// closing-quote scan, and the escape decode have progressed. Successive chunks
/// therefore extend the existing scans and decode instead of restarting from byte
/// zero, so a long streamed field stays linear in the number of received bytes even
/// when it follows a large preceding payload.
#[derive(Debug, Default)]
pub struct StringFieldCursor {
    /// Locates the field inside the payload the chunks accumulate.
    locator: FieldLocator,
    end_scanned_pos: usize,
    raw_decoded_pos: usize,
}

impl StringFieldCursor {
    /// Creates a cursor positioned at the start of a fresh payload.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Helper function to stream unescaped characters of an arbitrary string
/// property in raw JSON, resuming the field lookup, closing-quote scan and
/// escape decode from `cursor`.
///
/// It returns the newly decoded text (or `None` when no new readable text is
/// available) and whether the field's closing quote has streamed in. Decoding
/// stops before any trailing escape that is not yet complete, so the decoded
/// prefix stays stable between chunks and every decoded byte is emitted exactly
/// once.
pub fn extract_streamed_string_field(
    buffer: &str,
    field_name: &str,
    cursor: &mut StringFieldCursor,
) -> (Option<String>, bool) {
    let Some(val_start) = cursor.locator.locate(buffer, field_name) else {
        return (None, false);
    };
    let content_slice = &buffer[val_start..];
    let (raw_body, is_finished) =
        match find_json_string_end_from(content_slice, &mut cursor.end_scanned_pos) {
            Some(end) => (&content_slice[..end], true),
            None => (content_slice, false),
        };

    let decoded = decode_json_string_body_incremental(raw_body, &mut cursor.raw_decoded_pos);
    if decoded.is_empty() {
        (None, is_finished)
    } else {
        (Some(decoded), is_finished)
    }
}

/// Returns the byte offset of the first unescaped `"` in a JSON string body,
/// resuming from `from` to avoid O(n^2) rescanning across chunks.
pub(crate) fn find_json_string_end_from(raw: &str, from: &mut usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut i = *from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 < bytes.len() {
                    i += 2;
                } else {
                    break;
                }
            }
            b'"' => {
                *from = i;
                return Some(i);
            }
            _ => i += 1,
        }
    }
    *from = i;
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

/// Decodes a JSON string body incrementally starting from `raw_pos`.
///
/// It mutates `raw_pos` up to the last fully decoded character or escape, withholding
/// incomplete trailing escapes so chunks can be appended without re-decoding from zero.
pub(crate) fn decode_json_string_body_incremental(raw: &str, raw_pos: &mut usize) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = String::new();
    let mut i = *raw_pos;

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

    *raw_pos = i;
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_streamed_string_field_chunked() {
        let json = r#"{"status":"success","explanation":"Hello world!\nLine 2","items":[]}"#;
        let mut buf = String::new();
        let mut streamed = String::new();
        let mut cursor = StringFieldCursor::new();
        for c in json.chars() {
            buf.push(c);
            let (delta, _) = extract_streamed_string_field(&buf, "explanation", &mut cursor);
            if let Some(d) = delta {
                streamed.push_str(&d);
            }
        }
        assert_eq!(streamed, "Hello world!\nLine 2");
    }

    #[test]
    fn test_extract_streamed_string_field_combines_surrogate_pairs() {
        let json = r#"{"message":"Hi \uD83D\uDE00 there"}"#;
        let mut buf = String::new();
        let mut streamed = String::new();
        let mut cursor = StringFieldCursor::new();
        for c in json.chars() {
            buf.push(c);
            let (delta, _) = extract_streamed_string_field(&buf, "message", &mut cursor);
            if let Some(d) = delta {
                streamed.push_str(&d);
            }
        }
        assert_eq!(streamed, "Hi \u{1F600} there");
    }

    #[test]
    fn test_extract_streamed_string_field_handles_lone_and_invalid_surrogates() {
        // A lone high surrogate, a lone low surrogate and an invalid hex escape
        // must not panic and must decode deterministically as U+FFFD.
        let json = r#"{"message":"a\uD83Db\uDE00c\uZZZZd"}"#;
        let mut cursor = StringFieldCursor::new();
        let (delta, finished) = extract_streamed_string_field(json, "message", &mut cursor);
        assert!(finished);
        assert_eq!(delta.unwrap_or_default(), "a\u{FFFD}b\u{FFFD}c\u{FFFD}d");
    }

    #[test]
    fn test_incremental_field_location_matches_the_one_shot_scan() {
        // Every split point must locate the same field at the same offset: the
        // resumable walk may not depend on how the provider split its deltas.
        let json = concat!(
            r#"{"alpha":1,"changes":[{"op":"create","path":"a.rs","content":"#,
            r#""{\"message\":\"inner\"}"}],"message":"Late \u0041 value"}"#,
        );
        let expected = find_string_field_value_start(json, "message");
        assert!(expected.is_some());

        for split in json
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(json.len()))
        {
            let mut cursor = StringFieldCursor::new();
            let mut streamed = String::new();
            if let (Some(delta), _) =
                extract_streamed_string_field(&json[..split], "message", &mut cursor)
            {
                streamed.push_str(&delta);
            }
            let (delta, finished) = extract_streamed_string_field(json, "message", &mut cursor);
            if let Some(delta) = delta {
                streamed.push_str(&delta);
            }
            assert_eq!(streamed, "Late A value", "split {split}");
            assert!(finished, "split {split}");
            assert_eq!(cursor.locator.value_start, expected, "split {split}");
        }
    }

    #[test]
    fn test_late_field_location_follows_the_appended_bytes() {
        // The field arrives after a large preceding value. A locator that restarts
        // from byte zero on every delta pays a full buffer pass per chunk; one that
        // resumes from the recorded offsets pays one pass over the whole payload.
        let json = format!(
            "{{\"reasoning\":\"{}\",\"message\":\"Late answer\"}}",
            "x".repeat(400_000)
        );
        let chars: Vec<char> = json.chars().collect();
        let mut buffer = String::new();
        let mut streamed = String::new();
        let mut cursor = StringFieldCursor::new();
        let started = std::time::Instant::now();
        for chunk in chars.chunks(5) {
            buffer.extend(chunk);
            let (delta, _) = extract_streamed_string_field(&buffer, "message", &mut cursor);
            if let Some(delta) = delta {
                streamed.push_str(&delta);
            }
        }
        let elapsed = started.elapsed();

        assert_eq!(streamed, "Late answer");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "locating a late field in a {} byte payload took {elapsed:?}; the per-chunk cost must follow the appended bytes",
            json.len()
        );
    }
}
