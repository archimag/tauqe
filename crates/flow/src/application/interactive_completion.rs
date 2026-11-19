//! Classify completed interactive turns and persist their terminal response.

use crate::api::{Application, ContextClearScope, HistoryError, OperationGuard};

use tauqe_core::control::outcome::{LoopResult, SystemError};
use tauqe_core::control::repl::ReplArtifact;
use tauqe_core::edits::EditOutcome;
use tauqe_core::edits::VerificationVerdict;
use tauqe_core::history::HistoryFileChange;
use tauqe_core::llm::ModelResult;
use tauqe_core::plan::PlanRejection;

pub struct InteractiveTurnCompletion {
    pub result: ModelResult,
    pub(crate) error: Option<String>,
    pub(crate) plans_applied: bool,
    pub plan_rejections: Vec<PlanRejection>,
    pub plan_warnings: Vec<String>,
    pub(crate) review_decisions_applied: Vec<u32>,
    history_response: String,
}

pub fn classify_interactive_completion(
    result: Result<LoopResult<ReplArtifact>, SystemError>,
    streamed_text: String,
) -> InteractiveTurnCompletion {
    let (artifact, error) = match result {
        Ok(LoopResult::Success(success)) => (Some(success.value), None),
        Ok(LoopResult::Diverged(_)) => (None, Some("Execution diverged".to_string())),
        Ok(LoopResult::Exhausted(report)) => (None, Some(report.reason)),
        Err(error) => (None, Some(error.to_string())),
    };
    let (plans_applied, plan_rejections, plan_warnings, review_decisions_applied) = match &artifact
    {
        Some(artifact) => (
            artifact.plans_applied,
            artifact.plan_rejections.clone(),
            artifact.plan_warnings.clone(),
            artifact.review_decisions_applied.clone(),
        ),
        None => (false, Vec::new(), Vec::new(), Vec::new()),
    };
    let (result, history_response) = match artifact {
        Some(artifact) => {
            let text = artifact.message.unwrap_or_else(|| {
                if !streamed_text.trim().is_empty() {
                    streamed_text
                } else {
                    "Research completed.".to_string()
                }
            });
            let result = if artifact.modified_paths.is_empty() {
                ModelResult::Answer { text: text.clone() }
            } else {
                ModelResult::Edit {
                    summary: "REPL changes".to_string(),
                    edits: Vec::new(),
                    proposal: None,
                    outcome: EditOutcome {
                        applied: true,
                        error: None,
                        changed_files: artifact.modified_paths,
                        commit_hash: artifact.commit_hash,
                        verification: Some(VerificationVerdict::Passed),
                    },
                }
            };
            (result, text)
        }
        None => {
            let text = if !streamed_text.trim().is_empty() {
                streamed_text
            } else {
                error
                    .clone()
                    .unwrap_or_else(|| "Operation failed".to_string())
            };
            (ModelResult::Answer { text: text.clone() }, text)
        }
    };
    InteractiveTurnCompletion {
        result,
        error,
        plans_applied,
        plan_rejections,
        plan_warnings,
        review_decisions_applied,
        history_response,
    }
}

