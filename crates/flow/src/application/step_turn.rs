use crate::api::{Application, ApplicationObserver, OperationGuard};
use crate::application::infrastructure::step_run::StepRunPublication;
use crate::application::step_dispatch::{StepLoopBody, StepLoopReport};
use crate::application::turn_execution::SingleStepRequest;
use std::sync::Arc;
use tauqe_core::models::ReasoningEffort;
use tauqe_core::workflow::turn::TurnWorkContext;

/// Options of one step-endpoint execution through the control loops.
pub struct StepLoopOptions {
    /// Explicit model selection, or the active selection of the session.
    pub selection: Option<tauqe_core::models::ModelSelection>,
    /// Reasoning depth of the run.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
    /// Provider session identifier shared by every turn of the run.
    pub session_id: Option<String>,
    /// Registration of the batch that owns the run.
    pub operation: Arc<OperationGuard>,
    /// Frozen technical environment shared across a plan run, if present.
    pub(crate) execution:
        Option<crate::application::infrastructure::execution::PlanExecutionEnvironment>,
    /// Plan coordinates published with `model/started`.
    pub plan_context: Option<tauqe_core::workflow::turn::ModelTurnPlanContext>,
    /// Work the run executes, published with `model/started`.
    pub work_context: Option<TurnWorkContext>,
    /// Body the run executes.
    pub body: StepLoopBody,
}

/// Runs one step or sequence using its parent operation and settles its telemetry.
impl Application {
    pub(crate) async fn run_step_loop(
        &self,
        options: StepLoopOptions,
        observer: Arc<dyn ApplicationObserver>,
    ) -> StepLoopReport {
        let StepLoopOptions {
            selection,
            reasoning_effort,
            marker: _marker,
            session_id,
            operation,
            execution,
            plan_context,
            work_context,
            body,
        } = options;

        let session_label = session_id.clone();
        let prepared = match body {
            StepLoopBody::Single(task) => {
                let context = self.context_for_turn().await;
                match self
                    .prepare_single_step(
                        &operation,
                        SingleStepRequest {
                            selection,
                            execution,
                            reasoning_effort,
                            session_id,
                            capabilities: tauqe_core::control::turn::turn_capability_expr(),
                            history: None,
                            task,
                            context,
                        },
                    )
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(error) => return StepLoopReport::Failed(error),
                }
            }
        };
        let resolved = &prepared.execution.model;
        self.run_prepared_step(
            StepRunPublication {
                model: resolved.model.clone(),
                role: resolved.role,
                effort: resolved.effort,
                alias: resolved.alias.clone(),
                plan_context,
                work_context,
                session_label,
            },
            observer,
            |op_id, sink| async move {
                // run_step_task remains the sole owner of task supervision,
                // recovery, and adoption of the returned context.
                self.execute_prepared_single_step(&operation, &op_id, prepared, sink.clone())
                    .await
            },
        )
        .await
    }
}
