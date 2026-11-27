//! Typed application events and transport readiness.
use crate::api::{
    DiscussionFocus, InteractiveTurnCompletion, RepositoryState, UsageTotalsSnapshot,
};
use crate::application::edit_cycle::EditCycleEvent;
use tauqe_core::context::ContextState;
use tauqe_core::models::{ModelRef, ModelRole, ReasoningEffort};
use tauqe_core::providers::StreamEvent;
use tauqe_core::stop::HardCancellation;
use tauqe_core::workflow::turn::{ModelTurnPlanContext, TurnWorkContext};

pub enum ApplicationEvent {
    ConfigurationChanged(Box<crate::api::ConfigurationView>),
    ModelChanged {
        selection: tauqe_core::models::ModelSelection,
        model: ModelRef,
        roles: Box<tauqe_core::models::ModelRolesSummary>,
    },
    Focus(DiscussionFocus),
    Started {
        model: ModelRef,
        role: Option<ModelRole>,
        alias: Option<String>,
        effort: Option<ReasoningEffort>,
        plan_context: Option<ModelTurnPlanContext>,
        work_context: Option<TurnWorkContext>,
        is_batch: bool,
    },
    Stream(StreamEvent),
    Edit(EditCycleEvent),
    UsageTotals(UsageTotalsSnapshot),
    Usage(UsageTotalsSnapshot),
    Commit {
        hash: String,
        summary: String,
        changed_files: Vec<String>,
    },
    Repository(RepositoryState),
    SquashCompleted(crate::api::AppliedSquash),
    Context(ContextState),
    PlanDocument {
        plan: tauqe_core::plan::Plan,
        is_save: bool,
    },
    PlanList {
        plans: Vec<tauqe_core::plan::Plan>,
        active_id: Option<String>,
    },
    PlanBatchFinished(crate::api::PlanBatchFinished),
    ReviewBatchFinished(crate::api::ExecutedReviewBatch),
    ReviewRun(crate::api::ReviewEvent),
    ReviewDocument(tauqe_core::review::ReviewSession),
    ReviewList {
        reviews: Vec<tauqe_core::review::ReviewSession>,
        active_id: Option<String>,
    },
    Result {
        completion: Box<InteractiveTurnCompletion>,
        usage: UsageTotalsSnapshot,
    },
    Finished(String),
}

#[async_trait::async_trait]
pub trait ApplicationObserver: Send + Sync + 'static {
    fn publish(&self, operation_id: &str, event: ApplicationEvent);

    async fn ready(&self, _event: &StreamEvent, _cancel: &HardCancellation) {}
}
