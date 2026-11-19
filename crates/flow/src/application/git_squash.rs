//! Scribe squash-message generation, Git rewrite, and history policy.
use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, Awaited, OperationGuard, UsageGuard,
    UsageTotalsSnapshot,
};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::config::AppConfig;
use tauqe_core::git::CommitSummary;
use tauqe_core::models::{ModelRef, ModelRole};
use tauqe_core::providers::{ChatMessage, StreamEvent};
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
    config: AppConfig,
    model: ModelRef,
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

        let (ctx_snapshot, pinned_paths) = {
            let ctx = self.context_for_turn().await;
            let state_ctx = ctx.get_state();
            let pinned_paths: std::collections::HashSet<String> = state_ctx
                .items
                .into_iter()
                .filter(|i| i.layer == tauqe_core::context::ContextLayer::Pinned)
                .map(|i| i.path)
                .collect();
            (ctx.clone(), pinned_paths)
        };

        let pinned_files = offload(move || {
            let all_files = ctx_snapshot.read_context_files();
            all_files
                .into_iter()
                .filter(|f| pinned_paths.contains(&f.path))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let prompt = tauqe_core::prompt::build_squash_commit_prompt(
            &commits_ahead,
            &cumulative_diff,
            &pinned_files,
        );

        let config = self.configuration().await.config;
        let model = config.resolve_role(ModelRole::Scribe);
        Ok(PreparedSquashMessage {
            commits: commits_ahead,
            prompt,
            config,
            model,
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
            config,
            model,
        } = prepared;
        let provider = self.resolve_provider(model.provider, &config).await.ok();
        let (message, usage_snapshot) = if let Some(provider) = provider {
            // The slot claimed at entry already owns this operation.
            // The identifier and the usage window exist before the first streamed
            // byte, so a cancellation delivered before the stream starts still
            // closes exactly one accounted operation.
            let op_id = format!("op-{}", crate::api::next_operation_id());
            let publisher = Arc::clone(&observer);
            let usage_guard = self
                .begin_usage(&op_id, move |id, snapshot| {
                    publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
                })
                .await;
            let cancel_rx = operation.cancel_receiver();
            // The signal is also observed after the event loop, so a provider that
            // stops streaming on cancellation without reporting it is still
            // recognized as a cancelled turn.
            let cancel_observer = cancel_rx.clone();
            let (tx, mut rx) = tokio::sync::mpsc::channel(50);
            let msgs = vec![ChatMessage::user(&prompt)];
            let model_name = model.name.clone();
            let response_format = tauqe_core::prompt::squash_commit_response_format();
            let stream_task = tokio::spawn(async move {
                provider
                    .stream(
                        tauqe_core::providers::LlmRequest {
                            response_format: Some(response_format),
                            ..tauqe_core::providers::LlmRequest::new(&model_name, msgs)
                        },
                        tx,
                        cancel_rx,
                    )
                    .await
            });
            // The handle lets the last-resort watchdog interrupt a provider request
            // that never observes the cancellation signal.
            operation.set_abort_handle(stream_task.abort_handle());
            let mut full_text = String::new();
            let mut stream_error: Option<String> = None;
            let mut cancelled = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::TextDelta(delta) => {
                        full_text.push_str(&delta);
                    }
                    // Providers may report usage on intermediate chunks. It stays
                    // in the in-flight window and is folded into the session totals
                    // only by the single terminal flush below, so an intermediate
                    // report can never be counted twice.
                    StreamEvent::Usage(reported) => {
                        usage_guard.observe(&reported).await;
                    }
                    StreamEvent::Error(msg) => {
                        stream_error = Some(msg);
                        break;
                    }
                    // An explicit provider cancellation is reported as `CANCELLED`
                    // instead of being swallowed by the catch-all arm and surfacing
                    // later as a generic internal failure.
                    StreamEvent::Cancelled => {
                        cancelled = true;
                        break;
                    }
                    _ => {}
                }
            }

            // The signal itself is authoritative: a provider may end the stream on
            // cancellation without ever reporting a cancellation event.
            let cancelled = cancelled || *cancel_observer.borrow();

            // Surface both a cancelled turn and a streamed provider error so a
            // broken or interrupted provider never masquerades as a successful
            // fallback message. The abandoned request is reaped through the shared
            // bounded task-exit confirmation (`crate::api::abort_and_reap_with_deadline`, governed by
            // the task-exit deadline) before the outcome is reported, so no provider call
            // outlives the response and a task that never reaches a suspension point
            // cannot park the handler.
            if cancelled || stream_error.is_some() {
                let _ = crate::api::abort_and_reap_with_deadline(
                    &op_id,
                    "squash-message-stream",
                    exit_deadline,
                    stream_task,
                )
                .await;
                operation.clear_abort_handle();
                close_squash_usage(self, usage_guard, &op_id, &observer).await;
                let (code, message) = if cancelled {
                    (true, "Squash message generation was cancelled".to_string())
                } else {
                    (
                        false,
                        stream_error.unwrap_or_else(|| "Squash message stream failed".to_string()),
                    )
                };
                return Err(if code {
                    SquashMessageError::Cancelled(message)
                } else {
                    SquashMessageError::Model(message)
                });
            }

            // A completed stream is awaited through the shared bounded task-exit
            // confirmation (`crate::api::await_with_deadline`, governed by the task-exit
            // deadline).
            let stream_result = crate::api::await_with_deadline(
                &op_id,
                "squash-message-stream",
                exit_deadline,
                stream_task,
            )
            .await;
            operation.clear_abort_handle();
            match stream_result {
                Awaited::Joined(Ok(Ok(()))) => {}
                Awaited::Joined(Ok(Err(err))) => {
                    close_squash_usage(self, usage_guard, &op_id, &observer).await;
                    return Err(SquashMessageError::Model(err.to_string()));
                }
                Awaited::Joined(Err(join_err)) => {
                    close_squash_usage(self, usage_guard, &op_id, &observer).await;
                    return Err(SquashMessageError::Model(format!(
                        "Squash message stream task failed: {}",
                        join_err
                    )));
                }
                Awaited::Orphaned => {
                    // The confirmation expired, so the abandoned task is still running:
                    // the response carries the recorded expiry diagnostic instead of a
                    // result the provider never delivered.
                    close_squash_usage(self, usage_guard, &op_id, &observer).await;
                    let message = format!(
                    "Squash message stream task did not stop within the {} ms task-exit deadline",
                    exit_deadline.as_millis()
                );
                    return Err(SquashMessageError::Model(message));
                }
            }
            let msg =
                tauqe_core::prompt::parse_squash_commit_message(&full_text).unwrap_or_else(|| {
                    commits_ahead
                        .first()
                        .map(|c| c.subject.clone())
                        .unwrap_or_else(|| "Squashed commits".to_string())
                });
            let snapshot = close_squash_usage(self, usage_guard, &op_id, &observer).await;
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
async fn close_squash_usage(
    app: &Application,
    guard: UsageGuard,
    id: &str,
    observer: &Arc<dyn ApplicationObserver>,
) -> UsageTotalsSnapshot {
    guard.flush().await;
    let snapshot = app.usage_snapshot().await;
    observer.publish(id, ApplicationEvent::Usage(snapshot.clone()));
    snapshot
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
