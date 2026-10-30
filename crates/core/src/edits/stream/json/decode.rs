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

/// Incremental cursor for streaming a single top-level JSON string field.
///
/// It records where the field value starts and how far the closing-quote scan
/// and the escape decode have progressed. Successive chunks therefore extend
/// the existing scan and decode instead of restarting from byte zero, so a long
/// streamed field stays linear in the number of received bytes.
#[derive(Debug, Default)]
pub struct StringFieldCursor {
    value_start: Option<usize>,
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
    if cursor.value_start.is_none() {
        cursor.value_start = find_string_field_value_start(buffer, field_name);
    }
    let val_start = match cursor.value_start {
        Some(idx) => idx,
        None => return (None, false),
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
}
