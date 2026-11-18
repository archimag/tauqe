//! Shared review stream transitions, independent of response schema and RPC.

use crate::edits::stream::json::JsonFieldStreamFilter;
use crate::providers::{ModelUsageInfo, StreamEvent};
use serde::de::DeserializeOwned;

pub enum ReviewStreamUpdate {
    Reasoning(String),
    Content(String),
    Usage(ModelUsageInfo),
    Reset,
    Finished,
    Ignored,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewStreamError {
    #[error("review stream cancelled")]
    Cancelled,
    #[error("{0}")]
    Provider(String),
}

pub(crate) struct ReviewStream<T> {
    filter: JsonFieldStreamFilter<T>,
    failure: Option<String>,
    cancelled: bool,
}

impl<T: DeserializeOwned> Default for ReviewStream<T> {
    fn default() -> Self {
        Self {
            filter: JsonFieldStreamFilter::new("summary"),
            failure: None,
            cancelled: false,
        }
    }
}

impl<T: DeserializeOwned> ReviewStream<T> {
    /// The caller paces the raw event before any state transition or publication.
    pub(crate) fn observe(&mut self, event: StreamEvent) -> ReviewStreamUpdate {
        match event {
            StreamEvent::ReasoningDelta(delta) => ReviewStreamUpdate::Reasoning(delta),
            StreamEvent::TextDelta(chunk) => self
                .filter
                .push_chunk(&chunk)
                .map(ReviewStreamUpdate::Content)
                .unwrap_or(ReviewStreamUpdate::Ignored),
            StreamEvent::Usage(usage) => ReviewStreamUpdate::Usage(usage),
            StreamEvent::Done => ReviewStreamUpdate::Finished,
            StreamEvent::Cancelled => {
                self.cancelled = true;
                ReviewStreamUpdate::Finished
            }
            StreamEvent::Error(error) => {
                self.failure = Some(error);
                ReviewStreamUpdate::Finished
            }
            StreamEvent::ResetStream => {
                self.filter = JsonFieldStreamFilter::new("summary");
                ReviewStreamUpdate::Reset
            }
            _ => ReviewStreamUpdate::Ignored,
        }
    }

    /// After accounting, cancellation wins over provider errors and decoding.
    pub(crate) fn finish(
        self,
        cancelled: bool,
    ) -> Result<JsonFieldStreamFilter<T>, ReviewStreamError> {
        if cancelled || self.cancelled {
            return Err(ReviewStreamError::Cancelled);
        }
        if let Some(error) = self.failure {
            return Err(ReviewStreamError::Provider(error));
        }
        Ok(self.filter)
    }
}
