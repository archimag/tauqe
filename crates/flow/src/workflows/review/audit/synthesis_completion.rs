//! Persist a single audit around the adapter's saved-review publication.

use super::AuditResources;
use crate::api::OperationGuard;
use crate::application::documents::Documents;
use std::future::Future;
use std::path::Path;
use tauqe_core::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisPersistenceError {
    #[error("review synthesis persistence requires an active operation for this session")]
    OperationNotOwned,
    #[error("review synthesis was cancelled before it was persisted")]
    Cancelled,
    #[error("{0}")]
    Storage(String),
}

impl AuditResources {
    /// Keep the operation registered through saved-review publication and history.
    /// The callback publishes only; no history lock is held while it runs.
    pub(crate) async fn persist_review_synthesis<F, R>(
        &self,
        operation: &OperationGuard,
        root: &Path,
        session: &ReviewSession,
        publish_saved: F,
    ) -> Result<(), ReviewSynthesisPersistenceError>
    where
        F: FnOnce() -> R + Send,
        R: Future<Output = ()> + Send,
    {
        if !self.execution.owns(operation) {
            return Err(ReviewSynthesisPersistenceError::OperationNotOwned);
        }
        if *operation.cancel_receiver().borrow() {
            return Err(ReviewSynthesisPersistenceError::Cancelled);
        }
        Documents::new(root)
            .save_review(session)
            .await
            .map_err(ReviewSynthesisPersistenceError::Storage)?;

        // Cancellation after the write starts does not withdraw a durable audit.
        publish_saved().await;
        if let Err(error) = self
            .history
            .record_history_review(
                session.model.to_string(),
                session.user_prompt.clone(),
                session.description.clone(),
                session.target_files.clone(),
                session.items.len(),
            )
            .await
        {
            tracing::warn!(error = %error, "Failed to record the review in interaction history");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tauqe_core::config::AppConfig;
    use tauqe_core::history::entry::HistoryEntry;
    use tauqe_core::models::ModelRef;
    use tauqe_core::review::ReviewStorage;

    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root.to_path_buf(), sender)
    }
    fn session() -> ReviewSession {
        ReviewSession {
            id: "single-review".into(),
            title: "Single audit".into(),
            created_at: 42,
            model: ModelRef::openrouter("test/reviewer"),
            description: Some("Audit summary".into()),
            user_prompt: Some("Audit boundaries".into()),
            target_files: vec!["src/lib.rs".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn saved_review_publication_precedes_history_and_holds_no_history_lock() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let session = session();
        let published = AtomicBool::new(false);
        app.audit_resources()
            .persist_review_synthesis(&operation, root.path(), &session, || async {
                assert_eq!(
                    ReviewStorage::load_review(root.path(), &session.id)
                        .unwrap()
                        .unwrap(),
                    session
                );
                assert!(!root.path().join(".tauqe/history.jsonl").exists());
                let history =
                    tokio::time::timeout(Duration::from_secs(1), app.history_page(None, None))
                        .await
                        .unwrap()
                        .unwrap();
                assert_eq!(history.total_count, 0);
                published.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();
        assert!(published.load(Ordering::SeqCst));
        let raw = std::fs::read_to_string(root.path().join(".tauqe/history.jsonl")).unwrap();
        let entries: Vec<HistoryEntry> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], HistoryEntry::Review { model, user_prompt, files, findings_count, summary, .. }
            if model == &session.model.to_string() && user_prompt == &session.user_prompt
            && files == &session.target_files && *findings_count == session.items.len()
            && summary == &session.description)
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn cancellation_before_save_prevents_publication_and_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        app.cancel_active_operation();
        let published = AtomicBool::new(false);
        let result = app
            .audit_resources()
            .persist_review_synthesis(&operation, root.path(), &session(), || async {
                published.store(true, Ordering::SeqCst);
            })
            .await;
        assert!(matches!(
            result,
            Err(ReviewSynthesisPersistenceError::Cancelled)
        ));
        assert!(!published.load(Ordering::SeqCst));
        assert!(ReviewStorage::load_review(root.path(), "single-review")
            .unwrap()
            .is_none());
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }

    #[tokio::test]
    async fn cancellation_after_saved_publication_does_not_withdraw_review_or_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        app.audit_resources()
            .persist_review_synthesis(&operation, root.path(), &session(), || async {
                app.cancel_active_operation();
            })
            .await
            .unwrap();
        assert!(ReviewStorage::load_review(root.path(), "single-review")
            .unwrap()
            .is_some());
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
    }

    #[tokio::test]
    async fn failed_review_save_prevents_publication_and_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        std::fs::create_dir_all(root.path().join(".tauqe")).unwrap();
        std::fs::write(root.path().join(".tauqe/reviews"), "blocks review storage").unwrap();
        let published = AtomicBool::new(false);
        let result = app
            .audit_resources()
            .persist_review_synthesis(&operation, root.path(), &session(), || async {
                published.store(true, Ordering::SeqCst);
            })
            .await;
        assert!(matches!(
            result,
            Err(ReviewSynthesisPersistenceError::Storage(_))
        ));
        assert!(!published.load(Ordering::SeqCst));
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }

    #[tokio::test]
    async fn history_failure_keeps_the_saved_and_published_review() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        let published = AtomicBool::new(false);
        app.audit_resources()
            .persist_review_synthesis(&operation, root.path(), &session(), || async {
                published.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();
        assert!(published.load(Ordering::SeqCst));
        assert_eq!(
            ReviewStorage::load_review(root.path(), "single-review")
                .unwrap()
                .unwrap(),
            session()
        );
    }

    #[tokio::test]
    async fn foreign_operation_cannot_save_or_publish() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let published = AtomicBool::new(false);
        let result = app
            .audit_resources()
            .persist_review_synthesis(&foreign, root.path(), &session(), || async {
                published.store(true, Ordering::SeqCst);
            })
            .await;
        assert!(matches!(
            result,
            Err(ReviewSynthesisPersistenceError::OperationNotOwned)
        ));
        assert!(!published.load(Ordering::SeqCst));
        assert!(!root.path().join(".tauqe/reviews").exists());
    }
}
