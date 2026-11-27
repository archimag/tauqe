//! Stream decoding and completion classification for review post-analysis.

use crate::workflows::structured::{StructuredFailure, StructuredTaskError};
use tauqe_core::execution::budget::InputLimitExceeded;
use tauqe_core::review::analysis::{
    PostAnalysisOutput, PostAnalysisRegrouping, PostAnalysisRegroupingError,
};
use tauqe_core::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisCompletionError {
    #[error("Review post-analysis of session '{0}' was cancelled")]
    Cancelled(String),
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    InputLimit(#[from] InputLimitExceeded),
    #[error("Model returned an empty post-analysis for review session '{0}'")]
    EmptyResponse(String),
    #[error("Failed to parse the review post-analysis payload: {0}")]
    Parse(String),
    #[error(transparent)]
    Schema(#[from] PostAnalysisRegroupingError),
}

pub(super) struct ReviewAnalysisResponse;

impl ReviewAnalysisResponse {
    /// Classify after usage is flushed. Cancellation precedes provider errors,
    /// which precede decoding and schema validation, preserving failure priority.
    pub(super) fn finish(
        result: Result<PostAnalysisOutput, StructuredTaskError>,
        session: &ReviewSession,
        cancelled: bool,
    ) -> Result<PostAnalysisRegrouping, ReviewAnalysisCompletionError> {
        if cancelled {
            return Err(ReviewAnalysisCompletionError::Cancelled(session.id.clone()));
        }
        let output = match result {
            Ok(output) => output,
            Err(error) => match error.classify("review post-analysis") {
                StructuredFailure::Cancelled => {
                    return Err(ReviewAnalysisCompletionError::Cancelled(session.id.clone()))
                }
                StructuredFailure::Empty => {
                    return Err(ReviewAnalysisCompletionError::EmptyResponse(
                        session.id.clone(),
                    ))
                }
                StructuredFailure::Decode(message) => {
                    return Err(ReviewAnalysisCompletionError::Parse(message))
                }
                StructuredFailure::InputLimit(error) => return Err(error.into()),
                StructuredFailure::Failed(message) => {
                    return Err(ReviewAnalysisCompletionError::Provider(message))
                }
            },
        };
        PostAnalysisRegrouping::prepare(session, &output).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::execution::structured::StructuredError;

    fn decode(raw: &str) -> PostAnalysisOutput {
        tauqe_core::review::analysis::parse_post_analysis(raw).unwrap()
    }

    fn session() -> ReviewSession {
        ReviewSession {
            id: "decoded-review".into(),
            ..Default::default()
        }
    }

    fn payload(summary: &str) -> String {
        serde_json::json!({
            "summary": summary, "triage": [], "independent_issues": [],
            "architectural_groups": [],
        })
        .to_string()
    }

    #[test]
    fn cancellation_and_provider_errors_precede_decoding_failures() {
        let error = ReviewAnalysisResponse::finish(
            Err(StructuredTaskError::Stream("provider failed".into())),
            &session(),
            true,
        )
        .err()
        .unwrap();
        assert!(matches!(error, ReviewAnalysisCompletionError::Cancelled(_)));
        assert!(
            matches!(ReviewAnalysisResponse::finish(Err(StructuredTaskError::Stream("provider failed".into())), &session(), false), Err(ReviewAnalysisCompletionError::Provider(message)) if message == "provider failed")
        );
        assert!(matches!(
            ReviewAnalysisResponse::finish(Err(StructuredTaskError::Cancelled), &session(), false),
            Err(ReviewAnalysisCompletionError::Cancelled(_))
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
        let result = ReviewAnalysisResponse::finish(
            Err(StructuredTaskError::Request(StructuredError::InputLimit(
                limit,
            ))),
            &session(),
            false,
        );
        assert!(
            matches!(result, Err(ReviewAnalysisCompletionError::InputLimit(error)) if error.size == 12 && error.limit == 1)
        );
    }

    #[test]
    fn decoding_keeps_tolerant_fallback_and_typed_parse_and_schema_refusals() {
        assert!(ReviewAnalysisResponse::finish(
            Ok(decode(&format!(
                "Wrapper {{\"summary\": 42}} then {}",
                payload("Recovered verdict")
            ))),
            &session(),
            false
        )
        .unwrap()
        .history_response()
        .ends_with("Recovered verdict"));
        assert!(matches!(
            ReviewAnalysisResponse::finish(
                Err(StructuredTaskError::Request(StructuredError::Empty)),
                &session(),
                false
            ),
            Err(ReviewAnalysisCompletionError::EmptyResponse(_))
        ));
        let error = ReviewAnalysisResponse::finish(
            Err(StructuredTaskError::Request(StructuredError::Decode(
                "not JSON".into(),
            ))),
            &session(),
            false,
        )
        .err()
        .unwrap();
        assert!(matches!(error, ReviewAnalysisCompletionError::Parse(_)));
        assert!(matches!(
            ReviewAnalysisResponse::finish(Ok(decode(&payload(" "))), &session(), false),
            Err(ReviewAnalysisCompletionError::Schema(
                PostAnalysisRegroupingError::MissingSummary
            ))
        ));
        assert!(matches!(
            ReviewAnalysisResponse::finish(
                Ok(decode(
                    &serde_json::json!({
                        "summary": "Foreign reference", "triage": [],
                        "independent_issues": ["foreign"], "architectural_groups": [],
                    })
                    .to_string()
                )),
                &session(),
                false
            ),
            Err(ReviewAnalysisCompletionError::Schema(
                PostAnalysisRegroupingError::InvalidReference(_)
            ))
        ));
    }
}
