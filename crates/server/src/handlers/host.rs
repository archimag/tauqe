//! Server-side implementation host.
//!
//! The engine's unified implementation machinery
//! (`tauqe_core::workflow::implementation`) owns *what* a horizon does: which
//! leaf runs, on which coder rung, with which prompt, and when a failure
//! justifies an Architect wave. This module owns *how* one of those turns is
//! executed inside the daemon: preparation notifications, the provider turn with its
//! streamed telemetry and usage window, and forwarding the core's classification
//! of the turn result.
//!
//! One implementation serves every workflow that owns a step graph, so a plan
//! batch and a review run cannot drift apart in how they reach a provider, how
//! they publish telemetry, or how they forward a blocked, aborted, cancelled,
//! or unverified turn.

use crate::contracts::Project;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::AppConfig;
use tauqe_core::models::ModelRole;
use tauqe_core::workflow::implementation::{
    classify_refinement, classify_step_turn, ExecutionScope, ImplementationHost, RefinementRequest,
    RefinementResult, StepFailure, StepTurnRequest, StepTurnResult,
};
use tauqe_protocol::{events, ModelFinishedEvent, ModelStartedEvent};

use crate::handlers::architect::run_contour_synthesis;
use crate::handlers::common::{emit, prepare_implementation_step};
use crate::handlers::model::turn::{run_step_loop, StepLoopBody, StepLoopOptions, StepLoopReport};
use crate::handlers::review::publish_review;
use crate::state::{AppState, OperationGuard};

/// Turn seam between the engine's implementation machinery and the daemon.
///
/// The host borrows the operation that owns the whole run, so the batch slot
/// never lapses between members and a single cancellation reaches every step and
/// every wave of the run through one channel.
pub struct ServerImplementationHost {
    state: Arc<AppState>,
    operation: Arc<OperationGuard>,
    /// Configuration snapshot of the run, taken when the batch started.
    config: AppConfig,
    repo_root: PathBuf,
}

impl ServerImplementationHost {
    /// Creates the host of one implementation run.
    pub fn new(
        state: Arc<AppState>,
        operation: Arc<OperationGuard>,
        config: AppConfig,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            state,
            operation,
            config,
            repo_root,
        }
    }
}

#[async_trait]
impl ImplementationHost for ServerImplementationHost {
    async fn run_step(&self, request: StepTurnRequest) -> StepTurnResult {
        let prompt = match prepare_implementation_step(
            &self.state,
            &self.operation,
            &self.repo_root,
            &self.config,
            &request,
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
            publish_review(&self.state, &self.repo_root, review_id).await;
        }

        // The step turn runs through the control contour: the engine's bounded
        // turn drives research, staging, verification, and healing, while the
        // apply policy of the run gates the Git transaction of the step.
        let report = run_step_loop(
            &self.state,
            StepLoopOptions {
                selection: Some(selection),
                reasoning_effort,
                marker,
                session_id,
                operation: Arc::clone(&self.operation),
                plan_context,
                work_context,
                body: StepLoopBody::Single(prompt),
            },
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
            .state
            .application
            .prepare_implementation_refinement(&self.repo_root, &self.config, &request)
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
        let tauqe_core::api::PreparedImplementationRefinement {
            tier,
            options,
            task,
            intro_header,
            budget_hint,
        } = prepared;
        let op_id = format!("op-{}", self.operation.id());

        emit(
            &self.state.out,
            events::MODEL_STARTED,
            serde_json::to_value(ModelStartedEvent {
                operation_id: op_id.clone(),
                model: tier.model.clone().project(),
                role: Some(ModelRole::Architect.project()),
                alias: None,
                effort: tier.effort.map(crate::contracts::Project::project),
                plan_context: None,
                work_context: None,
                is_batch: true,
            })
            .ok(),
        );

        let context_snapshot = self.state.context_snapshot().await;
        let (contour, summary) = run_contour_synthesis(
            &self.state,
            &self.operation,
            &op_id,
            task,
            context_snapshot,
            options,
            self.config.models.clone(),
            tier.clone(),
            intro_header,
            budget_hint,
        )
        .await;

        if !summary.cancelled {
            emit(
                &self.state.out,
                events::MODEL_FINISHED,
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: op_id,
                    full_text: summary.assistant_text.clone(),
                })
                .ok(),
            );
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
