//! Persist a classified review before recording its completed history exchange.

use crate::api::{Application, OperationGuard, PreparedReviewPostAnalysis};
use std::path::Path;
use tauqe_core::review::analysis::PostAnalysisRegrouping;
use tauqe_core::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisPersistenceError {
    #[error("review post-analysis persistence requires an active operation for this session")]
    OperationNotOwned,
    #[error("review post-analysis regrouping belongs to a different review session")]
    MismatchedReview,
    #[error("Review post-analysis of session '{0}' was cancelled before it was persisted")]
    Cancelled(String),
    #[error("{0}")]
    Storage(String),
    #[error("Review session '{0}' disappeared before its regrouping was persisted")]
    NotFound(String),
}

impl Application {
    pub(crate) async fn persist_review_post_analysis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        prepared: &PreparedReviewPostAnalysis,
        regrouping: PostAnalysisRegrouping,
    ) -> Result<ReviewSession, ReviewAnalysisPersistenceError> {
        if !self.owns_operation(operation) {
            return Err(ReviewAnalysisPersistenceError::OperationNotOwned);
        }
        let session = &prepared.session;
        if !regrouping.is_for_review(&session.id) {
            return Err(ReviewAnalysisPersistenceError::MismatchedReview);
        }
        // Preserve the final cancellation gate directly before scheduling storage.
        // Once storage starts, the existing mutation remains cancel-blind.
        if *operation.cancel_receiver().borrow() {
            return Err(ReviewAnalysisPersistenceError::Cancelled(
                session.id.clone(),
            ));
        }
        let statistics = regrouping.statistics();
        let history_response = regrouping.history_response().to_string();
        let save_root = root.to_path_buf();
        let stored = tokio::task::spawn_blocking(move || regrouping.persist(&save_root))
            .await
            .map_err(|error| {
                ReviewAnalysisPersistenceError::Storage(format!("blocking task failed: {error}"))
            })?
            .map_err(|error| ReviewAnalysisPersistenceError::Storage(format!("{error:#}")))?
            .ok_or_else(|| ReviewAnalysisPersistenceError::NotFound(session.id.clone()))?;
        tracing::info!(
            review_id = session.id.as_str(), findings = session.items.len(),
            independent_issues = statistics.independent_issues,
            architectural_groups = statistics.architectural_groups,
            routed_todo = statistics.routed_todo,
            routed_discussion = statistics.routed_discussion,
            model = %prepared.tier.model,
            "review post-analysis persisted"
        );
        // Failed/cancelled persistence leaves no history exchange. Journal errors
        // after a durable regrouping remain best effort, without rolling it back.
        let history_turn_prompt = format!(
            "# Review Post-Analysis: [{}] {}\n\nClassify the {} finding(s) of the session into independent issues and architectural root-cause groups, and route every finding to the status its resolution starts from.",
            session.id, session.title, session.items.len(),
        );
        let _ = self
            .record_history_exchange(
                history_turn_prompt,
                session.target_files.clone(),
                history_response,
                Some(format!("Review Post-Analysis: {}", session.title)),
            )
            .await;
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::infrastructure::execution::PlanExecutionEnvironment;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::history::entry::HistoryEntry;
    use tauqe_core::models::{ModelEntry, ModelRef};
    use tauqe_core::review::analysis::PostAnalysisOutput;
    use tauqe_core::review::ReviewStorage;

    const UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";

    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let config = AppConfig {
            models: ModelsConfig {
                architect: Some(vec![ModelEntry::new(
                    ModelRef::openrouter("test/architect"),
                    None,
                )]),
                ..Default::default()
            },
            ..Default::default()
        };
        Application::for_repository(config, root.to_path_buf(), sender)
    }

    async fn prepare(
        app: &Application,
        operation: &OperationGuard,
        root: &Path,
    ) -> PreparedReviewPostAnalysis {
        let session = ReviewSession {
            id: "completed-review".into(),
            title: "Completed audit".into(),
            target_files: vec!["src/lib.rs".into()],
            items: vec![serde_json::from_value(serde_json::json!({
                "id": 1, "uuid": UUID, "title": "Missing boundary", "type": "architecture",
                "body": "Finding content stays intact"
            }))
            .unwrap()],
            ..Default::default()
        };
        ReviewStorage::save_session(root, &session).unwrap();
        let environment = PlanExecutionEnvironment::new(app.configuration().await, operation);
        app.prepare_review_post_analysis(operation, &environment, root, &session, None)
            .await
            .unwrap()
    }

    fn regrouping(prepared: &PreparedReviewPostAnalysis) -> PostAnalysisRegrouping {
        let output: PostAnalysisOutput = serde_json::from_value(serde_json::json!({
            "summary": "One shared boundary", "triage": [],
            "independent_issues": [UUID], "architectural_groups": [],
        }))
        .unwrap();
        PostAnalysisRegrouping::prepare(&prepared.session, &output).unwrap()
    }

    #[tokio::test]
    async fn durable_regrouping_precedes_the_existing_history_exchange() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        let stored = app
            .persist_review_post_analysis(&operation, root.path(), &prepared, regrouping(&prepared))
            .await
            .unwrap();
        assert_eq!(stored.description.as_deref(), Some("One shared boundary"));
        assert_eq!(stored.items[0].body, prepared.session.items[0].body);
        assert_eq!(
            ReviewStorage::load_review(root.path(), &stored.id)
                .unwrap()
                .unwrap(),
            stored
        );
        let raw = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
        let entries: Vec<HistoryEntry> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], HistoryEntry::Turn { prompt, context, .. }
            if prompt.starts_with("# Review Post-Analysis: [completed-review] Completed audit")
            && context == &prepared.session.target_files)
        );
        assert!(
            matches!(&entries[1], HistoryEntry::Response { message, summary, .. }
            if message.ends_with("One shared boundary")
            && summary.as_deref() == Some("Review Post-Analysis: Completed audit"))
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn cancellation_before_persistence_changes_neither_review_nor_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        app.cancel_active_operation();
        let result = app
            .persist_review_post_analysis(&operation, root.path(), &prepared, regrouping(&prepared))
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPersistenceError::Cancelled(_))
        ));
        assert_eq!(
            ReviewStorage::load_review(root.path(), &prepared.session.id)
                .unwrap()
                .unwrap(),
            prepared.session
        );
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }

    #[tokio::test]
    async fn missing_review_leaves_no_history_exchange() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        ReviewStorage::delete_review(root.path(), &prepared.session.id).unwrap();
        let result = app
            .persist_review_post_analysis(&operation, root.path(), &prepared, regrouping(&prepared))
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPersistenceError::NotFound(_))
        ));
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }

    #[tokio::test]
    async fn history_write_failure_keeps_the_successfully_persisted_review() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        std::fs::create_dir(root.path().join(".tauqe/history.jsonl")).unwrap();
        let stored = app
            .persist_review_post_analysis(&operation, root.path(), &prepared, regrouping(&prepared))
            .await
            .unwrap();
        assert_eq!(stored.description.as_deref(), Some("One shared boundary"));
        assert_eq!(
            ReviewStorage::load_review(root.path(), &stored.id)
                .unwrap()
                .unwrap(),
            stored
        );
    }

    #[tokio::test]
    async fn foreign_operation_or_mismatched_regrouping_cannot_persist() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let result = app
            .persist_review_post_analysis(&foreign, root.path(), &prepared, regrouping(&prepared))
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPersistenceError::OperationNotOwned)
        ));
        let mut different = prepared.session.clone();
        different.id = "different-review".into();
        let output: PostAnalysisOutput = serde_json::from_value(serde_json::json!({"summary": "Do not save", "triage": [], "independent_issues": [], "architectural_groups": []})).unwrap();
        let result = app
            .persist_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                PostAnalysisRegrouping::prepare(&different, &output).unwrap(),
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPersistenceError::MismatchedReview)
        ));
        assert_eq!(
            ReviewStorage::load_review(root.path(), &prepared.session.id)
                .unwrap()
                .unwrap(),
            prepared.session
        );
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }
}
