use super::preparation::prepare_implementation_step;
use super::refinement::{prepare_implementation_refinement, PreparedImplementationRefinement};
use super::PlanResources;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::documents::Documents;
use crate::application::usage_totals::{ReceiptAccumulator, UsageReceipt};
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::{
    self, publish_architect_started, ArchitectScope, HorizonVerdict, PlanHorizonContour,
};
use crate::workflows::step::{
    completion::{classify_step_attempt, step_answer, CoderAttempt},
    run::{SingleStepRequest, StepLoopReport},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tauqe_core::config::AppConfig;
use tauqe_core::workflow::implementation::{
    ExecutionScope, RefinementRequest, RefinementResult, StepFailure, StepTurnRequest,
    StepTurnResult,
};

/// Inputs shared by the coder attempts and Architect waves of one plan.
pub(super) struct PlanAttempts<'a> {
    pub(super) resources: &'a PlanResources,
    pub(super) observer: Arc<dyn ApplicationObserver>,
    pub(super) operation: &'a OperationGuard,
    pub(super) execution: &'a PlanExecutionEnvironment,
    pub(super) repo_root: &'a Path,
    pub(super) documents: &'a Documents,
    pub(super) receipts: Mutex<ReceiptAccumulator>,
}

impl PlanAttempts<'_> {
    pub(super) fn record_receipt(&self, receipt: &UsageReceipt) {
        self.receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .include(receipt);
    }

    pub(super) fn batch_usage(&self) -> tauqe_core::workflow::batch::BatchUsage {
        self.receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage()
    }

    pub(super) fn config(&self) -> &AppConfig {
        self.execution.config()
    }

    pub(super) async fn run_step(&self, request: StepTurnRequest) -> CoderAttempt {
        let prompt = match prepare_implementation_step(
            self.resources,
            self.operation,
            self.repo_root,
            self.config(),
            &request,
        )
        .await
        {
            Ok(prompt) => prompt,
            Err(error) => {
                return CoderAttempt {
                    settlement: error.into_step_result().into(),
                    answer: String::new(),
                };
            }
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
        let (report, receipt) = self
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
        self.restore_shared_publication();
        if let Some(receipt) = &receipt {
            self.record_receipt(receipt);
        }

        let cancelled = cancel_rx.is_cancelled();
        match report {
            StepLoopReport::Turn(mut result) => {
                result.cancelled |= cancelled;
                CoderAttempt {
                    answer: step_answer(&result),
                    settlement: classify_step_attempt(&result),
                }
            }
            _ if cancelled => CoderAttempt {
                settlement: StepTurnResult::Cancelled.into(),
                answer: String::new(),
            },
            StepLoopReport::Failed(reason) => CoderAttempt {
                settlement: StepTurnResult::Failed(StepFailure::infrastructure(reason)).into(),
                answer: String::new(),
            },
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
        let (contour, summary) = architect::run_horizon_wave(
            &step.execution,
            &step.context,
            self.operation,
            self.execution,
            PlanHorizonContour {
                operation_id: op_id.clone(),
                task,
                context,
                scope,
                tier: tier.clone(),
                intro_header,
            },
            self.observer.clone(),
        )
        .await;
        self.restore_shared_publication();
        if let Some(receipt) = &summary.receipt {
            self.record_receipt(receipt);
        }
        if !matches!(
            contour,
            HorizonVerdict::Cancelled | HorizonVerdict::Failed(_)
        ) {
            self.observer
                .publish(&op_id, ApplicationEvent::Finished(summary.assistant_text));
        }

        match contour {
            HorizonVerdict::Synthesized { output, tier } => RefinementResult::Synthesized {
                items: output.items,
                tier,
            },
            HorizonVerdict::Blocked { reason, tier } => RefinementResult::Blocked { reason, tier },
            HorizonVerdict::Aborted { report, tier } => RefinementResult::Blocked {
                reason: report.to_message(),
                tier,
            },
            HorizonVerdict::EmptyHorizon { tier } => RefinementResult::Blocked {
                reason: "The Architect reported a synthesized horizon without any step".into(),
                tier,
            },
            HorizonVerdict::InvalidTree { duplicate, tier } => RefinementResult::Blocked {
                reason: format!("duplicate or empty step ID: '{duplicate}'"),
                tier,
            },
            HorizonVerdict::Cancelled => RefinementResult::Blocked {
                reason: "The Architect wave was cancelled".into(),
                tier,
            },
            HorizonVerdict::Failed(reason) => RefinementResult::Blocked { reason, tier },
        }
    }

    fn restore_shared_publication(&self) {
        // A step or Architect contour replaces the shared source with its own
        // detached generation and closes it on settlement. Resume publication
        // from the shared manager for the gaps and subsequent preloads.
        let _ = self
            .resources
            .step
            .context
            .activate_manual_publication(self.operation, self.observer.clone());
    }
}
