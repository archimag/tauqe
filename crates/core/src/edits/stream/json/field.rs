//! Generic single-field streaming filter for JSON Structured Output payloads.
//!
//! json_skimmer routes only the requested string field (`message`, `summary`,
//! ...) for live preview. The original chunks are accumulated independently
//! for final serde decoding, which retains compatible wrapper handling.

use std::marker::PhantomData;

use serde::Deserialize;

use super::decode::prepare_json_payload;
use super::events::{key_path, PreviewEvents};

/// Generic streaming filter for a single top-level string field of a JSON
/// Structured Output payload.
///
/// json_skimmer selects string fragments at the exact root path without
/// extracting JSON from prose or markdown wrappers for live preview. The
/// buffered original response belongs to the independent final decoder;
/// previewed fragments do not establish that the document is valid.
pub struct JsonFieldStreamFilter<T> {
    buffer: String,
    preview: PreviewEvents,
    _marker: PhantomData<fn() -> T>,
}

impl<T> JsonFieldStreamFilter<T>
where
    T: for<'de> Deserialize<'de>,
{
    /// Creates a filter that streams the top-level string field `target_field`.
    pub fn new(target_field: impl Into<String>) -> Self {
        let target_field = target_field.into();
        Self {
            buffer: String::new(),
            preview: PreviewEvents::new(key_path(&[&target_field])),
            _marker: PhantomData,
        }
    }

    /// Appends a raw chunk and returns the freshly decoded text of the target
    /// field, or `None` when no new readable text is available.
    ///
    /// The raw buffer keeps accumulating independently so [`finish`](Self::finish)
    /// can deserialize the complete payload even when preview is invalid.
    pub fn push_chunk(&mut self, chunk: &str) -> Option<String> {
        self.buffer.push_str(chunk);
        let text = self.preview.push(chunk.as_bytes()).concat();
        (!text.is_empty()).then_some(text)
    }

    /// Returns the accumulated raw JSON buffer received so far.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Returns the accumulated raw JSON buffer received so far, including any
    /// surrounding markdown code fence.
    #[cfg(test)]
    fn raw_buffer(&self) -> &str {
        &self.buffer
    }

    /// Deserializes the accumulated payload into `T`.
    ///
    /// The buffer is first unwrapped from any markdown code fence and trimmed
    /// to the outermost balanced JSON object, so wrapper prose around the
    /// payload does not break decoding.
    pub fn finish(self) -> Result<T, serde_json::Error> {
        let mut preview = self.preview;
        preview.finish();
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
        assert_eq!(streamed, "");
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
    #[test]
    fn json_field_stream_filter_only_previews_the_root_field() {
        let json = concat!(
            "{\"action\":{\"kind\":\"edit\",\"changes\":[{\"op\":\"create\",",
            "\"path\":\"a.rs\",\"content\":\"let message = \\\"message\\\";\\n\"}],",
            "\"message\":\"Branch answer\"},\"metadata\":{\"message\":\"Hidden\"},",
            "\"message\":\"Root answer\",\"count\":7}"
        );
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let mut streamed = String::new();
        for c in json.chars() {
            if let Some(delta) = filter.push_chunk(&c.to_string()) {
                streamed.push_str(&delta);
            }
        }
        assert_eq!(streamed, "Root answer");
        assert!(!streamed.contains("let message"), "{streamed}");
        assert_eq!(filter.raw_buffer(), json);
    }

    #[test]
    fn json_field_stream_filter_invalid_and_unfinished_attempts() {
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        assert_eq!(
            filter.push_chunk("{\"message\":\"Hi \\uD83D"),
            Some("Hi ".into())
        );
        assert_eq!(filter.push_chunk("\\uDE00"), Some("😀".into()));
        assert_eq!(filter.push_chunk("!"), Some("!".into()));
        assert_eq!(filter.push_chunk("\",\"count\":1}"), None);
        assert_eq!(filter.finish().unwrap().message, "Hi 😀!");

        let mut invalid = JsonFieldStreamFilter::<FieldSample>::new("message");
        assert_eq!(
            invalid.push_chunk("{\"message\":\"before\",!"),
            Some("before".into())
        );
        assert_eq!(invalid.push_chunk("\"message\":\"after\"}"), None);
    }

    #[test]
    fn json_field_stream_filter_does_not_preview_wrapped_json() {
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        assert_eq!(
            filter.push_chunk("```json\n{\"message\":\"wrapped\",\"count\":2}\n```"),
            None
        );
        assert_eq!(filter.finish().unwrap().message, "wrapped");
    }

    #[test]
    fn prose_wrapper_is_final_decoder_only() {
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        let wrapped = "Here is the result: {\"message\":\"accepted\",\"count\":3}";
        for chunk in wrapped.as_bytes().chunks(4) {
            assert_eq!(filter.push_chunk(std::str::from_utf8(chunk).unwrap()), None);
        }
        let result = filter.finish().expect("final decoder accepts the wrapper");
        assert_eq!(result.message, "accepted");
        assert_eq!(result.count, 3);
    }

    #[test]
    fn previewed_fragment_does_not_accept_invalid_full_payload() {
        let mut filter = JsonFieldStreamFilter::<FieldSample>::new("message");
        assert_eq!(
            filter.push_chunk("{\"message\":\"visible\""),
            Some("visible".into())
        );
        assert_eq!(filter.push_chunk(",\"count\":\"wrong\"}"), None);
        assert!(filter.finish().is_err());
    }
}
