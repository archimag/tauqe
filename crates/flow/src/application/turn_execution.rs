//! Assemble bounded execution without transport-owned policies or loop closures.
use crate::api::{
    Application, InteractiveTask, InteractiveTaskError, InteractiveTaskOutcome, OperationGuard,
};
use crate::application::step_dispatch::{StepLoopBody, StepLoopReport};
use std::{path::PathBuf, sync::Arc};
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{ModelRef, ModelSelection, ReasoningEffort};
use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol};
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_core::workflow::policy::ApplyPolicy;
use tokio::sync::mpsc;

/// Compose the decoder, instructions, and schema from the contour's exact grant.
pub fn turn_prompt_protocol(capabilities: CapabilityTree) -> Box<dyn PromptProtocol> {
    Box::new(JsonPromptProtocol::new(capabilities))
}
pub struct InteractiveExecutionRequest {
    pub provider: Arc<dyn LlmProvider>,
    pub model: ModelRef,
    pub config: AppConfig,
    pub capabilities: CapabilityTree,
    pub prompt: String,
    pub focus: Option<DiscussionTarget>,
    pub session_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}
pub struct StartedInteractiveExecution {
    pub task: InteractiveTask,
    pub events: mpsc::UnboundedReceiver<StreamEvent>,
    pub turn_id: Option<u64>,
}
pub struct StepExecutionRequest {
    pub provider: Arc<dyn LlmProvider>,
    pub model: ModelRef,
    pub config: AppConfig,
    pub selection: ModelSelection,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub marker: Option<String>,
    pub session_id: Option<String>,
    pub body: StepLoopBody,
}
/// Frozen engine collaborators captured before usage/relay setup. Fields stay private.
pub struct PreparedStepExecution {
    operation_id: u64,
    request: StepExecutionRequest,
    root: PathBuf,
    context: ContextManager,
    policy: Box<dyn ApplyPolicy>,
    protocol: Box<dyn PromptProtocol>,
    cancel: CancellationToken,
}
#[derive(Debug, thiserror::Error)]
#[error("Step execution requires an active operation for this session")]
pub struct StepExecutionPreparationError;
impl Application {
    pub(crate) async fn start_interactive_execution(
        &self,
        operation: Arc<OperationGuard>,
        request: InteractiveExecutionRequest,
    ) -> Result<StartedInteractiveExecution, InteractiveTaskError> {
        if !self.owns_operation(&operation) {
            return Err(InteractiveTaskError::OperationNotOwned);
        }
        let protocol = turn_prompt_protocol(request.capabilities.clone());
        debug_assert_eq!(protocol.capability_expr(), request.capabilities);
        let (sink, events) = EventSink::channel();
        let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
        let root = self.repository_root().await;
        let history = self
            .prepare_interactive_history(&operation, request.prompt.clone())
            .await
            .unwrap_or_default();
        let mut policy =
            self.apply_policy(request.config.repl.workflow, operation.cancel_receiver());
        let turn_id = history.turn_id;
        let task = self.start_interactive_task(operation, move |mut context| async move {
            let result = tauqe_core::control::repl::run_repl_execution(
                &mut context,
                policy.as_mut(),
                &cancel,
                &sink,
                tauqe_core::control::repl::ReplExecutionConfig {
                    turn: tauqe_core::control::execution::TurnExecutionConfig {
                        repo_root: &root,
                        provider: request.provider.as_ref(),
                        model: &request.model,
                        protocol: protocol.as_ref(),
                        config: &request.config,
                        session_id: request.session_id,
                        reasoning_effort: request.reasoning_effort,
                    },
                    prompt: request.prompt,
                    history: history.history.as_ref(),
                    focus: request.focus.as_ref(),
                },
            )
            .await;
            InteractiveTaskOutcome { result, context }
        })?;
        Ok(StartedInteractiveExecution {
            task,
            events,
            turn_id,
        })
    }

    pub(crate) async fn prepare_step_execution(
        &self,
        operation: &OperationGuard,
        request: StepExecutionRequest,
    ) -> Result<PreparedStepExecution, StepExecutionPreparationError> {
        if !self.owns_operation(operation) {
            return Err(StepExecutionPreparationError);
        }
        let protocol = turn_prompt_protocol(tauqe_core::control::turn::turn_capability_expr());
        debug_assert_eq!(
            protocol.capability_expr(),
            tauqe_core::control::turn::turn_capability_expr()
        );
        let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
        let policy = self.apply_policy(request.config.repl.workflow, operation.cancel_receiver());
        let root = self.repository_root().await;
        let context = self.context_for_turn().await;
        Ok(PreparedStepExecution {
            operation_id: operation.id(),
            request,
            root,
            context,
            policy,
            protocol,
            cancel,
        })
    }

    pub(crate) async fn execute_prepared_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedStepExecution,
        sink: EventSink,
    ) -> StepLoopReport {
        if !self.owns_operation(operation) || prepared.operation_id != operation.id() {
            return StepLoopReport::Failed(StepExecutionPreparationError.to_string());
        }
        self.run_step_task(operation, operation_id, async move {
            let PreparedStepExecution {
                operation_id: _,
                request,
                root,
                mut context,
                mut policy,
                protocol,
                cancel,
            } = prepared;
            let report = crate::application::step_dispatch::dispatch_step_execution(
                &mut context,
                policy.as_mut(),
                &cancel,
                &sink,
                crate::application::step_dispatch::StepExecutionConfig {
                    turn: tauqe_core::control::execution::TurnExecutionConfig {
                        repo_root: &root,
                        provider: request.provider.as_ref(),
                        model: &request.model,
                        protocol: protocol.as_ref(),
                        config: &request.config,
                        reasoning_effort: request.reasoning_effort,
                        session_id: request.session_id,
                    },
                    selection: &request.selection,
                    marker: request.marker,
                },
                request.body,
            )
            .await;
            (report, context)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
