use super::preparation::prepare_implementation_step;
use super::refinement::{prepare_implementation_refinement, PreparedImplementationRefinement};
use super::PlanResources;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::{
    self, publish_architect_started, ArchitectScope, PlanHorizonContour,
};
use crate::workflows::step::{
    completion::classify_step_attempt,
    run::{SingleStepRequest, StepLoopReport},
};
use std::{path::Path, sync::Arc};
use tauqe_core::config::AppConfig;
use tauqe_core::workflow::implementation::{
    classify_refinement, ExecutionScope, RefinementRequest, RefinementResult, StepFailure,
    StepTurnRequest, StepTurnResult, StepTurnSettlement,
};

/// Inputs shared by the coder attempts and Architect waves of one plan.
pub(super) struct PlanAttempts<'a> {
    pub(super) resources: &'a PlanResources,
    pub(super) observer: Arc<dyn ApplicationObserver>,
    pub(super) operation: &'a OperationGuard,
    pub(super) execution: &'a PlanExecutionEnvironment,
    pub(super) repo_root: &'a Path,
    pub(super) documents: &'a Documents,
}

impl PlanAttempts<'_> {
    pub(super) fn config(&self) -> &AppConfig {
        self.execution.config()
    }

    pub(super) async fn run_step(&self, request: StepTurnRequest) -> StepTurnSettlement {
        let prompt = match prepare_implementation_step(
            self.resources,
            self.operation,
            self.repo_root,
            self.config(),
            &self.execution.compaction_environment(),
            &request,
            |context| {
                self.observer
                    .publish("", ApplicationEvent::Context(context))
            },
        )
        .await
        {
            Ok(prompt) => prompt,
            Err(error) => return error.into_step_result().into(),
        };
        let StepTurnRequest {
            scope,
            title,
            queue,
            selection,
            reasoning_effort,
            session_id,
            cancel_rx,
            ..
        } = request;
        let plan_context = scope.plan_context(&queue);
        let work_context = Some(scope.work_context(&title, queue));

        // The shared turn settles the disk transaction before the implementation
        // pipeline interprets the attempt or schedules another coder.
        let report = self
            .resources
            .step
            .run_published_attempt(
                self.operation,
                SingleStepRequest {
                    selection: Some(selection),
                    reasoning_effort,
                    session_id,
                    execution: self.execution.clone(),
                    capabilities: tauqe_core::llm::step::turn_capability_expr(),
                    task: prompt,
                    context: self.resources.step.context.context_for_turn().await,
                },
                plan_context,
                work_context,
                self.observer.clone(),
            )
            .await;

        let cancelled = *cancel_rx.borrow();
        match report {
            StepLoopReport::Turn(mut result) => {
                result.cancelled |= cancelled;
                // A recovery diagnostic means restoration was not confirmed.
                // Keep this separate from retryable content failures.
                if let Some(reason) = result.recovery_error.take().filter(|_| !result.cancelled) {
                    return StepTurnSettlement {
                        result: StepTurnResult::Failed(StepFailure::unreverted(reason)),
                        edit: result.edit,
                    };
                }
                classify_step_attempt(&result)
            }
            _ if cancelled => StepTurnResult::Cancelled.into(),
            StepLoopReport::Failed(reason) => {
                StepTurnResult::Failed(StepFailure::infrastructure(reason)).into()
            }
        }
    }

    pub(super) async fn run_refinement(&self, request: RefinementRequest) -> RefinementResult {
        let prepared = match prepare_implementation_refinement(
            self.documents,
            self.execution.models(),
            request
                .selection
                .as_ref()
                .map(|selection| self.execution.resolve_architect_model(selection)),
            &request,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return RefinementResult::Blocked {
                    reason: error.reason,
                    tier: error.tier,
                }
            }
        };
        let PreparedImplementationRefinement {
            tier,
            task,
            intro_header,
            budget_hint,
        } = prepared;
        let op_id = self.operation.publication_id();
        let ExecutionScope::Plan { plan_id, .. } = &request.scope else {
            return RefinementResult::Blocked {
                reason: "The plan execution host can only refine plans".into(),
                tier,
            };
        };
        let scope = ArchitectScope::Plan(plan_id.clone());

        publish_architect_started(self.observer.as_ref(), &op_id, &tier, true);
        let step = &self.resources.step;
        let context = step.context.context_for_turn().await;
        let (contour, summary) = architect::run_contour(
            &step.execution,
            &step.context,
            &step.usage,
            self.operation,
            self.execution,
            PlanHorizonContour {
                operation_id: op_id.clone(),
                task,
                context,
                scope,
                tier: tier.clone(),
                intro_header,
                budget_hint,
            },
            self.observer.clone(),
        )
        .await;
        if !summary.cancelled {
            self.observer
                .publish(&op_id, ApplicationEvent::Finished(summary.assistant_text));
        }

        let outcome = match contour {
            Ok(outcome) => outcome,
            Err(err) => {
                return RefinementResult::Blocked {
                    reason: summary.error.unwrap_or_else(|| err.to_string()),
                    tier,
                }
            }
        };

        classify_refinement(outcome)
    }
}
