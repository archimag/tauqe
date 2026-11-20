//! Addressed text preview for structured discussion responses.
//!
//! The final response is decoded by the prompt protocol, not by this filter.

use super::json::events::{key_path, PreviewEvents};
use super::EditStreamFilter;
use crate::providers::StreamEvent;

/// Streams only the selected root-level string field of a discussion payload.
pub struct DiscussionStreamFilter {
    preview: PreviewEvents,
}

impl Default for DiscussionStreamFilter {
    fn default() -> Self {
        Self::with_target_field("message")
    }
}

impl DiscussionStreamFilter {
    /// Selects one root-level conversational field for live preview.
    pub(crate) fn with_target_field(target_field: impl Into<String>) -> Self {
        let target_field = target_field.into();
        Self {
            preview: PreviewEvents::new(key_path(&[&target_field])),
        }
    }
}

impl EditStreamFilter for DiscussionStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.preview
            .push(chunk.as_bytes())
            .into_iter()
            .map(StreamEvent::TextDelta)
            .collect()
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        self.preview.finish();
        Vec::new()
    }

    fn reset(&mut self) {
        self.preview.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(filter: &mut DiscussionStreamFilter, input: &str) -> String {
        input
            .chars()
            .flat_map(|ch| filter.push_chunk(ch.encode_utf8(&mut [0; 4])))
            .filter_map(|event| match event {
                StreamEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn only_the_root_message_is_previewed() {
        let mut filter = DiscussionStreamFilter::default();
        let input = r#"{"metadata":{"message":"hidden"},"action":{"message":"hidden too","changes":[{"content":"message"}]},"message":"Live \uD83D\uDE00!"}"#;
        assert_eq!(preview(&mut filter, input), "Live 😀!");
        assert!(Box::new(filter).finish().is_empty());
    }

    #[test]
    fn incomplete_text_is_live_without_repeats_and_reset_isolates_attempts() {
        let mut filter = DiscussionStreamFilter::default();
        assert_eq!(preview(&mut filter, "{\"message\":\"First "), "First ");
        filter.reset();
        assert_eq!(preview(&mut filter, "{\"message\":\"Second"), "Second");
        assert!(Box::new(filter).finish().is_empty());
    }

    #[test]
    fn invalid_or_wrapped_input_never_recovers_a_preview() {
        for input in [
            "```json\n{\"message\":\"wrapped\"}\n```",
            "Prose {\"message\":\"wrapped\"}",
            "{\"message\":\"before\",!\"message\":\"after\"}",
        ] {
            let mut filter = DiscussionStreamFilter::default();
            let text = preview(&mut filter, input);
            assert!(
                !text.contains("wrapped") && !text.contains("after"),
                "{text}"
            );
            assert!(Box::new(filter).finish().is_empty());
        }
    }

    #[test]
    fn explanation_only_at_the_root_is_previewed() {
        let mut filter = DiscussionStreamFilter::with_target_field("explanation");
        assert_eq!(
            preview(
                &mut filter,
                r#"{"items":[{"explanation":"hidden"}],"explanation":"Horizon"}"#
            ),
            "Horizon"
        );
    }
}
