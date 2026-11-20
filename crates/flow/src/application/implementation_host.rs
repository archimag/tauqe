use crate::api::{
    Application, OperationGuard, PlanHorizonContour, PreparedImplementationRefinement,
    StepLoopOptions,
};
use crate::api::{ApplicationEvent, ApplicationObserver};
use crate::application::architect::publish_architect_started;
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::plan_horizon::ArchitectScope;
use crate::application::step_dispatch::{StepLoopBody, StepLoopReport};
use async_trait::async_trait;
use std::{path::PathBuf, sync::Arc};
use tauqe_core::config::AppConfig;
use tauqe_core::workflow::implementation::{
    classify_refinement, classify_step_turn, ExecutionScope, RefinementRequest, RefinementResult,
    StepFailure, StepTurnRequest, StepTurnResult,
};

/// Model execution seam of the implementation workflow. Durable state is owned
/// directly by Application, not by a model-result substitute.
#[async_trait]
pub(crate) trait ImplementationHost: Send + Sync {
    async fn run_step(&self, request: StepTurnRequest) -> StepTurnResult;
    async fn run_refinement(&self, request: RefinementRequest) -> RefinementResult;
}

/// Turn seam between the application workflow and its execution machinery.
///
/// The host borrows the operation that owns the whole run, so the batch slot
/// never lapses between members and a single cancellation reaches every step and
/// every wave of the run through one channel.
pub(crate) struct ApplicationImplementationHost {
    application: Application,
    observer: Arc<dyn ApplicationObserver>,
    operation: Arc<OperationGuard>,
    /// Configuration snapshot of the run, taken when the batch started.
    config: AppConfig,
    execution: PlanExecutionEnvironment,
    repo_root: PathBuf,
}

impl ApplicationImplementationHost {
    /// Creates the host of one implementation run.
    pub fn new(
        application: Application,
        observer: Arc<dyn ApplicationObserver>,
        operation: Arc<OperationGuard>,
        config: AppConfig,
        execution: PlanExecutionEnvironment,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            application,
            observer,
            operation,
            config,
            execution,
            repo_root,
        }
    }
}

#[async_trait]
impl ImplementationHost for ApplicationImplementationHost {
    async fn run_step(&self, request: StepTurnRequest) -> StepTurnResult {
        let prompt = match self
            .application
            .prepare_implementation_step(
                &self.operation,
                &self.repo_root,
                &self.config,
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
            Err(error) => return error.into_step_result(),
        };
        let StepTurnRequest {
            scope,
            title,
            queue,
            selection,
            reasoning_effort,
            marker,
            session_id,
            cancel_rx,
            ..
        } = request;
        let plan_context = scope.plan_context(&queue);
        let work_context = Some(scope.work_context(&title, queue));

        // The step executor already persisted the lifecycle of the reviewed entity
        // before it asked for this turn, so a review scope is published here: the
        // client observes the finding or the group the run has taken over instead of
        // the `TODO` it held when the batch was launched. A plan scope carries no
        // publication of its own, because the plan supervisor owns it.
        if let ExecutionScope::ReviewGroup { review_id, .. }
        | ExecutionScope::ReviewFinding { review_id, .. } = &scope
        {
            self.application
                .publish_review_snapshot(&self.repo_root, review_id, self.observer.as_ref())
                .await;
        }

        // The step turn runs through the control contour: the engine's bounded
        // turn drives research, staging, verification, and healing, while the
        // apply policy of the run gates the Git transaction of the step.
        let report = self
            .application
            .run_step_loop(
                StepLoopOptions {
                    selection: Some(selection),
                    reasoning_effort,
                    marker,
                    session_id,
                    operation: Arc::clone(&self.operation),
                    execution: Some(self.execution.clone()),
                    plan_context,
                    work_context,
                    body: StepLoopBody::Single(prompt),
                },
                self.observer.clone(),
            )
            .await;

        if *cancel_rx.borrow() {
            return StepTurnResult::Cancelled;
        }
        match report {
            StepLoopReport::Cancelled => StepTurnResult::Cancelled,
            StepLoopReport::Failed(reason) => {
                StepTurnResult::Failed(StepFailure::infrastructure(reason))
            }
            StepLoopReport::Sequence(_) => StepTurnResult::Failed(StepFailure::infrastructure(
                "A single step turn reported a sequence outcome",
            )),
            StepLoopReport::Turn(result) => classify_step_turn(result),
        }
    }

    async fn run_refinement(&self, request: RefinementRequest) -> RefinementResult {
        let prepared = match self
            .application
            .prepare_implementation_refinement(
                &self.repo_root,
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
        let scope = match &request.scope {
            ExecutionScope::Plan { plan_id, .. } => ArchitectScope::Plan(plan_id.clone()),
            ExecutionScope::ReviewGroup { review_id, .. }
            | ExecutionScope::ReviewFinding { review_id, .. } => {
                ArchitectScope::Review(review_id.clone())
            }
        };

        publish_architect_started(self.observer.as_ref(), &op_id, &tier, true);
        let context = self.application.context_for_turn().await;
        let (contour, summary) = self
            .application
            .run_architect_contour_with_environment(
                &self.operation,
                &self.execution,
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
