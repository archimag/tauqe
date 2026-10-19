//! Generic single-field streaming filter for JSON Structured Output payloads.
//!
//! It accumulates raw provider chunks, decodes the requested string field
//! (`message`, `summary`, ...) on the fly without leaking JSON syntax to the
//! user, and, once streaming ends, deserializes the full buffered payload into a
//! strictly typed structure `T`. The byte-level scanners are owned by
//! [`super::decode`], so escape decoding, UTF-16 surrogate pairing and code
//! fence tolerance stay identical to the edit streaming contour.

use std::marker::PhantomData;

use serde::Deserialize;

use super::decode::{
    decode_json_string_body_incremental, find_json_string_end_from, find_string_field_value_start,
    prepare_json_payload,
};

/// Generic streaming filter for a single top-level string field of a JSON
/// Structured Output payload.
///
/// The filter accumulates raw provider chunks, decodes the requested string
/// field (`message`, `summary`, ...) on the fly without leaking JSON syntax to
/// the user, and, once streaming ends, deserializes the full buffered payload
/// into a strictly typed structure `T`. Reusing
/// [`decode::extract_streamed_string_field`] keeps escape decoding, UTF-16
/// surrogate pairing and code fence tolerance identical to the edit streaming
/// contour.
pub struct JsonFieldStreamFilter<T> {
    target_field: String,
    buffer: String,
    value_start: Option<usize>,
    raw_decoded_pos: usize,
    end_scanned_pos: usize,
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
            value_start: None,
            raw_decoded_pos: 0,
            end_scanned_pos: 0,
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
        if self.value_start.is_none() {
            self.value_start = find_string_field_value_start(&self.buffer, &self.target_field);
        }
        let val_start = self.value_start?;
        let content_slice = &self.buffer[val_start..];
        let (raw_body, is_finished) =
            match find_json_string_end_from(content_slice, &mut self.end_scanned_pos) {
                Some(end) => (&content_slice[..end], true),
                None => (content_slice, false),
            };

        let delta = decode_json_string_body_incremental(raw_body, &mut self.raw_decoded_pos);
        if is_finished && self.raw_decoded_pos >= raw_body.len() {
            self.finished = true;
        }
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
