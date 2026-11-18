//! RPC projection for core-owned Architect horizon synthesis.
pub(super) use super::horizon::PlanHorizonDiagnosticsOwned;
use crate::contracts::Project;
use crate::handlers::architect::run_contour_synthesis;
use crate::handlers::common::emit;
use crate::state::{AppState, OperationGuard};
use std::sync::Arc;
#[cfg(test)]
pub(super) use tauqe_core::api::collect_plan_context_files;
use tauqe_core::api::{
    ContourRelaySummary, PlanHorizonContour, PlanHorizonFailure, PlanHorizonObserver,
    PlanHorizonRequest,
};
use tauqe_core::context::ContextState;
use tauqe_core::control::refine::StructuredRefineOutcome;
use tauqe_core::models::{ArchitectTier, ModelRole, ModelSelection};
use tauqe_core::plan::Plan;
use tauqe_protocol::{
    errors, events, ModelFinishedEvent, ModelStartedEvent, PlanListChangedEvent, PlanUpdatedEvent,
    Response,
};
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_plan_horizon_synthesis(
    req_id: tauqe_protocol::RequestId,
    state: &Arc<AppState>,
    plan: Plan,
    diagnostics: Option<PlanHorizonDiagnosticsOwned>,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    operation_title: &str,
    is_batch: bool,
    operation: &OperationGuard,
) -> Response {
    let observer = Arc::new(Observer {
        state: Arc::clone(state),
        root: state.repo_root().await,
        is_batch,
    });
    match state
        .application
        .execute_plan_horizon(
            operation,
            PlanHorizonRequest {
                plan,
                diagnostics,
                model: custom_model,
                instructions,
                operation_title: operation_title.into(),
            },
            observer,
        )
        .await
    {
        Ok(result) => Response::ok_typed(req_id, &crate::contracts::Project::project(result)),
        Err(error) => {
            let code = error_code(error.kind);
            Response::err(req_id, code, error.message)
        }
    }
}
pub(super) struct Observer {
    pub(super) state: Arc<AppState>,
    pub(super) root: std::path::PathBuf,
    pub(super) is_batch: bool,
}
#[async_trait::async_trait]
impl PlanHorizonObserver for Observer {
    fn context_changed(&self, state: ContextState) {
        crate::handlers::context::publish_context(&self.state.out, &state);
    }
    fn started(&self, id: &str, tier: &ArchitectTier) {
        emit(
            &self.state.out,
            events::MODEL_STARTED,
            serde_json::to_value(ModelStartedEvent {
                operation_id: id.into(),
                model: tier.model.clone().project(),
                role: Some(ModelRole::Architect.project()),
                alias: None,
                effort: tier.effort.map(crate::contracts::Project::project),
                plan_context: None,
                work_context: None,
                is_batch: self.is_batch,
            })
            .ok(),
        );
    }
    fn finished(&self, id: &str, text: String) {
        emit(
            &self.state.out,
            events::MODEL_FINISHED,
            serde_json::to_value(ModelFinishedEvent {
                operation_id: id.into(),
                full_text: text,
            })
            .ok(),
        );
    }
    async fn run(
        &self,
        operation: &OperationGuard,
        work: PlanHorizonContour,
    ) -> (Result<StructuredRefineOutcome, String>, ContourRelaySummary) {
        let (result, summary) = run_contour_synthesis(
            &self.state,
            operation,
            &work.operation_id,
            work.task,
            work.context,
            work.options,
            work.models_config,
            work.tier,
            work.intro_header,
            work.budget_hint,
        )
        .await;
        (result.map_err(|e| e.to_string()), summary)
    }
    async fn publish(&self, updated_plan: &Plan) {
        emit(
            &self.state.out,
            events::PLAN_UPDATED,
            serde_json::to_value(PlanUpdatedEvent {
                plan: updated_plan.clone().project(),
                is_save: false,
            })
            .ok(),
        );

        let list_root = self.root.clone();
        if let Ok(plans) = self
            .state
            .application
            .documents_at(&list_root)
            .plans()
            .await
        {
            let active_root = self.root.clone();
            let active_id = self
                .state
                .application
                .documents_at(&active_root)
                .active_plan_id()
                .await
                .ok()
                .flatten();
            emit(
                &self.state.out,
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

pub(super) fn error_code(kind: PlanHorizonFailure) -> &'static str {
    match kind {
        PlanHorizonFailure::Cancelled => errors::CANCELLED,
        PlanHorizonFailure::Model => errors::MODEL_ERROR,
        PlanHorizonFailure::EmptyItems => errors::EMPTY_ITEMS,
        PlanHorizonFailure::InvalidTree => errors::INVALID_PLAN_TREE,
        PlanHorizonFailure::Save => errors::PLAN_SAVE_FAILED,
    }
}