impl Application {
    /// Keep the existing cleanup policy even when the journal write fails.
    /// The adapter publishes edit/Git events before calling this scenario.
    pub(crate) async fn persist_interactive_completion(
        &self,
        operation: &OperationGuard,
        completion: &InteractiveTurnCompletion,
    ) -> Result<(), HistoryError> {
        if !self.owns_operation(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let (commit, summary, files) = match &completion.result {
            ModelResult::Answer { .. } => (None, None, Vec::new()),
            ModelResult::Edit {
                summary, outcome, ..
            } => (
                outcome.commit_hash.clone(),
                Some(summary.clone()),
                outcome
                    .changed_files
                    .iter()
                    .cloned()
                    .map(HistoryFileChange::plain)
                    .collect(),
            ),
        };
        let persisted = self
            .record_history_response(completion.history_response.clone(), commit, summary, files)
            .await;
        if matches!(completion.result, ModelResult::Edit { .. }) {
            let _ = self.clear_context(ContextClearScope::Transient).await;
            self.reset_session_uuid().await;
        }
        persisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::control::outcome::{FailureReport, LoopSuccess, ResponseEnvelope};
    use tauqe_core::history::entry::HistoryEntry;

    fn artifact(message: Option<&str>, paths: Vec<String>) -> ReplArtifact {
        ReplArtifact {
            message: message.map(str::to_string),
            modified_paths: paths,
            plans_applied: false,
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
            review_decisions_applied: Vec::new(),
            envelope: ResponseEnvelope::default(),
            commit_hash: None,
        }
    }

    fn completed(artifact: ReplArtifact, streamed: &str) -> InteractiveTurnCompletion {
        classify_interactive_completion(
            Ok(LoopResult::Success(LoopSuccess::new(
                artifact,
                ResponseEnvelope::default(),
            ))),
            streamed.to_string(),
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
    fn completion_preserves_message_precedence_and_mutation_metadata() {
        for (message, streamed, expected) in [
            (Some("Answer"), "Stream", "Answer"),
            (Some(""), "Stream", ""),
            (None, " Stream ", " Stream "),
            (None, "  ", "Research completed."),
        ] {
            let completion = completed(artifact(message, Vec::new()), streamed);
            assert_eq!(
                completion.result,
                ModelResult::Answer {
                    text: expected.into()
                }
            );
            assert!(completion.error.is_none());
        }
        let mut value = artifact(Some("Updated"), Vec::new());
        value.plans_applied = true;
        value.plan_rejections = vec![PlanRejection::payload_defect("invalid plan update")];
        value.plan_warnings = vec!["Adjusted status".into()];
        value.review_decisions_applied = vec![7];
        let completion = completed(value, "");
        assert!(completion.plans_applied);
        assert_eq!(completion.plan_rejections.len(), 1);
        assert_eq!(completion.plan_warnings, vec!["Adjusted status"]);
        assert_eq!(completion.review_decisions_applied, vec![7]);
    }

    #[test]
    fn failed_loop_outcomes_preserve_error_and_streamed_answer_fallbacks() {
        let cases = [
            (
                Ok(LoopResult::Diverged(ResponseEnvelope::default())),
                "Execution diverged",
            ),
            (
                Ok(LoopResult::Exhausted(FailureReport::new(
                    "round budget",
                    3,
                    Vec::new(),
                ))),
                "round budget",
            ),
            (
                Err(SystemError::Provider("offline".into())),
                "provider error: offline",
            ),
        ];
        for (result, expected) in cases {
            let completion = classify_interactive_completion(result, "Streamed answer".into());
            assert_eq!(completion.error.as_deref(), Some(expected));
            assert_eq!(
                completion.result,
                ModelResult::Answer {
                    text: "Streamed answer".into()
                }
            );
        }
        let completion =
            classify_interactive_completion(Err(SystemError::Other("failed".into())), "  ".into());
        assert_eq!(
            completion.result,
            ModelResult::Answer {
                text: "infrastructure error: failed".into()
            }
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
        let completion = completed(
            artifact(Some("Full developer reply"), vec!["discovered.rs".into()]),
            "",
        );
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
        let history = app.history_for_turn().await.unwrap().snapshot().unwrap();
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
        let completion = completed(artifact(Some("Answer"), Vec::new()), "");
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
        let completion = completed(artifact(None, vec!["discovered.rs".into()]), "");
        let operation = app.try_begin_operation().unwrap();
        assert!(matches!(
            app.persist_interactive_completion(&operation, &completion)
                .await,
            Err(HistoryError::Storage(_))
        ));
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
        let completion = completed(artifact(None, vec!["discovered.rs".into()]), "");
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
