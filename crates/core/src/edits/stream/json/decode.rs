//! Helpers for final decoding of buffered structured responses.
//!
//! Live previews use json_skimmer instead. Balanced-object scanning here is
//! retained for final decoders that accept compatible response wrappers.

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

pub(crate) fn trim_response_start(raw: &str) -> &str {
    raw.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// Detects a response that starts at a separator between JSON fields.
pub(crate) fn starts_with_detached_json_field(raw: &str) -> bool {
    let raw = trim_response_start(raw);
    raw.strip_prefix('"')
        .is_some_and(|rest| rest.trim_start().starts_with(','))
        || raw
            .strip_prefix(',')
            .is_some_and(|rest| rest.trim_start().starts_with('"'))
}

/// Recognizes the beginning of a structured response, optionally fenced.
pub(crate) fn starts_with_json_object(raw: &str) -> bool {
    let raw = trim_response_start(raw);
    let raw = match raw.strip_prefix("```") {
        Some(fence) => fence.strip_prefix("json").unwrap_or(fence).trim_start(),
        None => raw,
    };
    raw.starts_with('{')
}

/// Trims `buffer` to the outermost balanced JSON object so a strictly typed payload
/// can be deserialized.
pub(crate) fn prepare_json_payload(buffer: &str) -> String {
    let trimmed = buffer.trim();
    match trimmed.find('{') {
        Some(start) => {
            let candidate = &trimmed[start..];
            match balanced_object_end(candidate, 0) {
                Some(end) => candidate[..end].to_string(),
                None => candidate.to_string(),
            }
        }
        None => trimmed.to_string(),
    }
}
