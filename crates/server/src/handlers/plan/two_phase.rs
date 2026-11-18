//! RPC projection for core-owned independent plan synthesis.
use crate::contracts::Project;
use crate::handlers::common::emit;
use crate::state::{AppState, OperationGuard};
use std::sync::Arc;
pub(super) use tauqe_core::api::TwoPhaseRequest;
use tauqe_core::api::{
    ContourRelaySummary, PlanHorizonContour, PlanHorizonObserver, TwoPhaseObserver,
};
use tauqe_core::context::ContextState;
use tauqe_core::control::refine::StructuredRefineOutcome;
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::Plan;
use tauqe_protocol::{
    errors, events, ModelErrorEvent, PlanCreatePipelineParams, PlanListChangedEvent,
    PlanUpdatedEvent, Request, RequestId, Response, TurnPhase, TurnPhaseEvent,
};
/// Handles `plan/createPipeline`: the two-phase creation of a new plan.
///
/// The directives are the only coordinate a creation carries, so the plan
/// identity is derived from them deterministically and the issued document is
/// written by the pipeline instead of being supplied by the caller.
pub async fn handle_plan_create_pipeline(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanCreatePipelineParams = match req.params {
        Some(value) => match serde_json::from_value::<PlanCreatePipelineParams>(value) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan create pipeline parameters: {err}"),
                )
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan create pipeline parameters",
            )
        }
    };

    let directives = params.directives.trim().to_string();
    if directives.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Plan creation directives cannot be empty",
        );
    }

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };

    let plan = match state.application.prepare_plan_creation(&directives).await {
        Ok(plan) => plan,
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
    };

    tracing::info!(
        plan_id = %plan.id,
        critique = params.critic_model.is_some() && !params.skip_critique,
        "plan creation pipeline requested"
    );

    execute_two_phase_plan_synthesis(
        req.id,
        state,
        TwoPhaseRequest {
            plan,
            directives: Some(directives),
            planner_model: params.planner_model.map(crate::contracts::Project::project),
            critic_model: params.critic_model.map(crate::contracts::Project::project),
            skip_critique: params.skip_critique,
            creation: true,
            operation_title: "Plan Creation",
        },
        &operation,
    )
    .await
}

pub(super) async fn execute_two_phase_plan_synthesis(
    req_id: RequestId,
    state: &Arc<AppState>,
    request: TwoPhaseRequest,
    operation: &OperationGuard,
) -> Response {
    let observer = Arc::new(Observer {
        shared: super::engine::Observer {
            state: state.clone(),
            root: state.repo_root().await,
            is_batch: false,
        },
    });
    match state
        .application
        .execute_two_phase_plan(
            operation,
            request,
            observer,
            crate::reap::task_exit_deadline(),
        )
        .await
    {
        Ok(result) => Response::ok_typed(req_id, &crate::contracts::Project::project(result)),
        Err(error) => Response::err(req_id, super::engine::error_code(error.kind), error.message),
    }
}
struct Observer {
    shared: super::engine::Observer,
}
#[async_trait::async_trait]
impl PlanHorizonObserver for Observer {
    fn context_changed(&self, state: ContextState) {
        self.shared.context_changed(state);
    }
    fn started(&self, id: &str, tier: &ArchitectTier) {
        self.shared.started(id, tier);
    }
    fn finished(&self, id: &str, text: String) {
        self.shared.finished(id, text);
    }
    async fn run(
        &self,
        operation: &OperationGuard,
        work: PlanHorizonContour,
    ) -> (Result<StructuredRefineOutcome, String>, ContourRelaySummary) {
        self.shared.run(operation, work).await
    }
    async fn publish(&self, plan: &Plan) {
        self.shared.publish(plan).await;
    }
}
#[async_trait::async_trait]
impl TwoPhaseObserver for Observer {
    async fn critic_provider(
        &self,
        config: &tauqe_core::config::AppConfig,
        provider: tauqe_core::models::ProviderKind,
    ) -> Result<Arc<dyn tauqe_core::providers::LlmProvider>, String> {
        crate::handlers::architect::resolve_architect_provider(&self.shared.state, config, provider)
            .await
    }
    fn usage_totals(&self, id: &str, snapshot: &tauqe_core::api::UsageTotalsSnapshot) {
        crate::usage::publish_snapshot(&self.shared.state.out, id, snapshot);
    }
    fn critique_round(&self, id: &str, round: usize, budget: usize) {
        emit(
            &self.shared.state.out,
            events::TURN_PHASE,
            serde_json::to_value(TurnPhaseEvent {
                operation_id: id.into(),
                phase: TurnPhase::Proposal,
                round: Some(round),
                max_rounds: Some(budget),
                detail: Some("Independent plan critique".into()),
            })
            .ok(),
        );
    }
    fn cancelled(&self, id: &str) {
        emit(
            &self.shared.state.out,
            events::MODEL_CANCELLED,
            Some(serde_json::json!({"operation_id": id})),
        );
    }
    fn failed(&self, id: &str, message: &str) {
        emit(
            &self.shared.state.out,
            events::MODEL_ERROR,
            serde_json::to_value(ModelErrorEvent {
                operation_id: id.into(),
                message: message.into(),
            })
            .ok(),
        );
    }
    async fn publish_issued(&self, plan: &Plan, creation: bool) {
        emit(
            &self.shared.state.out,
            events::PLAN_UPDATED,
            serde_json::to_value(PlanUpdatedEvent {
                plan: plan.clone().project(),
                is_save: creation,
            })
            .ok(),
        );
        let root = self.shared.root.clone();
        if let Ok(plans) = self
            .shared
            .state
            .application
            .documents_at(&root)
            .plans()
            .await
        {
            let root = self.shared.root.clone();
            let active_id = self
                .shared
                .state
                .application
                .documents_at(&root)
                .active_plan_id()
                .await
                .ok()
                .flatten();
            emit(
                &self.shared.state.out,
                events::PLAN_LIST_CHANGED,
                serde_json::to_value(PlanListChangedEvent {
                    plans: plans
                        .into_iter()
                        .map(crate::contracts::Project::project)
                        .collect(),
                    active_id,
                })
                .ok(),
            );
        }
    }
}
