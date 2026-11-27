//! Completion classification for a typed single-audit result.

use crate::workflows::structured::{StructuredFailure, StructuredTaskError};
use tauqe_core::execution::budget::InputLimitExceeded;
use tauqe_core::review::parser::SingleReviewOutput;
use tauqe_core::review::synthesis::{
    materialize_review, EmptyReviewResponse, ReviewSynthesisMetadata,
};
use tauqe_core::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisCompletionError {
    #[error("review synthesis cancelled")]
    Cancelled,
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    InputLimit(#[from] InputLimitExceeded),
    #[error(transparent)]
    EmptyResponse(#[from] EmptyReviewResponse),
}

pub struct ReviewSynthesisStream;

impl ReviewSynthesisStream {
    /// Interpret the supervised request only after its usage has been published.
    pub fn finish(
        result: Result<SingleReviewOutput, StructuredTaskError>,
        metadata: ReviewSynthesisMetadata,
        cancelled: bool,
    ) -> Result<ReviewSession, ReviewSynthesisCompletionError> {
        if cancelled {
            return Err(ReviewSynthesisCompletionError::Cancelled);
        }
        let output = match result {
            Ok(output) => output,
            Err(error) => match error.classify("review synthesis") {
                StructuredFailure::Cancelled => {
                    return Err(ReviewSynthesisCompletionError::Cancelled);
                }
                StructuredFailure::Empty => {
                    return Err(EmptyReviewResponse.into());
                }
                StructuredFailure::InputLimit(error) => return Err(error.into()),
                StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => {
                    return Err(ReviewSynthesisCompletionError::Provider(message));
                }
            },
        };
        Ok(materialize_review(metadata, output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::execution::structured::StructuredError;
    use tauqe_core::models::ModelRef;

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
    fn typed_output_preserves_provider_text_and_review_identity() {
        let raw = serde_json::json!({"summary": "Fresh verdict", "title": "Audit", "findings": []})
            .to_string();
        let output = tauqe_core::review::parser::decode_single_review(&raw).unwrap();
        let review = ReviewSynthesisStream::finish(Ok(output), metadata(), false).unwrap();
        assert_eq!(review.raw_markdown, raw);
        assert_eq!(review.description.as_deref(), Some("Fresh verdict"));
        assert_eq!(review.id, "review-stream");
    }

    #[test]
    fn cancellation_precedes_provider_errors_and_empty_response_validation() {
        let error = ReviewSynthesisStream::finish(
            Err(StructuredTaskError::Stream("provider failed".into())),
            metadata(),
            true,
        )
        .unwrap_err();
        assert!(matches!(error, ReviewSynthesisCompletionError::Cancelled));
        let error = ReviewSynthesisStream::finish(
            Err(StructuredTaskError::Stream("provider failed".into())),
            metadata(),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(&error, ReviewSynthesisCompletionError::Provider(message) if message == "provider failed")
        );
        assert!(matches!(
            ReviewSynthesisStream::finish(Err(StructuredTaskError::Cancelled), metadata(), false),
            Err(ReviewSynthesisCompletionError::Cancelled)
        ));
    }

    #[test]
    fn input_limit_remains_typed_after_completion() {
        let limit = InputLimitExceeded {
            size: 12,
            limit: 1,
            messages: 5,
            response_format: 4,
            adapter_and_framing: 3,
        };
        let result = ReviewSynthesisStream::finish(
            Err(StructuredTaskError::Request(StructuredError::InputLimit(
                limit,
            ))),
            metadata(),
            false,
        );
        assert!(
            matches!(result, Err(ReviewSynthesisCompletionError::InputLimit(error)) if error.size == 12 && error.limit == 1)
        );
    }

    #[test]
    fn empty_output_is_refused_and_nonempty_unstructured_output_is_retained() {
        let error = ReviewSynthesisStream::finish(
            Err(StructuredTaskError::Request(StructuredError::Empty)),
            metadata(),
            false,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ReviewSynthesisCompletionError::EmptyResponse(_)
        ));
        assert_eq!(error.to_string(), "Model returned an empty review");
        let output =
            tauqe_core::review::parser::decode_single_review("Unstructured review remains visible")
                .unwrap();
        let review = ReviewSynthesisStream::finish(Ok(output), metadata(), false).unwrap();
        assert_eq!(review.items.len(), 1);
        assert!(review.items[0]
            .body
            .contains("Unstructured review remains visible"));
    }
}
