//! Prepare and settle an isolated attempt under its parent operation.
use super::completion::classify_step_edit_completion;
use super::StepAttempt;
use crate::api::{ApplicationObserver, OperationGuard};
use crate::application::context::ContextSession;
use crate::application::execution_scope::ExecutionScope;
use crate::application::usage_totals::UsageReceipt;
use crate::application::{BoundExecution, ExecutionSession, PlanExecutionEnvironment};
use crate::workflows::turn::{RoundError, StepRecovery, TechnicalObservation, TurnOutcome};
use std::path::PathBuf;
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::executor::PromptIdentity;
use tauqe_core::execution::usage::UsageBinding;
use tauqe_core::execution::{EventSink, ReadyPromptExecutor};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{ModelSelection, ReasoningEffort};
use tauqe_core::workflow::turn::{ModelTurnPlanContext, TurnWorkContext};
use tauqe_core::workflow::WorkflowOptions;

mod publication;
mod supervision;
pub(crate) use publication::StepRunPublication;

pub(crate) struct StepResources {
    pub context: ContextSession,
    pub execution: ExecutionSession,
}

/// Final policy facts survive even when the task did not return an answer.
pub(crate) struct StepAttemptReport {
    pub outcome: Result<TurnOutcome, RoundError>,
    pub edit: Option<EditOutcome>,
    pub recovery: StepRecovery,
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
    executor: ReadyPromptExecutor,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
    max_turn_rounds: usize,
    task: String,
    capabilities: CapabilityTree,
    root: PathBuf,
    context: ContextManager,
}
impl PreparedSingleStep {
    pub(crate) fn identity(&self) -> &PromptIdentity {
        self.executor.identity()
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Step execution requires an active operation for this session")]
struct StepExecutionPreparationError;

impl StepResources {
    pub(crate) async fn prepare_single_step_with_usage(
        &self,
        operation: &OperationGuard,
        request: SingleStepRequest,
        usage: UsageBinding,
    ) -> Result<PreparedSingleStep, String> {
        let root = self.context.repository_root().await;
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
        let BoundExecution {
            model,
            prompt,
            policy,
        } = execution;
        let prompt = prompt.with_usage(usage);
        let max_turn_rounds = WorkflowOptions::from_app_config(&model.config).max_turn_rounds();
        let executor = prompt
            .bind_ready_executor(
                &root,
                &model.config,
                Some(&operation.hard_cancellation()),
                PromptIdentity::new(
                    model.model.clone(),
                    model.effort,
                    model.role,
                    model.alias.clone(),
                ),
                model.config.repl.patch_strategy(),
            )
            .await
            .map_err(|error| error.to_string())?;
        if !self.execution.owns(operation) {
            return Err(StepExecutionPreparationError.to_string());
        }
        Ok(PreparedSingleStep {
            operation_id: operation.id(),
            executor,
            policy,
            max_turn_rounds,
            task: request.task,
            capabilities: request.capabilities,
            root,
            context: request.context,
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
    ) -> (StepLoopReport, Option<UsageReceipt>) {
        let session_label = request.session_id.clone();
        let scope = self.execution.prepare_scope();
        let prepared = match self
            .prepare_single_step_with_usage(operation, request, scope.binding())
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => return (StepLoopReport::Failed(error), None),
        };
        self.publish_step_run(
            StepRunPublication {
                identity: prepared.identity().clone(),
                plan_context,
                work_context,
                session_label,
            },
            observer.clone(),
            scope,
            |id, sink| async move {
                self.execute_observed_step(operation, &id, prepared, sink, observer)
                    .await
            },
            classify_step_edit_completion,
        )
        .await
        .map(|(report, receipt)| (report, Some(receipt)))
        .unwrap_or_else(|error| (StepLoopReport::Failed(error), None))
    }

    pub(crate) fn prepare_usage_scope(&self) -> ExecutionScope {
        self.execution.prepare_scope()
    }

    pub(crate) async fn execute_observed_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedSingleStep,
        sink: EventSink,
        observer: Arc<dyn ApplicationObserver>,
    ) -> StepLoopReport {
        self.execute_step(operation, operation_id, prepared, sink, Some(observer))
            .await
    }

    async fn execute_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedSingleStep,
        sink: EventSink,
        observer: Option<Arc<dyn ApplicationObserver>>,
    ) -> StepLoopReport {
        if !self.execution.owns(operation)
            || prepared.operation_id != operation.id()
            || !operation.reserve_supervisor_release()
        {
            return StepLoopReport::Failed(StepExecutionPreparationError.to_string());
        }
        let PreparedSingleStep {
            executor,
            policy,
            max_turn_rounds,
            task,
            capabilities,
            root,
            mut context,
            ..
        } = prepared;
        let observation = if let Some(observer) = observer {
            let Some(listener) = self.context.activate_research_publication(
                operation,
                operation_id.to_owned(),
                observer,
                &context,
            ) else {
                return StepLoopReport::Failed("Could not bind step context publication".into());
            };
            context.set_listener(Some(listener));
            Some(TechnicalObservation {
                session: self.context.clone(),
                operation_id: operation.id(),
            })
        } else {
            None
        };
        let attempt = StepAttempt {
            root,
            task,
            context,
            capabilities,
            max_turn_rounds,
            executor,
            events: sink.clone(),
            observation,
        };
        self.supervise_step_attempt(operation, operation_id, &sink, attempt, policy)
            .await
    }
}

#[cfg(test)]
impl StepResources {
    pub(crate) async fn execute_prepared_single_step(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        prepared: PreparedSingleStep,
        sink: EventSink,
    ) -> StepLoopReport {
        self.execute_step(operation, operation_id, prepared, sink, None)
            .await
    }

    pub(crate) async fn prepare_single_step(
        &self,
        operation: &OperationGuard,
        request: SingleStepRequest,
    ) -> Result<PreparedSingleStep, String> {
        struct RecordingUsage(std::sync::Mutex<Vec<tauqe_core::providers::ModelUsageInfo>>);
        impl tauqe_core::execution::usage::UsageSink for RecordingUsage {
            fn record(&self, report: &tauqe_core::providers::ModelUsageInfo) {
                self.0.lock().unwrap().push(report.clone());
            }
        }
        let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(RecordingUsage(
            std::sync::Mutex::new(Vec::new()),
        )));
        scope.activate();
        self.prepare_single_step_with_usage(operation, request, UsageBinding::Bound(scope))
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
