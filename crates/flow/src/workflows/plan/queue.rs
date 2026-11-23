//! Run admitted plans in order under the same operation.

use super::run::run_batch;
use super::PlanResources;
use crate::api::{ApplicationObserver, BatchOutcome, OperationGuard, PlanBatchOptions};
use crate::application::PlanExecutionEnvironment;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;
use tauqe_core::workflow::batch::QueueInterruption;

pub(crate) async fn run_pipeline(
    resources: &PlanResources,
    execution: &PlanExecutionEnvironment,
    plan_ids: Vec<String>,
    model: Option<ModelSelection>,
    operation: &OperationGuard,
    observer: Arc<dyn ApplicationObserver>,
) {
    let options = PlanBatchOptions {
        model,
        step_id: None,
    };
    let cancel = operation.cancel_receiver();
    let soft_stop = operation.soft_stop_receiver();
    for plan_id in plan_ids {
        if QueueInterruption::before_next(*cancel.borrow(), Some(&soft_stop)).is_some() {
            break;
        }
        let outcome = run_batch(
            resources,
            execution,
            &plan_id,
            operation,
            &options,
            observer.clone(),
        )
        .await;
        if !matches!(outcome, BatchOutcome::Completed { .. }) {
            break;
        }
    }
}
