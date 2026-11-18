//! Headless locale synthesis, bounded shutdown, and accounting.
use crate::api::{Application, Awaited, OperationGuard, UsageGuard, UsageTotalsSnapshot};
use crate::config::AppConfig;
use crate::locale::{LocaleGenerateParams, LocaleGenerateResult};
use crate::models::{ModelRef, ModelRole};
use crate::providers::{ChatMessage, LlmProvider, StreamEvent};
use std::sync::Arc;
use std::time::Duration;

pub trait UsageObserver: Send + Sync {
    fn publish_usage_totals(&self, id: &str, snapshot: &UsageTotalsSnapshot);
    fn publish_usage(&self, id: &str, snapshot: &UsageTotalsSnapshot);
}
#[derive(Debug, thiserror::Error)]
pub enum LocaleGenerationError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    Model(String),
    #[error("{0}")]
    Cancelled(String),
}
pub struct PreparedLocaleGeneration {
    params: LocaleGenerateParams,
    config: AppConfig,
    model: ModelRef,
}
impl PreparedLocaleGeneration {
    pub fn provider(&self) -> Result<Arc<dyn LlmProvider>, LocaleGenerationError> {
        crate::providers::create_provider(self.model.provider, &self.config)
            .map(Arc::from)
            .map_err(|e| LocaleGenerationError::Provider(e.to_string()))
    }
}
impl Application {
    pub async fn prepare_locale_generation(
        &self,
        params: LocaleGenerateParams,
    ) -> Result<PreparedLocaleGeneration, LocaleGenerationError> {
        if params.language.trim().is_empty() {
            return Err(LocaleGenerationError::Invalid(
                "Target language cannot be empty".into(),
            ));
        }
        if params.items.is_empty() {
            return Err(LocaleGenerationError::Invalid(
                "No catalog items to translate".into(),
            ));
        }
        let config = self.configuration().await.config;
        let model = config.resolve_role(ModelRole::Scribe);
        Ok(PreparedLocaleGeneration {
            params,
            config,
            model,
        })
    }
    pub async fn generate_locale(
        &self,
        operation: &OperationGuard,
        prepared: PreparedLocaleGeneration,
        provider: Arc<dyn LlmProvider>,
        observer: Arc<dyn UsageObserver>,
        run: LocaleExecutionConfig,
    ) -> Result<LocaleGenerateResult, LocaleGenerationError> {
        if !self.owns_operation(operation) {
            return Err(LocaleGenerationError::Model(
                "locale generation requires an active operation for this session".into(),
            ));
        }
        let PreparedLocaleGeneration { params, model, .. } = prepared;
        let op_id = run.operation_id;
        let deadline = run.exit_deadline;
        let publisher = Arc::clone(&observer);
        let usage_guard = self
            .begin_usage(&op_id, move |id, snapshot| {
                publisher.publish_usage_totals(id, snapshot)
            })
            .await;
        // Locale synthesis never produces code changes, so it is a Structured
        // Output operation (see `crates/core/Conventions.md`, section 1.2): the
        // model answers with the enforced JSON schema instead of free-form text.
        let messages = vec![
            ChatMessage::system(crate::locale::build_locale_system_prompt()),
            ChatMessage::user(crate::locale::build_locale_user_prompt(&params)),
        ];
        let response_format = crate::locale::locale_response_format();

        let cancel_rx = operation.cancel_receiver();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let model_name = model.name.clone();
        let stream_cancel = cancel_rx.clone();
        let stream_task = tokio::spawn(async move {
            provider
                .stream_chat(
                    &model_name,
                    messages,
                    Some(response_format),
                    tx,
                    stream_cancel,
                )
                .await
        });
        // The streamed provider task is reachable only through the operation slot:
        // a hard cancel and the shutdown drain abort the call through this handle,
        // so a provider that ignores the cooperative signal cannot outlive the
        // request it belongs to.
        operation.set_abort_handle(stream_task.abort_handle());

        let mut raw_text = String::new();
        let mut stream_error = None;
        let mut cancelled = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => raw_text.push_str(&delta),
                // Usage is folded into the in-flight window as it arrives, so a
                // cancelled or failed call is charged for whatever it already
                // spent instead of losing the report when the loop breaks early.
                StreamEvent::Usage(reported) => usage_guard.observe(&reported).await,
                StreamEvent::Cancelled => {
                    cancelled = true;
                    break;
                }
                StreamEvent::Error(message) => {
                    stream_error = Some(message);
                    break;
                }
                _ => {}
            }
        }

        // The event loop has exited, so the slot must stop pointing at the streamed
        // task: the call is about to be awaited locally, and a hard cancel must not
        // abort a task that has already finished.
        operation.clear_abort_handle();

        // A hard cancel is delivered as a signal rather than as a stream event, so
        // the borrowed signal is consulted as well: an operation aborted through
        // its handle yields a join error, which must surface as a cancellation
        // instead of a model failure.
        if *cancel_rx.borrow() || cancelled {
            // The cancelled stream is reaped through the shared bounded task-exit
            // confirmation (`crate::api::abort_and_reap_with_deadline`, governed by the task-exit
            // deadline), so a provider call that never reaches a suspension point
            // cannot outlive the response.
            let _ = crate::api::abort_and_reap_with_deadline(
                &op_id,
                "locale-generation-stream",
                deadline,
                stream_task,
            )
            .await;
            close_locale_usage(self, usage_guard, &op_id, &observer).await;
            return Err(LocaleGenerationError::Cancelled(
                "Locale generation was cancelled".to_string(),
            ));
        }
        if let Some(message) = stream_error {
            // The stream reported a terminal error, but its task has not necessarily
            // returned yet: it is reaped through the shared bounded task-exit
            // confirmation (`crate::api::abort_and_reap_with_deadline`, governed by the task-exit
            // deadline) exactly as the cancellation branch above does, so no provider
            // call outlives the response after the operation slot has been released.
            let _ = crate::api::abort_and_reap_with_deadline(
                &op_id,
                "locale-generation-stream",
                deadline,
                stream_task,
            )
            .await;
            close_locale_usage(self, usage_guard, &op_id, &observer).await;
            return Err(LocaleGenerationError::Model(message));
        }
        // A completed stream is awaited through the shared bounded task-exit
        // confirmation (`crate::api::await_with_deadline`, governed by the task-exit
        // deadline): a task that never reaches a suspension point after its last event
        // is classified as a typed stream failure carrying the recorded diagnostic
        // instead of parking the request.
        match crate::api::await_with_deadline(
            &op_id,
            "locale-generation-stream",
            deadline,
            stream_task,
        )
        .await
        {
            Awaited::Joined(Ok(Ok(()))) => {}
            Awaited::Joined(Ok(Err(err))) => {
                close_locale_usage(self, usage_guard, &op_id, &observer).await;
                return Err(LocaleGenerationError::Model(err.to_string()));
            }
            Awaited::Joined(Err(join_err)) => {
                close_locale_usage(self, usage_guard, &op_id, &observer).await;
                return Err(LocaleGenerationError::Model(format!(
                    "Locale generation stream task failed: {}",
                    join_err
                )));
            }
            Awaited::Orphaned => {
                // The confirmation expired, so the abandoned task is still running: the
                // response carries the recorded expiry diagnostic instead of a result the
                // provider never delivered.
                close_locale_usage(self, usage_guard, &op_id, &observer).await;
                let message = format!(
                "Locale generation stream task did not stop within the {} ms task-exit deadline",
                deadline.as_millis()
            );
                return Err(LocaleGenerationError::Model(message));
            }
        }

        // The terminal flush folds the reported usage and the measured duration of
        // this one-shot call into the session totals, then mirrors the authoritative
        // numbers onto the legacy per-operation channel. The legacy fields are
        // projected from that snapshot, never from a separately maintained counter,
        // so the aggregate and legacy views can never disagree.
        close_locale_usage(self, usage_guard, &op_id, &observer).await;

        // Deterministic geometry validation and graceful English fallback live in
        // `core`: the daemon never trusts the model's own width arithmetic, so an
        // over-budget string is shortened or reverted to its English term and a
        // missing key transparently degrades to English (c-5).
        let result = crate::locale::validate_and_sanitize_locale(
            &params,
            &crate::locale::parse_locale_output(&raw_text),
        );
        Ok(result)
    }
}
pub struct LocaleExecutionConfig {
    pub operation_id: String,
    pub exit_deadline: Duration,
}
async fn close_locale_usage(
    application: &Application,
    guard: UsageGuard,
    id: &str,
    observer: &Arc<dyn UsageObserver>,
) {
    guard.flush().await;
    observer.publish_usage(id, &application.usage_snapshot().await);
}
