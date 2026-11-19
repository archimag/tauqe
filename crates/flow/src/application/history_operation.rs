//! Exclusive dialogue-history cleanup with its dependent session transitions.

use crate::api::{Application, FocusUpdate, HistoryError, OperationGuard, OperationInProgress};

pub struct HistoryOperation {
    application: Application,
    guard: OperationGuard,
}

/// Journal deletion may fail independently of the in-memory dialogue reset.
/// Adapters retain control of how that existing best-effort outcome is reported.
pub struct HistoryClearOutcome {
    pub focus_update: FocusUpdate,
    pub persistence_error: Option<HistoryError>,
}

impl Application {
    pub(crate) fn begin_history_change(&self) -> Result<HistoryOperation, OperationInProgress> {
        let guard = self.try_begin_operation()?;
        Ok(HistoryOperation {
            application: self.clone(),
            guard,
        })
    }
}

impl HistoryOperation {
    /// Always detach focus and rotate dialogue identity, preserving the existing
    /// cleanup policy even when deleting the persisted journal fails.
    async fn clear(&self) -> HistoryClearOutcome {
        let persistence_error = self.application.clear_history(&self.guard).await.err();
        let focus_update = self.application.clear_discussion_focus().await;
        self.application.reset_session_uuid().await;
        HistoryClearOutcome {
            focus_update,
            persistence_error,
        }
    }
}

impl Application {
    pub async fn clear_dialogue_history(
        &self,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<(), OperationInProgress> {
        let operation = self.begin_history_change()?;
        let outcome = operation.clear().await;
        if let Some(error) = outcome.persistence_error {
            tracing::warn!(%error, "Failed to clear the interaction history");
        }
        outcome.focus_update.publish("", observer);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{DiscussionFocus, DiscussionTarget};
    use std::path::PathBuf;
    use tauqe_core::config::AppConfig;

    fn application(root: PathBuf) -> Application {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root, sender)
    }

    async fn focus(app: &Application) -> DiscussionFocus {
        let focus = DiscussionFocus::Focused {
            target: DiscussionTarget::Plan {
                id: "plan-1".into(),
            },
            focused_items: vec!["step-1".into()],
        };
        app.set_discussion_focus(focus.clone()).await;
        focus
    }

    #[tokio::test]
    async fn an_active_turn_refuses_cleanup_without_changing_dialogue_state() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path().to_path_buf());
        app.record_history_turn("Keep me".into(), Vec::new())
            .await
            .unwrap();
        let focus = focus(&app).await;
        let session = app.session_uuid().await;
        let turn = app.try_begin_operation().unwrap();
        assert!(app.begin_history_change().is_err());
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
        assert_eq!(app.discussion_focus().await, focus);
        assert_eq!(app.session_uuid().await, session);
        drop(turn);
        assert!(app.begin_history_change().is_ok());
    }

    #[tokio::test]
    async fn clearing_resets_journal_focus_and_dialogue_while_retaining_the_lease() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path().to_path_buf());
        app.record_history_turn("Task".into(), Vec::new())
            .await
            .unwrap();
        focus(&app).await;
        let old_session = app.session_uuid().await;
        let operation = app.begin_history_change().unwrap();
        let outcome = operation.clear().await;
        assert!(outcome.persistence_error.is_none());
        assert!(outcome.focus_update.changed);
        assert_eq!(outcome.focus_update.focus, DiscussionFocus::None);
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
        assert_eq!(app.history_token_estimate().await, 0);
        assert_ne!(app.session_uuid().await, old_session);
        assert!(app.try_begin_operation().is_err());
        let session = app.session_uuid().await;
        let repeated = operation.clear().await;
        assert!(!repeated.focus_update.changed);
        assert_ne!(app.session_uuid().await, session);
        drop(operation);
        assert!(app.try_begin_operation().is_ok());
    }

    #[tokio::test]
    async fn deletion_failure_is_reported_without_skipping_the_dialogue_reset() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path().to_path_buf());
        let blocked = root.path().join(".tauqe/history.jsonl");
        std::fs::create_dir_all(&blocked).unwrap();
        focus(&app).await;
        let old_session = app.session_uuid().await;
        let operation = app.begin_history_change().unwrap();
        let outcome = operation.clear().await;
        assert!(matches!(
            outcome.persistence_error,
            Some(HistoryError::Storage(_))
        ));
        assert!(blocked.is_dir());
        assert_eq!(app.discussion_focus().await, DiscussionFocus::None);
        assert!(outcome.focus_update.changed);
        assert_ne!(app.session_uuid().await, old_session);
    }
}
