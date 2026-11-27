//! Scribe squash-message generation, Git rewrite, and history policy.
use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, OperationGuard, UsageTotalsSnapshot,
};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::workflows::structured::{self, StructuredFailure};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::git::CommitSummary;
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::providers::ChatMessage;
#[derive(Debug, thiserror::Error)]
pub enum SquashMessageError {
    #[error("Cannot generate a squash message while a model operation is in progress")]
    Busy,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Commits(String),
    #[error("{0}")]
    Model(String),
    #[error("{0}")]
    Cancelled(String),
}
struct PreparedSquashMessage {
    commits: Vec<CommitSummary>,
    prompt: String,
}
pub struct SquashMessage {
    pub message: String,
    pub usage: Option<UsageTotalsSnapshot>,
}
async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))
}
impl Application {
    async fn prepare_squash_message(
        &self,
        operation: &OperationGuard,
        requested_ref: &str,
    ) -> Result<PreparedSquashMessage, SquashMessageError> {
        if !self.owns_operation(operation) {
            return Err(SquashMessageError::Model(
                "squash message requires an active operation for this session".into(),
            ));
        }
        let base_ref = tauqe_core::git::validate_revision(requested_ref)
            .map_err(|e| SquashMessageError::Invalid(e.into()))?;
        let repo_root = self.repository_root().await;

        let git_data = offload(move || {
            let commits = tauqe_core::git::get_commits_ahead(repo_root.as_path(), &base_ref);
            let diff = tauqe_core::git::get_cumulative_diff(repo_root.as_path(), &base_ref)
                .unwrap_or_default();
            (commits, diff)
        })
        .await;

        let (commits_ahead, cumulative_diff) = match git_data {
            Ok((Ok(commits), diff)) => (commits, diff),
            Ok((Err(err), _)) => {
                return Err(SquashMessageError::Commits(format!(
                    "Failed to get commits ahead of {}: {}",
                    requested_ref, err
                )));
            }
            Err(err) => return Err(SquashMessageError::Commits(err)),
        };

        let context = self.context_for_turn().await;
        let pinned_files = tauqe_core::execution::cache::pinned_context_files(&context)
            .await
            .map_err(|error| SquashMessageError::Model(error.to_string()))?;

        let prompt = tauqe_core::prompt::build_squash_commit_prompt(
            &commits_ahead,
            &cumulative_diff,
            &pinned_files,
        );

        Ok(PreparedSquashMessage {
            commits: commits_ahead,
            prompt,
        })
    }
    pub async fn generate_squash_message(
        &self,
        base_ref: &str,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: Duration,
    ) -> Result<SquashMessage, SquashMessageError> {
        let operation = self
            .try_begin_operation()
            .map_err(|_| SquashMessageError::Busy)?;
        let prepared = self.prepare_squash_message(&operation, base_ref).await?;
        let PreparedSquashMessage {
            commits: commits_ahead,
            prompt,
        } = prepared;
        let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
        let execution = environment
            .for_scribe_structured(self, &operation, &ModelSelection::Role(ModelRole::Scribe))
            .await;
        let (message, usage_snapshot) = if let Ok(execution) = execution {
            // The slot claimed at entry already owns this operation.
            // The identifier and the usage window exist before the first streamed
            // byte, so a cancellation delivered before the stream starts still
            // closes exactly one accounted operation.
            let op_id = format!("op-{}", crate::api::next_operation_id());
            let publisher = Arc::clone(&observer);
            let cancel = operation.hard_cancellation();
            let (result, receipt) = structured::run_bound(
                &self.execution_session(),
                self.context_session(),
                &operation,
                &execution,
                tauqe_core::prompt::squash_commit_contract(),
                vec![ChatMessage::user(&prompt)],
                &op_id,
                exit_deadline,
                move |id, snapshot| {
                    publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
                },
                |_| async {},
            )
            .await
            .map_err(|error| {
                SquashMessageError::Model(match error.classify("squash message") {
                    StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => {
                        message
                    }
                    _ => "Squash message generation was cancelled".into(),
                })
            })?;
            let snapshot = receipt.snapshot;
            observer.publish(&op_id, ApplicationEvent::Usage(snapshot.clone()));
            if cancel.is_cancelled() {
                return Err(SquashMessageError::Cancelled(
                    "Squash message generation was cancelled".into(),
                ));
            }
            let message = match result {
                Ok(message) => message,
                Err(error) => match error.classify("squash message") {
                    StructuredFailure::Empty => None,
                    StructuredFailure::InputLimit(error) => {
                        return Err(SquashMessageError::Model(error.to_string()));
                    }
                    StructuredFailure::Cancelled => {
                        return Err(SquashMessageError::Cancelled(
                            "Squash message generation was cancelled".into(),
                        ));
                    }
                    StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => {
                        return Err(SquashMessageError::Model(message));
                    }
                },
            };
            let msg = message.unwrap_or_else(|| {
                commits_ahead
                    .first()
                    .map(|c| c.subject.clone())
                    .unwrap_or_else(|| "Squashed commits".to_string())
            });
            (msg, Some(snapshot))
        } else {
            // Without a provider no usage window opens, so the zero operation
            // neither reaches the session totals nor increments the operation count.
            (
                commits_ahead
                    .first()
                    .map(|c| c.subject.clone())
                    .unwrap_or_else(|| "Squashed commits".to_string()),
                None,
            )
        };

        Ok(SquashMessage {
            message,
            usage: usage_snapshot,
        })
    }
}
#[derive(Debug, thiserror::Error)]
pub enum SquashApplyError {
    #[error("Cannot squash commits while a model operation is in progress")]
    Busy,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Commits(String),
    #[error("{0}")]
    Squash(String),
}
#[derive(Clone)]
pub struct AppliedSquash {
    pub commit: String,
    pub message: String,
    pub base_ref: String,
}
impl Application {
    pub async fn apply_squash(
        &self,
        base_ref: String,
        message: String,
        observer: &dyn ApplicationObserver,
    ) -> Result<AppliedSquash, SquashApplyError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| SquashApplyError::Busy)?;
        if message.trim().is_empty() {
            return Err(SquashApplyError::Invalid(
                "Commit message cannot be empty".into(),
            ));
        }
        let revision = tauqe_core::git::validate_revision(&base_ref)
            .map_err(|e| SquashApplyError::Invalid(e.into()))?;
        let root = self.repository_root().await;
        let requested = base_ref.clone();
        let commit_message = message.clone();
        let (count, hash) = offload(move || {
            let commits = tauqe_core::git::get_commits_ahead(&root, &revision).map_err(|e| {
                SquashApplyError::Commits(format!(
                    "Failed to get commits ahead of {}: {}",
                    requested, e
                ))
            })?;
            let hash = tauqe_core::git::squash_to_single_commit(&root, &revision, &commit_message)
                .map_err(|e| SquashApplyError::Squash(e.to_string()))?;
            Ok::<_, SquashApplyError>((commits.len(), hash))
        })
        .await
        .map_err(SquashApplyError::Squash)??;
        let root = self.repository_root().await;
        let repository = offload(move || tauqe_core::git::get_repository_state(Some(&root)))
            .await
            .map_err(SquashApplyError::Squash)?;
        let summary = message
            .lines()
            .next()
            .unwrap_or("Squash commits")
            .to_string();
        let history = format!(
            "Squashed {} commits ahead of {} into {}.",
            count, base_ref, hash
        );
        if let Err(error) = self
            .record_history_response(history, Some(hash.clone()), Some(summary), Vec::new())
            .await
        {
            tracing::warn!(error=%error,"Failed to record the squashed commit in the interaction history");
        }
        let applied = AppliedSquash {
            commit: hash,
            message,
            base_ref,
        };
        observer.publish("", ApplicationEvent::Repository(repository));
        observer.publish("", ApplicationEvent::SquashCompleted(applied.clone()));
        Ok(applied)
    }
}
