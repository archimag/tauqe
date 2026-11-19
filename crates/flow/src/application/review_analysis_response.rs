//! Stream decoding and completion classification for review post-analysis.

use tauqe_core::providers::StreamEvent;
use tauqe_core::review::analysis::{
    parse_post_analysis, PostAnalysisOutput, PostAnalysisRegrouping, PostAnalysisRegroupingError,
};
use tauqe_core::review::stream::{ReviewStream, ReviewStreamError};
use tauqe_core::review::ReviewSession;

pub use tauqe_core::review::stream::ReviewStreamUpdate as ReviewAnalysisUpdate;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisCompletionError {
    #[error("Review post-analysis of session '{0}' was cancelled")]
    Cancelled(String),
    #[error("{0}")]
    Provider(String),
    #[error("Model returned an empty post-analysis for review session '{0}'")]
    EmptyResponse(String),
    #[error("Failed to parse the review post-analysis payload: {0}")]
    Parse(String),
    #[error(transparent)]
    Schema(#[from] PostAnalysisRegroupingError),
}

impl ReviewAnalysisCompletionError {
    /// Only cancellation and provider failures terminate the provider task.
    pub(crate) fn requires_abort(&self) -> bool {
        matches!(self, Self::Cancelled(_) | Self::Provider(_))
    }
}

#[derive(Default)]
pub struct ReviewAnalysisResponse {
    stream: ReviewStream<PostAnalysisOutput>,
}

impl ReviewAnalysisResponse {
    pub fn observe(&mut self, event: StreamEvent) -> ReviewAnalysisUpdate {
        self.stream.observe(event)
    }

    /// Classify after usage is flushed. Cancellation precedes provider errors,
    /// which precede decoding and schema validation, preserving failure priority.
    pub fn finish(
        self,
        session: &ReviewSession,
        cancelled: bool,
    ) -> Result<PostAnalysisRegrouping, ReviewAnalysisCompletionError> {
        let filter = self.stream.finish(cancelled).map_err(|error| match error {
            ReviewStreamError::Cancelled => {
                ReviewAnalysisCompletionError::Cancelled(session.id.clone())
            }
            ReviewStreamError::Provider(message) => {
                ReviewAnalysisCompletionError::Provider(message)
            }
        })?;
        let raw = filter.buffer().to_string();
        if raw.trim().is_empty() {
            return Err(ReviewAnalysisCompletionError::EmptyResponse(
                session.id.clone(),
            ));
        }
        let output = match filter.finish() {
            Ok(output) => output,
            Err(_) => parse_post_analysis(&raw)
                .map_err(|error| ReviewAnalysisCompletionError::Parse(format!("{error:#}")))?,
        };
        PostAnalysisRegrouping::prepare(session, &output).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::providers::ModelUsageInfo;

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
    fn streams_only_decoded_summary_and_accepts_channel_closure_without_done() {
        let mut response = ReviewAnalysisResponse::default();
        let mut text = String::new();
        for ch in payload("Boundary\n\"quoted\"").chars() {
            if let ReviewAnalysisUpdate::Content(delta) =
                response.observe(StreamEvent::TextDelta(ch.to_string()))
            {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "Boundary\n\"quoted\"");
        let prepared = response.finish(&session(), false).unwrap();
        assert!(prepared.history_response().ends_with(&text));
    }

    #[test]
    fn retry_resets_partial_json_but_keeps_usage_visible() {
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::TextDelta(
            "{\"summary\":\"discarded prefix".into(),
        ));
        let usage = ModelUsageInfo {
            total_tokens: 17,
            cost: Some(0.25),
            ..Default::default()
        };
        assert!(
            matches!(response.observe(StreamEvent::Usage(usage)), ReviewAnalysisUpdate::Usage(report) if report.total_tokens == 17)
        );
        assert!(matches!(
            response.observe(StreamEvent::ResetStream),
            ReviewAnalysisUpdate::Reset
        ));
        assert!(
            matches!(response.observe(StreamEvent::TextDelta(payload("Fresh verdict"))), ReviewAnalysisUpdate::Content(delta) if delta == "Fresh verdict")
        );
        assert!(matches!(
            response.observe(StreamEvent::Done),
            ReviewAnalysisUpdate::Finished
        ));
        let prepared = response.finish(&session(), false).unwrap();
        assert!(prepared.history_response().ends_with("Fresh verdict"));
        assert!(!prepared.history_response().contains("discarded"));
    }

    #[test]
    fn cancellation_and_provider_errors_precede_decoding_failures() {
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::Error("provider failed".into()));
        let error = response.finish(&session(), true).err().unwrap();
        assert!(matches!(error, ReviewAnalysisCompletionError::Cancelled(_)));
        assert!(error.requires_abort());
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::Error("provider failed".into()));
        assert!(
            matches!(response.finish(&session(), false), Err(ReviewAnalysisCompletionError::Provider(message)) if message == "provider failed")
        );
        let mut response = ReviewAnalysisResponse::default();
        assert!(matches!(
            response.observe(StreamEvent::Cancelled),
            ReviewAnalysisUpdate::Finished
        ));
        assert!(matches!(
            response.finish(&session(), false),
            Err(ReviewAnalysisCompletionError::Cancelled(_))
        ));
    }

    #[test]
    fn decoding_keeps_tolerant_fallback_and_typed_parse_and_schema_refusals() {
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::TextDelta(format!(
            "Wrapper {{\"summary\": 42}} then {}",
            payload("Recovered verdict")
        )));
        assert!(response
            .finish(&session(), false)
            .unwrap()
            .history_response()
            .ends_with("Recovered verdict"));
        assert!(matches!(
            ReviewAnalysisResponse::default().finish(&session(), false),
            Err(ReviewAnalysisCompletionError::EmptyResponse(_))
        ));
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::TextDelta("not JSON".into()));
        let error = response.finish(&session(), false).err().unwrap();
        assert!(matches!(error, ReviewAnalysisCompletionError::Parse(_)));
        assert!(!error.requires_abort());
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::TextDelta(payload(" ")));
        assert!(matches!(
            response.finish(&session(), false),
            Err(ReviewAnalysisCompletionError::Schema(
                PostAnalysisRegroupingError::MissingSummary
            ))
        ));
        let mut response = ReviewAnalysisResponse::default();
        response.observe(StreamEvent::TextDelta(
            serde_json::json!({
                "summary": "Foreign reference", "triage": [],
                "independent_issues": ["foreign"], "architectural_groups": [],
            })
            .to_string(),
        ));
        assert!(matches!(
            response.finish(&session(), false),
            Err(ReviewAnalysisCompletionError::Schema(
                PostAnalysisRegroupingError::InvalidReference(_)
            ))
        ));
    }
}
