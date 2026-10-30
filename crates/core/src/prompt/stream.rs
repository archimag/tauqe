//! Streaming filter that splits model output into UI events and buffered text.
//!
//! [`CommandStreamFilter`] decorates a protocol-specific [`EditStreamFilter`].
//! It forwards every delta to the inner filter so lightweight streaming events
//! reach the user interface immediately, while accumulating the raw model
//! output in a buffer. The accumulated text is later decoded into normalized
//! commands by the core, keeping command parsing decoupled from real-time
//! rendering.

use crate::edits::stream::EditStreamFilter;
use crate::providers::StreamEvent;

/// Decorator over an [`EditStreamFilter`] that buffers raw output alongside
/// streaming UI events.
///
/// Rationale: the user interface must render deltas without waiting for the
/// complete response, yet deterministic command decoding needs the full raw
/// text. Separating these concerns prevents real-time rendering from blocking
/// on parsing and keeps the rendering path free of protocol business logic.
pub struct CommandStreamFilter {
    inner: Box<dyn EditStreamFilter>,
    raw: String,
}

impl CommandStreamFilter {
    /// Wraps the inner filter responsible for producing streaming UI events.
    pub fn new(inner: Box<dyn EditStreamFilter>) -> Self {
        Self {
            inner,
            raw: String::new(),
        }
    }

    /// Returns the raw model output accumulated so far.
    pub fn raw_text(&self) -> &str {
        &self.raw
    }

    /// Flushes the inner filter and returns its final events together with the
    /// accumulated raw output intended for command decoding.
    pub fn finish_with_raw(self) -> (Vec<StreamEvent>, String) {
        let CommandStreamFilter { inner, raw } = self;
        (inner.finish(), raw)
    }
}

impl EditStreamFilter for CommandStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.raw.push_str(chunk);
        self.inner.push_chunk(chunk)
    }

    fn finish(self: Box<Self>) -> Vec<StreamEvent> {
        let CommandStreamFilter { inner, .. } = *self;
        inner.finish()
    }

    fn reset(&mut self) {
        // The recorded output is the text decoded once the turn ends, so the
        // voided attempt must leave neither a fragment there nor in the filter
        // that renders the preview.
        self.raw.clear();
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal inner filter that echoes each chunk as a `TextDelta` and reports
    /// completion with `Done`.
    struct EchoFilter;

    impl EditStreamFilter for EchoFilter {
        fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
            vec![StreamEvent::TextDelta(chunk.to_string())]
        }

        fn finish(self: Box<Self>) -> Vec<StreamEvent> {
            vec![StreamEvent::Done]
        }

        fn reset(&mut self) {}
    }

    #[test]
    fn test_forwards_events_and_accumulates_raw_text() {
        let mut filter = CommandStreamFilter::new(Box::new(EchoFilter));

        let first = filter.push_chunk("Hello ");
        assert!(matches!(&first[..], [StreamEvent::TextDelta(t)] if t == "Hello "));

        let second = filter.push_chunk("<edit path=\"a.rs\">");
        assert!(matches!(&second[..], [StreamEvent::TextDelta(t)] if t == "<edit path=\"a.rs\">"));

        assert_eq!(filter.raw_text(), "Hello <edit path=\"a.rs\">");

        let (events, raw) = Box::new(filter).finish_with_raw();
        assert!(matches!(&events[..], [StreamEvent::Done]));
        assert_eq!(raw, "Hello <edit path=\"a.rs\">");
    }

    #[test]
    fn test_finish_delegates_to_inner_filter() {
        let mut filter = CommandStreamFilter::new(Box::new(EchoFilter));
        filter.push_chunk("delta");
        let events = Box::new(filter).finish();
        assert!(matches!(&events[..], [StreamEvent::Done]));
    }
    #[test]
    fn test_reset_discards_the_recorded_output() {
        let mut filter = CommandStreamFilter::new(Box::new(EchoFilter));
        filter.push_chunk("first attempt");

        filter.reset();
        assert_eq!(filter.raw_text(), "");

        let events = filter.push_chunk("second attempt");
        assert!(matches!(&events[..], [StreamEvent::TextDelta(t)] if t == "second attempt"));
        assert_eq!(filter.raw_text(), "second attempt");
    }
}
