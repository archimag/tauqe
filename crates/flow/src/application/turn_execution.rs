//! Assemble bounded execution without transport-owned policies or loop closures.
use crate::api::{
    Application, InteractiveTask, InteractiveTaskError, InteractiveTaskOutcome, OperationGuard,
};
use crate::application::infrastructure::execution::{
    DialogueExecution, EditExecution, ExecutionFactory, PlanExecutionEnvironment,
};
use crate::application::step_dispatch::StepLoopReport;
use std::{path::PathBuf, sync::Arc};
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::outcome::{FailureReport, LoopResult, SystemError};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::history::HistorySnapshot;
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{ModelSelection, ReasoningEffort};
use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol};
use tauqe_core::providers::StreamEvent;
use tokio::sync::mpsc;

/// Compose the decoder, instructions, and schema from the contour's exact grant.
pub fn turn_prompt_protocol(capabilities: CapabilityTree) -> Box<dyn PromptProtocol> {
    Box::new(JsonPromptProtocol::new(capabilities))
}
pub struct InteractiveExecutionRequest {
    pub capabilities: CapabilityTree,
    pub prompt: String,
    pub focus: Option<DiscussionTarget>,
}
pub struct StartedInteractiveExecution {
    pub task: InteractiveTask,
    pub events: mpsc::UnboundedReceiver<StreamEvent>,
    pub turn_id: Option<u64>,
}
/// Semantic inputs for an isolated step; the factory supplies technical collaborators.
pub(crate) struct SingleStepRequest {
    pub selection: Option<ModelSelection>,
    pub execution: Option<PlanExecutionEnvironment>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub session_id: Option<String>,
    pub capabilities: CapabilityTree,
    pub history: Option<HistorySnapshot>,
    pub task: String,
    pub context: ContextManager,
}
pub(crate) struct PreparedSingleStep {
    operation_id: u64,
    pub(crate) execution: EditExecution,
    task: String,
    history: Option<HistorySnapshot>,
    root: PathBuf,
    context: ContextManager,
    cancel: CancellationToken,
}
#[derive(Debug, thiserror::Error)]
#[error("Step execution requires an active operation for this session")]
pub struct StepExecutionPreparationError;
impl Application {
    pub(crate) async fn prepare_single_step(
        &self,
        operation: &OperationGuard,
        request: SingleStepRequest,
    ) -> Result<PreparedSingleStep, String> {
        let execution = match request.execution {
            Some(environment) => {
                environment
                    .for_edit_unit(
                        self,
                        operation,
                        request.selection,
                        request.reasoning_effort,
                        request.session_id,
                        request.capabilities,
                    )
                    .await
            }
            None => {
                let factory = ExecutionFactory::bind(self, operation)
                    .await
                    .map_err(|error| error.to_string())?;
                factory
                    .for_edit_unit(
                        request.selection,
                        request.reasoning_effort,
                        request.session_id,
                        request.capabilities,
                    )
                    .await
            }
        }
        .map_err(|error| error.to_string())?;
        let root = self.repository_root().await;
        if !self.owns_operation(operation) {
            return Err(StepExecutionPreparationError.to_string());
        }
        Ok(PreparedSingleStep {
            operation_id: operation.id(),
            execution,
            task: request.task,
            history: request.history,
            root,
            context: request.context,
            cancel: CancellationToken::from_receiver(operation.cancel_receiver()),
        })
    }

    pub(crate) async fn execute_prepared_single_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedSingleStep,
        sink: EventSink,
    ) -> StepLoopReport {
        if !self.owns_operation(operation) || prepared.operation_id != operation.id() {
            return StepLoopReport::Failed(StepExecutionPreparationError.to_string());
        }
        self.run_step_task(operation, operation_id, async move {
            let PreparedSingleStep {
                operation_id: _,
                mut execution,
                task,
                history,
                root,
                mut context,
                cancel,
            } = prepared;
            let report = execution
                .run(&root, &mut context, history.as_ref(), &cancel, &sink, task)
                .await;
            let report = match report {
                Ok(result) => StepLoopReport::Turn(result),
                Err(SystemError::RoundBudgetExhausted { spent, .. }) => {
                    StepLoopReport::Turn(LoopResult::Exhausted(FailureReport::new(
                        "Turn round budget exhausted",
                        spent,
                        Vec::new(),
                    )))
                }
                Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
                Err(error) => StepLoopReport::Failed(error.to_string()),
            };
            if matches!(&report, StepLoopReport::Turn(LoopResult::Success(success))
                if !success.value.modified_paths.is_empty() && success.value.commit_error.is_none())
            {
                tauqe_core::blocking::run_blocking(|| context.clear_transient());
                sink.emit(StreamEvent::ContextChanged(context.get_state()));
            }
            (report, context)
        })
        .await
    }

    pub(crate) async fn start_interactive_execution(
        &self,
        operation: Arc<OperationGuard>,
        request: InteractiveExecutionRequest,
        mut dialogue: DialogueExecution,
    ) -> Result<StartedInteractiveExecution, InteractiveTaskError> {
        if !self.owns_operation(&operation) {
            return Err(InteractiveTaskError::OperationNotOwned);
        }
        let (sink, events) = EventSink::channel();
        let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
        let root = self.repository_root().await;
        let history = self
            .prepare_interactive_history(&operation, request.prompt.clone())
            .await
            .unwrap_or_default();
        let turn_id = history.turn_id;
        let task = self.start_interactive_task(operation, move |mut context| async move {
            let result = crate::application::interactive_dialogue::run(
                &mut dialogue,
                &root,
                &mut context,
                &cancel,
                &sink,
                request,
                history.history.as_ref(),
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
}

#[cfg(test)]
mod tests;
