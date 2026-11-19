//! Preserve journal transitions when interactive execution is interrupted.

use crate::api::{Application, HistoryError, OperationGuard};

#[derive(Debug)]
pub enum InteractiveInterruption {
    Cancelled {
        turn_id: Option<u64>,
    },
    StreamFailure {
        streamed_text: String,
        error: Option<String>,
    },
    TaskFailure {
        reason: String,
    },
}

impl Application {
    pub(crate) async fn persist_interactive_interruption(
        &self,
        operation: &OperationGuard,
        interruption: InteractiveInterruption,
    ) -> Result<(), HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let text = match interruption {
            InteractiveInterruption::Cancelled { turn_id } => {
                return match turn_id {
                    Some(id) => {
                        self.rollback_history_entry(id, "Operation cancelled".into())
                            .await
                    }
                    None => Ok(()),
                };
            }
            InteractiveInterruption::StreamFailure {
                streamed_text,
                error,
            } => {
                if !streamed_text.trim().is_empty() {
                    streamed_text
                } else {
                    error.unwrap_or_else(|| "Operation error".to_string())
                }
            }
            InteractiveInterruption::TaskFailure { reason } => {
                format!("Workflow task failed: {reason}")
            }
        };
        self.record_history_response(text, None, None, Vec::new())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::history::HistoryEntryStatus;
    use tauqe_core::history::{entry::HistoryEntry, HistoryMutation};

    use tokio::sync::mpsc;

    fn application(
        root: &std::path::Path,
    ) -> (Application, mpsc::UnboundedReceiver<HistoryMutation>) {
        std::fs::write(root.join("resident.rs"), "source").unwrap();
        let (history, receiver) = mpsc::unbounded_channel();
        (
            Application::for_repository(AppConfig::default(), root.to_path_buf(), history),
            receiver,
        )
    }

    async fn begin(app: &Application) -> (OperationGuard, Option<u64>) {
        app.test_mutate_context(|context| {
            context.add_auto_file("resident.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let operation = app.try_begin_operation().unwrap();
        let started = app
            .prepare_interactive_history(&operation, "Request".into())
            .await
            .unwrap();
        (operation, started.turn_id)
    }

    #[tokio::test]
    async fn cancellation_rolls_back_only_the_addressed_turn_and_publishes_the_existing_reason() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut events) = application(root.path());
        let (operation, turn_id) = begin(&app).await;
        while events.try_recv().is_ok() {}
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        app.persist_interactive_interruption(
            &operation,
            InteractiveInterruption::Cancelled { turn_id },
        )
        .await
        .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 0);
        match events.try_recv().unwrap() {
            HistoryMutation::RolledBack { item } => {
                assert_eq!(item.status, HistoryEntryStatus::RolledBack);
                assert_eq!(item.summary.as_deref(), Some("Operation cancelled"));
            }
            _ => panic!("cancellation must publish the turn rollback"),
        }
        assert!(events.try_recv().is_err());
        assert_eq!(identity, app.session_uuid().await);
        assert_eq!(revision, app.context_state().await.revision);
    }

    #[tokio::test]
    async fn cancellation_without_a_turn_id_does_not_remove_an_existing_turn() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut events) = application(root.path());
        let (operation, _) = begin(&app).await;
        while events.try_recv().is_ok() {}
        app.persist_interactive_interruption(
            &operation,
            InteractiveInterruption::Cancelled { turn_id: None },
        )
        .await
        .unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn stream_failure_preserves_text_and_error_fallback_precedence() {
        let cases = [
            (" Streamed text ", Some("provider error"), " Streamed text "),
            ("  ", Some("provider error"), "provider error"),
            ("", None, "Operation error"),
            ("", Some(""), ""),
        ];
        for (streamed, error, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            let (app, _) = application(root.path());
            let (operation, _) = begin(&app).await;
            let identity = app.session_uuid().await;
            let revision = app.context_state().await.revision;
            app.persist_interactive_interruption(
                &operation,
                InteractiveInterruption::StreamFailure {
                    streamed_text: streamed.into(),
                    error: error.map(str::to_string),
                },
            )
            .await
            .unwrap();
            let snapshot = app.history_for_turn().await.unwrap().snapshot().unwrap();
            assert_eq!(snapshot.entries().len(), 2);
            match snapshot.entries().last().unwrap() {
                HistoryEntry::Response {
                    message,
                    commit,
                    summary,
                    files,
                    ..
                } => {
                    assert_eq!(message, expected);
                    assert!(commit.is_none());
                    assert!(summary.is_none());
                    assert!(files.is_empty());
                }
                _ => panic!("stream failure must preserve the legacy response record"),
            }
            assert_eq!(identity, app.session_uuid().await);
            assert_eq!(revision, app.context_state().await.revision);
        }
    }

    #[tokio::test]
    async fn task_failure_is_not_reclassified_by_its_diagnostic_text() {
        let root = tempfile::tempdir().unwrap();
        let (app, _) = application(root.path());
        let (operation, _) = begin(&app).await;
        app.persist_interactive_interruption(
            &operation,
            InteractiveInterruption::TaskFailure {
                reason: "Operation cancelled".into(),
            },
        )
        .await
        .unwrap();
        let snapshot = app.history_for_turn().await.unwrap().snapshot().unwrap();
        assert_eq!(snapshot.entries().len(), 2);
        assert!(
            matches!(snapshot.entries().last().unwrap(), HistoryEntry::Response { message, .. } if message == "Workflow task failed: Operation cancelled")
        );
    }

    #[tokio::test]
    async fn persistence_failure_does_not_clear_context_or_rotate_dialogue() {
        let root = tempfile::tempdir().unwrap();
        let (app, _) = application(root.path());
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        app.test_mutate_context(|context| {
            context.add_auto_file("resident.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let operation = app.try_begin_operation().unwrap();
        assert!(matches!(
            app.persist_interactive_interruption(
                &operation,
                InteractiveInterruption::TaskFailure {
                    reason: "panic".into()
                }
            )
            .await,
            Err(HistoryError::Storage(_))
        ));
        assert_eq!(identity, app.session_uuid().await);
        assert_eq!(revision, app.context_state().await.revision);
    }

    #[tokio::test]
    async fn a_foreign_operation_cannot_rollback_or_append_history() {
        let root = tempfile::tempdir().unwrap();
        let (app, _) = application(root.path());
        let (_operation, turn_id) = begin(&app).await;
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        for interruption in [
            InteractiveInterruption::Cancelled { turn_id },
            InteractiveInterruption::TaskFailure {
                reason: "panic".into(),
            },
        ] {
            assert!(matches!(
                app.persist_interactive_interruption(&foreign, interruption)
                    .await,
                Err(HistoryError::OperationNotOwned)
            ));
        }
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
    }
}
