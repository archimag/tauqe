//! Shared provider invocation and bounded task lifetime for review workflows.

use crate::api::{Application, OperationGuard};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::models::ReasoningEffort;
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tokio::sync::mpsc;

#[derive(Clone)]
pub(crate) struct ProviderRequest {
    pub model_name: String,
    pub messages: Vec<ChatMessage>,
    pub response_format: ResponseFormat,
    pub effort: Option<ReasoningEffort>,
    pub session_id: String,
}

#[derive(Debug, thiserror::Error)]
#[error("provider execution requires an active operation for this session")]
pub struct ProviderTaskError;

pub struct ProviderTask<'a> {
    operation: &'a OperationGuard,
    task: tokio::task::JoinHandle<()>,
    events: mpsc::Receiver<StreamEvent>,
    task_name: &'static str,
}

impl Application {
    pub(crate) fn start_provider_task<'a>(
        &self,
        operation: &'a OperationGuard,
        request: ProviderRequest,
        provider: Arc<dyn LlmProvider>,
        task_name: &'static str,
    ) -> Result<ProviderTask<'a>, ProviderTaskError> {
        if !self.owns_operation(operation) {
            return Err(ProviderTaskError);
        }
        let (tx, events) = mpsc::channel(100);
        let err_tx = tx.clone();
        let cancel = operation.cancel_receiver();
        let task = tokio::spawn(async move {
            if let Err(error) = provider
                .stream(
                    tauqe_core::providers::LlmRequest {
                        model: &request.model_name,
                        messages: request.messages,
                        response_format: Some(request.response_format),
                        reasoning_effort: request.effort,
                        session_id: Some(request.session_id.as_str()),
                    },
                    tx,
                    cancel,
                )
                .await
            {
                let _ = err_tx.send(StreamEvent::Error(error.to_string())).await;
            }
        });
        operation.set_abort_handle(task.abort_handle());
        Ok(ProviderTask {
            operation,
            task,
            events,
            task_name,
        })
    }
}

impl ProviderTask<'_> {
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        self.events.recv().await
    }
    /// Release registration before accounting and persistence.
    pub fn clear_abort_handle(&self) {
        self.operation.clear_abort_handle();
    }
    /// Abort once and confirm exit under the caller's resolved deadline.
    pub async fn abort_and_reap(
        self,
        operation_id: &str,
        deadline: Duration,
    ) -> crate::api::TaskExit {
        crate::api::abort_and_reap_with_deadline(operation_id, self.task_name, deadline, self.task)
            .await
    }
}
