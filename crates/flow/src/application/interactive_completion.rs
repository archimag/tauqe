//! Project completed interactive turns and persist their terminal response.

use crate::api::{Application, ContextClearScope, HistoryError, OperationGuard};

use tauqe_core::edits::EditOutcome;
use tauqe_core::history::HistoryFileChange;
use tauqe_core::llm::ModelResult;
use tauqe_core::plan::PlanRejection;

pub(crate) const REPL_EDIT_SUMMARY: &str = "REPL changes";

pub struct InteractiveTurnCompletion {
    pub result: ModelResult,
    pub(crate) error: Option<String>,
    pub plan_rejections: Vec<PlanRejection>,
    pub plan_warnings: Vec<String>,
    pub(crate) history_response: String,
}

impl InteractiveTurnCompletion {
    pub(crate) fn answer(text: String) -> Self {
        Self {
            result: ModelResult::Answer { text: text.clone() },
            error: None,
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
            history_response: text,
        }
    }

    pub(crate) fn failed(error: String) -> Self {
        Self {
            error: Some(error.clone()),
            ..Self::answer(error)
        }
    }

    pub(crate) fn edit(text: String, outcome: EditOutcome) -> Self {
        Self {
            error: outcome.error.clone(),
            history_response: outcome.error.clone().unwrap_or(text),
            result: ModelResult::Edit {
                summary: REPL_EDIT_SUMMARY.into(),
                edits: Vec::new(),
                proposal: None,
                outcome,
            },
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
        }
    }
}

impl Application {
    /// Record the response independently of publishing the settled edit.
    pub(crate) async fn persist_interactive_completion(
        &self,
        operation: &OperationGuard,
        completion: &InteractiveTurnCompletion,
    ) -> Result<(), HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let edit = match &completion.result {
            ModelResult::Answer { .. } => None,
            ModelResult::Edit {
                summary, outcome, ..
            } => Some((outcome, summary.as_str())),
        };
        self.record_interactive_response(completion.history_response.clone(), edit)
            .await
    }

    pub(crate) async fn record_interactive_response(
        &self,
        message: String,
        edit: Option<(&EditOutcome, &str)>,
    ) -> Result<(), HistoryError> {
        let (commit, summary, files) = match edit {
            Some((outcome, summary)) if outcome.applied => (
                outcome.commit_hash.clone(),
                Some(summary.to_owned()),
                outcome
                    .changed_files
                    .iter()
                    .cloned()
                    .map(HistoryFileChange::plain)
                    .collect(),
            ),
            _ => (None, None, Vec::new()),
        };
        self.record_history_response(message, commit, summary, files)
            .await
    }

    /// An applied edit invalidates transient context even if history is unavailable.
    pub(crate) async fn clean_up_interactive_edit(&self) {
        let _ = self.clear_context(ContextClearScope::Transient).await;
        self.reset_session_uuid().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::edits::VerificationVerdict;
    use tauqe_core::history::entry::HistoryEntry;

    fn applied_edit(
        text: &str,
        verification: Option<VerificationVerdict>,
    ) -> InteractiveTurnCompletion {
        InteractiveTurnCompletion::edit(
            text.into(),
            EditOutcome {
                applied: true,
                changed_files: vec!["discovered.rs".into()],
                verification,
                ..Default::default()
            },
        )
    }

    fn application(root: &std::path::Path) -> Application {
        std::fs::write(root.join("discovered.rs"), "source").unwrap();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            tauqe_core::config::AppConfig::default(),
            root.to_path_buf(),
            history,
        )
    }

    #[test]
    fn edit_completion_preserves_actual_verification_and_finalization_failure() {
        let completion = InteractiveTurnCompletion::edit(
            "Updated".into(),
            EditOutcome {
                verification: Some(VerificationVerdict::NotConfigured),
                error: Some("Git finalization failed".into()),
                ..Default::default()
            },
        );
        assert_eq!(completion.error.as_deref(), Some("Git finalization failed"));
        let ModelResult::Edit { outcome, .. } = completion.result else {
            panic!("expected edit outcome");
        };
        assert!(!outcome.applied);
        assert!(outcome.commit_hash.is_none());
        assert_eq!(outcome.error.as_deref(), Some("Git finalization failed"));
        assert_eq!(
            outcome.verification,
            Some(VerificationVerdict::NotConfigured)
        );
    }

    #[tokio::test]
    async fn edit_completion_records_the_full_reply_and_releases_transient_context_without_git() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.record_history_turn("Request".into(), Vec::new())
            .await
            .unwrap();
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let identity = app.session_uuid().await;
        let completion = applied_edit("Full developer reply", Some(VerificationVerdict::Passed));
        match &completion.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(outcome.applied);
                assert_eq!(outcome.commit_hash, None);
                assert_eq!(outcome.verification, Some(VerificationVerdict::Passed));
            }
            _ => panic!("modified paths must produce an edit result"),
        }
        let operation = app.try_begin_operation().unwrap();
        app.persist_interactive_completion(&operation, &completion)
            .await
            .unwrap();
        let history = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        app.clean_up_interactive_edit().await;
        match history.entries().last().unwrap() {
            HistoryEntry::Response {
                message,
                commit,
                summary,
                files,
                ..
            } => {
                assert_eq!(message, "Full developer reply");
                assert_eq!(commit, &None);
                assert_eq!(summary.as_deref(), Some("REPL changes"));
                assert_eq!(files, &vec![HistoryFileChange::plain("discovered.rs")]);
            }
            _ => panic!("completion must record a response"),
        }
        assert!(app.context_state().await.items.is_empty());
        assert_ne!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn answer_completion_preserves_context_and_dialogue_identity() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let identity = app.session_uuid().await;
        let before = app.context_state().await.revision;
        let completion = InteractiveTurnCompletion::answer("Answer".into());
        let operation = app.try_begin_operation().unwrap();
        app.persist_interactive_completion(&operation, &completion)
            .await
            .unwrap();
        assert_eq!(before, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn failed_history_write_does_not_skip_edit_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let identity = app.session_uuid().await;
        let completion = applied_edit("Edited", None);
        let operation = app.try_begin_operation().unwrap();
        assert!(matches!(
            app.persist_interactive_completion(&operation, &completion)
                .await,
            Err(HistoryError::Storage(_))
        ));
        app.clean_up_interactive_edit().await;
        assert!(app.context_state().await.items.is_empty());
        assert_ne!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn a_foreign_operation_cannot_persist_or_clear_a_completion() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let identity = app.session_uuid().await;
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let completion = applied_edit("Edited", None);
        assert!(matches!(
            app.persist_interactive_completion(&foreign, &completion)
                .await,
            Err(HistoryError::OperationNotOwned)
        ));
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
        assert_eq!(app.context_state().await.items.len(), 1);
        assert_eq!(identity, app.session_uuid().await);
    }
}
