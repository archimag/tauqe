//! Completion classification for a single-audit review stream.

use crate::providers::StreamEvent;
use crate::review::parser::ReviewFindingsOutput;
use crate::review::stream::{ReviewStream, ReviewStreamError, ReviewStreamUpdate};
use crate::review::synthesis::{
    EmptyReviewResponse, ReviewSynthesisMetadata, ReviewSynthesisResponse,
};
use crate::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisCompletionError {
    #[error("review synthesis cancelled")]
    Cancelled,
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    EmptyResponse(#[from] EmptyReviewResponse),
}

impl ReviewSynthesisCompletionError {
    pub fn requires_abort(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Provider(_))
    }
}

#[derive(Default)]
pub struct ReviewSynthesisStream {
    stream: ReviewStream<ReviewFindingsOutput>,
}

impl ReviewSynthesisStream {
    pub fn observe(&mut self, event: StreamEvent) -> ReviewStreamUpdate {
        self.stream.observe(event)
    }

    /// Called after usage publication. Empty output retains its detach policy.
    pub fn finish(
        self,
        metadata: ReviewSynthesisMetadata,
        cancelled: bool,
    ) -> Result<ReviewSession, ReviewSynthesisCompletionError> {
        let filter = self.stream.finish(cancelled).map_err(|error| match error {
            ReviewStreamError::Cancelled => ReviewSynthesisCompletionError::Cancelled,
            ReviewStreamError::Provider(message) => {
                ReviewSynthesisCompletionError::Provider(message)
            }
        })?;
        ReviewSynthesisResponse::from_filter(filter)
            .finish(metadata)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelRef;
    use crate::providers::ModelUsageInfo;

    fn metadata() -> ReviewSynthesisMetadata {
        ReviewSynthesisMetadata {
            id: "review-stream".into(),
            created_at: 42,
            model: ModelRef::openrouter("test/reviewer"),
            user_prompt: None,
            target_files: vec!["audit.rs".into()],
        }
    }

    #[test]
    fn retry_discards_partial_json_and_keeps_each_usage_report_visible() {
        let mut response = ReviewSynthesisStream::default();
        response.observe(StreamEvent::TextDelta("{\"summary\":\"abandoned".into()));
        let mut tokens = 0;
        if let ReviewStreamUpdate::Usage(usage) =
            response.observe(StreamEvent::Usage(ModelUsageInfo {
                total_tokens: 11,
                ..Default::default()
            }))
        {
            tokens += usage.total_tokens;
        }
        assert!(matches!(
            response.observe(StreamEvent::ResetStream),
            ReviewStreamUpdate::Reset
        ));
        let raw = serde_json::json!({"summary": "Fresh verdict", "title": "Audit", "findings": []})
            .to_string();
        assert!(
            matches!(response.observe(StreamEvent::TextDelta(raw.clone())), ReviewStreamUpdate::Content(text) if text == "Fresh verdict")
        );
        if let ReviewStreamUpdate::Usage(usage) =
            response.observe(StreamEvent::Usage(ModelUsageInfo {
                total_tokens: 7,
                ..Default::default()
            }))
        {
            tokens += usage.total_tokens;
        }
        assert!(matches!(
            response.observe(StreamEvent::Done),
            ReviewStreamUpdate::Finished
        ));
        let review = response.finish(metadata(), false).unwrap();
        assert_eq!(review.raw_markdown, raw);
        assert_eq!(review.description.as_deref(), Some("Fresh verdict"));
        assert_eq!(tokens, 18);
    }

    #[test]
    fn cancellation_precedes_provider_errors_and_empty_response_validation() {
        let mut response = ReviewSynthesisStream::default();
        assert!(matches!(
            response.observe(StreamEvent::Error("provider failed".into())),
            ReviewStreamUpdate::Finished
        ));
        let error = response.finish(metadata(), true).unwrap_err();
        assert!(matches!(error, ReviewSynthesisCompletionError::Cancelled));
        assert!(error.requires_abort());
        let mut response = ReviewSynthesisStream::default();
        response.observe(StreamEvent::Error("provider failed".into()));
        let error = response.finish(metadata(), false).unwrap_err();
        assert!(
            matches!(&error, ReviewSynthesisCompletionError::Provider(message) if message == "provider failed")
        );
        assert!(error.requires_abort());
        let mut response = ReviewSynthesisStream::default();
        assert!(matches!(
            response.observe(StreamEvent::Cancelled),
            ReviewStreamUpdate::Finished
        ));
        assert!(matches!(
            response.finish(metadata(), false),
            Err(ReviewSynthesisCompletionError::Cancelled)
        ));
    }

    #[test]
    fn empty_output_keeps_detach_policy_and_nonempty_unstructured_output_is_retained() {
        let error = ReviewSynthesisStream::default()
            .finish(metadata(), false)
            .unwrap_err();
        assert!(matches!(
            error,
            ReviewSynthesisCompletionError::EmptyResponse(_)
        ));
        assert_eq!(error.to_string(), "Model returned an empty review");
        assert!(!error.requires_abort());
        let mut response = ReviewSynthesisStream::default();
        assert!(
            matches!(response.observe(StreamEvent::ReasoningDelta("Check boundaries".into())), ReviewStreamUpdate::Reasoning(text) if text == "Check boundaries")
        );
        assert!(matches!(
            response.observe(StreamEvent::RepoMapTokens(10)),
            ReviewStreamUpdate::Ignored
        ));
        response.observe(StreamEvent::TextDelta(
            "Unstructured review remains visible".into(),
        ));
        // Channel closure without a Done event retains the existing decoding policy.
        let review = response.finish(metadata(), false).unwrap();
        assert_eq!(review.items.len(), 1);
        assert!(review.items[0]
            .body
            .contains("Unstructured review remains visible"));
    }
}
