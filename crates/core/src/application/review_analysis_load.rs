//! Explicit review selection and guarded loading for post-analysis.

use crate::api::{Application, OperationGuard};
use crate::review::ReviewSession;
use crate::review::ReviewStorage;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisLoadError {
    #[error("review_id is required: post-analysis is scoped to an explicit session")]
    MissingReviewId,
    #[error("review post-analysis loading requires an active operation for this session")]
    OperationNotOwned,
    #[error("Review session '{0}' not found")]
    NotFound(String),
    #[error("{0}")]
    Storage(String),
}

/// Resolve the explicit target before claiming the operation slot. An empty
/// target never falls back to the active/latest review.
pub fn parse_review_analysis_target(id: &str) -> Result<String, ReviewAnalysisLoadError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(ReviewAnalysisLoadError::MissingReviewId);
    }
    Ok(id.to_string())
}

impl Application {
    pub async fn load_review_for_post_analysis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        review_id: &str,
    ) -> Result<ReviewSession, ReviewAnalysisLoadError> {
        if !self.owns_operation(operation) {
            return Err(ReviewAnalysisLoadError::OperationNotOwned);
        }
        let review_id = parse_review_analysis_target(review_id)?;
        let load_id = review_id.clone();
        let load_root = root.to_path_buf();
        tokio::task::spawn_blocking(move || ReviewStorage::load_review(&load_root, &load_id))
            .await
            .map_err(|error| {
                ReviewAnalysisLoadError::Storage(format!("blocking task failed: {error}"))
            })?
            .map_err(|error| ReviewAnalysisLoadError::Storage(format!("{error:#}")))?
            .ok_or(ReviewAnalysisLoadError::NotFound(review_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(root: &Path, id: &str) -> ReviewSession {
        let session = ReviewSession {
            id: id.into(),
            title: format!("Review {id}"),
            ..Default::default()
        };
        ReviewStorage::save_session(root, &session).unwrap();
        session
    }

    #[tokio::test]
    async fn loading_uses_the_explicit_trimmed_id_without_changing_storage() {
        let root = tempfile::tempdir().unwrap();
        let requested = save(root.path(), "requested");
        save(root.path(), "active");
        ReviewStorage::save_active_id(root.path(), Some("active")).unwrap();
        let path = root.path().join(".tauqe/reviews/requested.json");
        let before = std::fs::read(&path).unwrap();
        let app = Application::default();
        let operation = app.try_begin_operation().unwrap();
        let loaded = app
            .load_review_for_post_analysis(&operation, root.path(), "  requested  ")
            .await
            .unwrap();
        assert_eq!(loaded, requested);
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(
            ReviewStorage::load_active_id(root.path())
                .unwrap()
                .as_deref(),
            Some("active")
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn blank_and_unknown_targets_never_fall_back_to_the_active_review() {
        let root = tempfile::tempdir().unwrap();
        save(root.path(), "active");
        ReviewStorage::save_active_id(root.path(), Some("active")).unwrap();
        let app = Application::default();
        let operation = app.try_begin_operation().unwrap();
        assert!(matches!(
            parse_review_analysis_target(" "),
            Err(ReviewAnalysisLoadError::MissingReviewId)
        ));
        assert!(matches!(
            app.load_review_for_post_analysis(&operation, root.path(), " ")
                .await,
            Err(ReviewAnalysisLoadError::MissingReviewId)
        ));
        let result = app
            .load_review_for_post_analysis(&operation, root.path(), " unknown ")
            .await;
        assert!(matches!(result, Err(ReviewAnalysisLoadError::NotFound(id)) if id == "unknown"));
    }

    #[tokio::test]
    async fn foreign_operation_is_rejected_before_repository_access() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        let app = Application::default();
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(matches!(
            app.load_review_for_post_analysis(&foreign, &absent, "review")
                .await,
            Err(ReviewAnalysisLoadError::OperationNotOwned)
        ));
        assert!(!absent.exists());
    }
}
