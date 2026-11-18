//! Plan execution scope validation before operation admission.
use crate::plan::{Plan, PlanScopeEvaluation};
use crate::{api::Application, plan::storage::PlanStorage};
#[derive(Debug, Clone, Copy)]
pub enum PlanExecutionFailure {
    NothingToExecute,
    NotFound,
    PlanLoadFailed,
    StepBlocked,
    StepBlockedByDependencies,
    StepCancelled,
    StepInDiscussion,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlanExecutionError {
    pub kind: PlanExecutionFailure,
    pub message: String,
}
async fn offload_plan<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}
impl Application {
    pub async fn prepare_plan_execution(
        &self,
        plan_id: &str,
        step_id: Option<&str>,
    ) -> Result<(Plan, Vec<String>), PlanExecutionError> {
        let repo_root = self.repository_root().await;
        let load_root = repo_root.clone();
        let load_plan_id = plan_id.to_string();
        let plan =
            match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Err(PlanExecutionError {
                        kind: PlanExecutionFailure::NotFound,
                        message: format!("Plan '{}' not found", plan_id),
                    });
                }
                Err(err) => {
                    return Err(PlanExecutionError {
                        kind: PlanExecutionFailure::PlanLoadFailed,
                        message: err,
                    })
                }
            };

        let steps = match plan.implementation.evaluate_scope(step_id) {
            PlanScopeEvaluation::Ready(steps) => steps,
            PlanScopeEvaluation::BlockedByDiscussion(items) => {
                return Err(PlanExecutionError { kind: PlanExecutionFailure::StepInDiscussion, message: format!(
                    "Scope contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) first",
                    items.join(", ")
                ) });
            }
            PlanScopeEvaluation::BlockedByBlockers(items) => {
                return Err(PlanExecutionError { kind: PlanExecutionFailure::StepBlocked, message: format!(
                    "Scope contains step(s) in BLOCKED status: {}. Resolve the reported blockers and replan before executing",
                    items.join(", ")
                ) });
            }
            PlanScopeEvaluation::BlockedByDependencies(items) => {
                return Err(PlanExecutionError { kind: PlanExecutionFailure::StepBlockedByDependencies, message: format!(
                    "Scope contains step(s) whose prerequisites are not yet DONE: {}. Complete the prerequisites first",
                    items.join(", ")
                ) });
            }
            PlanScopeEvaluation::StepBlocked => {
                return Err(PlanExecutionError { kind: PlanExecutionFailure::StepBlocked, message: ("Target step is BLOCKED; resolve the reported blocker and replan before executing").into() });
            }
            PlanScopeEvaluation::AllCompleted => {
                // A plan that carries an architectural concept but no synthesized
                // horizon has no leaf to schedule yet: the batch projects the first
                // horizon just-in-time, so the request is not rejected as a
                // completed scope.
                if plan.implementation.items.is_empty() && !plan.concept.is_empty() {
                    Vec::<crate::plan::PlanItem>::new()
                } else {
                    return Err(PlanExecutionError {
                        kind: PlanExecutionFailure::NothingToExecute,
                        message: ("All steps in this scope are already completed").into(),
                    });
                }
            }
            PlanScopeEvaluation::Cancelled => {
                return Err(PlanExecutionError {
                    kind: PlanExecutionFailure::StepCancelled,
                    message: ("Step is CANCELLED and cannot be executed").into(),
                });
            }
            PlanScopeEvaluation::NotFound => {
                return Err(PlanExecutionError {
                    kind: PlanExecutionFailure::NotFound,
                    message: ("Target item not found in plan").into(),
                });
            }
        };

        Ok((plan, steps.into_iter().map(|step| step.id).collect()))
    }
}
