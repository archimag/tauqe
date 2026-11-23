//! Prepare and settle an isolated attempt under its parent operation.
use super::completion::classify_step_edit_completion;
use super::StepAttempt;
use crate::api::{ApplicationObserver, OperationGuard};
use crate::application::context::ContextSession;
use crate::application::usage::UsageSession;
use crate::application::{BoundExecution, ExecutionSession, PlanExecutionEnvironment};
use crate::workflows::{
    round::RoundError,
    turn::{TurnDisk, TurnOutcome},
};
use std::path::PathBuf;
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::{CancellationToken, EventSink};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{ModelSelection, ReasoningEffort};
use tauqe_core::workflow::turn::{ModelTurnPlanContext, TurnWorkContext};

mod publication;
mod supervision;
pub(crate) use publication::StepRunPublication;

pub(crate) struct StepResources {
    pub context: ContextSession,
    pub usage: UsageSession,
    pub execution: ExecutionSession,
}

/// The disk receipt survives even when the task did not return an answer.
pub(crate) struct StepAttemptReport {
    pub outcome: Result<TurnOutcome, RoundError>,
    pub edit: Option<EditOutcome>,
    pub recovery_error: Option<String>,
    pub cancelled: bool,
}

pub(crate) enum StepLoopReport {
    Turn(Box<StepAttemptReport>),
    Failed(String),
}

/// Semantic inputs for an isolated step; the factory supplies technical collaborators.
pub(crate) struct SingleStepRequest {
    pub selection: Option<ModelSelection>,
    pub execution: PlanExecutionEnvironment,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub session_id: Option<String>,
    pub capabilities: CapabilityTree,
    pub task: String,
    pub context: ContextManager,
}
pub(crate) struct PreparedSingleStep {
    operation_id: u64,
    execution: BoundExecution,
    task: String,
    capabilities: CapabilityTree,
    root: PathBuf,
    context: ContextManager,
    cancel: CancellationToken,
}
#[derive(Debug, thiserror::Error)]
#[error("Step execution requires an active operation for this session")]
struct StepExecutionPreparationError;

impl StepResources {
    pub(crate) async fn prepare_single_step(
        &self,
        operation: &OperationGuard,
        request: SingleStepRequest,
    ) -> Result<PreparedSingleStep, String> {
        let execution = request
            .execution
            .bind_step(
                &self.execution,
                operation,
                request.selection,
                request.reasoning_effort,
                request.session_id,
                request.capabilities.clone(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let root = self.context.repository_root().await;
        if !self.execution.owns(operation) {
            return Err(StepExecutionPreparationError.to_string());
        }
        Ok(PreparedSingleStep {
            operation_id: operation.id(),
            execution,
            task: request.task,
            capabilities: request.capabilities,
            root,
            context: request.context,
            cancel: CancellationToken::from_receiver(operation.cancel_receiver()),
        })
    }

    /// Open an individual publication window; sequences open one around all attempts instead.
    pub(crate) async fn run_published_attempt(
        &self,
        operation: &OperationGuard,
        request: SingleStepRequest,
        plan_context: Option<ModelTurnPlanContext>,
        work_context: Option<TurnWorkContext>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> StepLoopReport {
        let session_label = request.session_id.clone();
        let prepared = match self.prepare_single_step(operation, request).await {
            Ok(prepared) => prepared,
            Err(error) => return StepLoopReport::Failed(error),
        };
        let model = &prepared.execution.model;
        self.publish_step_run(
            StepRunPublication {
                model: model.model.clone(),
                role: model.role,
                effort: model.effort,
                alias: model.alias.clone(),
                plan_context,
                work_context,
                session_label,
            },
            observer,
            |id, sink| async move {
                self.execute_prepared_single_step(operation, &id, prepared, sink)
                    .await
            },
            classify_step_edit_completion,
        )
        .await
    }

    pub(crate) async fn execute_prepared_single_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedSingleStep,
        sink: EventSink,
    ) -> StepLoopReport {
        if !self.execution.owns(operation)
            || prepared.operation_id != operation.id()
            || !operation.reserve_supervisor_release()
        {
            return StepLoopReport::Failed(StepExecutionPreparationError.to_string());
        }
        let PreparedSingleStep {
            execution,
            task,
            capabilities,
            root,
            context,
            cancel,
            ..
        } = prepared;
        let BoundExecution {
            model,
            prompt,
            policy,
        } = execution;
        let executor = prompt.into_executor(
            &model.model,
            model.effort,
            model.config.repl.patch_strategy(),
        );
        let (disk, worker) = TurnDisk::start(root.clone(), policy);
        let recovery_root = root.clone();
        let attempt = StepAttempt {
            root,
            task,
            context,
            capabilities,
            config: model.config,
            executor,
            cancel,
            events: sink.clone(),
        };
        self.supervise_step_attempt(
            operation,
            operation_id,
            recovery_root,
            worker,
            &sink,
            attempt.run(disk),
        )
        .await
    }
}

/// Compose the decoder, instructions, and schema from the contour's exact grant.
#[cfg(any(test, feature = "test-support"))]
pub fn turn_prompt_protocol(
    capabilities: CapabilityTree,
) -> Box<dyn tauqe_core::prompt::protocol::PromptProtocol> {
    Box::new(tauqe_core::prompt::json::JsonPromptProtocol::new(
        capabilities,
    ))
}

#[cfg(test)]
mod tests;
