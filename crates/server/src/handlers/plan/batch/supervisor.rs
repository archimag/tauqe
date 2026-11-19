use std::sync::Arc;

use crate::state::{AppState, OperationGuard};
use tauqe_flow::api::{BatchOutcome, PlanBatchOptions};

pub(in crate::handlers::plan) async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    _step_ids: Vec<String>,
    operation: Arc<OperationGuard>,
    options: PlanBatchOptions,
) -> BatchOutcome {
    state
        .application
        .test_run_plan_batch(plan_id, &operation, &options, super::observer(state))
        .await
}
