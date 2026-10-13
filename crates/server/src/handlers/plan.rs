use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    create_provider, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStop,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, Event, ModelRole, ModelSelection, Plan, PlanDeleteParams,
    PlanExecuteParams, PlanGetParams, PlanGetResult, PlanItem, PlanItemStatus, PlanListChangedEvent,
    PlanListResult, PlanRefineParams, PlanRefineResult, PlanSaveParams, PlanScopeEvaluation,
    PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent, Request, Response, TurnPhase,
    TurnPhaseEvent,
};
use tokio::sync::{mpsc, watch};

use crate::state::{ActiveOperation, AppState};
use crate::transport::OutChannel;

fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

pub async fn handle_plan_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::load_all(&repo_root) {
        Ok(plans) => {
            Response::ok_typed(req.id, &PlanListResult { plans, active_id: None })
        }
        Err(err) => Response::err(req.id, "PLAN_LIST_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_plan_get(req: Request, state: &Arc<AppState>) -> Response {
    let target_id = req.params.and_then(|p| {
        if let Ok(params) = serde_json::from_value::<PlanGetParams>(p.clone()) {
            if !params.id.trim().is_empty() {
                return Some(params.id);
            }
        }
        if let Ok(val) = serde_json::from_value::<serde_json::Value>(p) {
            if let Some(id_str) = val.get("id").and_then(|v| v.as_str()) {
                if !id_str.trim().is_empty() {
                    return Some(id_str.to_string());
                }
            }
        }
        None
    });

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let plan_id = match &target_id {
        Some(id) => Some(id.clone()),
        None => PlanStorage::load_active_id(&repo_root)
            .ok()
            .flatten()
            .or_else(|| {
                PlanStorage::list_plans(&repo_root)
                    .ok()
                    .and_then(|plans| plans.into_iter().next().map(|p| p.id))
            }),
    };

    if target_id.is_none() {
        if let Ok(None) = PlanStorage::load_active_id(&repo_root) {
            if let Some(ref id) = plan_id {
                let _ = PlanStorage::save_active_id(&repo_root, Some(id.as_str()));
            }
        }
    }

    match plan_id {
        Some(id) => match PlanStorage::load_plan(&repo_root, &id) {
            Ok(plan) => Response::ok_typed(req.id, &PlanGetResult { plan }),
            Err(err) => Response::err(req.id, "PLAN_GET_FAILED", format!("{:#}", err)),
        },
        None => Response::ok_typed(req.id, &PlanGetResult { plan: None }),
    }
}

pub async fn handle_plan_save(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan payload"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_plan(&repo_root, &params.plan) {
        return Response::err(req.id, "PLAN_SAVE_FAILED", format!("{:#}", err));
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: params.plan.clone(),
        })
        .ok(),
    );

    if let Ok(plans) = PlanStorage::load_all(&repo_root) {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent { plans, active_id: None }).ok(),
        );
    }

    Response::ok(req.id, serde_json::json!({ "saved": true }))
}

pub async fn handle_plan_update_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanUpdateItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid update item params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::update_item(
        &repo_root,
        &params.plan_id,
        &params.item_id,
        params.status,
    ) {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone(),
                })
                .ok(),
            );
            if let Ok(plans) = PlanStorage::load_all(&repo_root) {
                emit(
                    &state.out,
                    events::PLAN_LIST_CHANGED,
                    serde_json::to_value(PlanListChangedEvent { plans, active_id: None }).ok(),
                );
            }
            Response::ok(req.id, serde_json::json!({ "plan": plan }))
        }
        Ok(None) => Response::err(
            req.id,
            "NOT_FOUND",
            format!("Item '{}' not found in plan '{}'", params.item_id, params.plan_id),
        ),
        Err(err) => Response::err(req.id, "PLAN_UPDATE_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_plan_delete(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan ID"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::delete_plan(&repo_root, &params.id) {
        Ok(deleted) => {
            if deleted {
                if let Ok(plans) = PlanStorage::load_all(&repo_root) {
                    emit(
                        &state.out,
                        events::PLAN_LIST_CHANGED,
                        serde_json::to_value(PlanListChangedEvent { plans, active_id: None }).ok(),
                    );
                }
            }
            Response::ok(req.id, serde_json::json!({ "deleted": deleted }))
        }
        Err(err) => Response::err(req.id, "PLAN_DELETE_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_plan_set_active(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanSetActiveParams = match req.params {
        Some(p) => serde_json::from_value(p).unwrap_or_default(),
        None => PlanSetActiveParams::default(),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_active_id(&repo_root, params.id.as_deref()) {
        return Response::err(req.id, "PLAN_SET_ACTIVE_FAILED", format!("{:#}", err));
    }

    if let Ok(plans) = PlanStorage::load_all(&repo_root) {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans,
                active_id: params.id.clone(),
            })
            .ok(),
        );
    }

    Response::ok(req.id, serde_json::json!({ "active_id": params.id }))
}

/// Outcome of a server-driven batch of plan steps.
#[derive(Debug, PartialEq, Eq)]
enum BatchOutcome {
    Completed {
        steps: usize,
    },
    Failed {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Blocked {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Cancelled {
        completed: usize,
    },
}

fn publish_plan(state: &Arc<AppState>, repo_root: &Path, plan: Plan, active_id: &str) {
    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent { plan }).ok(),
    );
    if let Ok(plans) = PlanStorage::load_all(repo_root) {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans,
                active_id: Some(active_id.to_string()),
            })
            .ok(),
        );
    }
}

fn set_step_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    status: PlanItemStatus,
) {
    if let Ok(Some(plan)) = PlanStorage::update_item(repo_root, plan_id, step_id, Some(status)) {
        publish_plan(state, repo_root, plan, plan_id);
    }
}

fn report_step_failure(state: &Arc<AppState>, operation_id: &str, message: &str) {
    emit(
        &state.out,
        events::MODEL_ERROR,
        Some(serde_json::json!({
            "operation_id": operation_id,
            "message": message,
        })),
    );
}

/// Loads the declared target and reference files of a step into the auto context
/// layer before the turn starts. Target files are registered as editable, while
/// reference files are registered as read-only. Declared step files are never
/// capped by an artificial quota.
async fn preload_step_files(state: &Arc<AppState>, repo_root: &Path, item: &PlanItem) {
    if item.files.is_empty() && item.readonly_files.is_empty() {
        return;
    }
    let available_files =
        tauqe_core::git::list_repository_files(Some(repo_root)).unwrap_or_default();
    let mut requests: Vec<ContextRequest> =
        Vec::with_capacity(item.files.len() + item.readonly_files.len());
    requests.extend(item.files.iter().map(|p| ContextRequest {
        path: p.clone(),
        access: ContextAccess::Editable,
    }));
    requests.extend(item.readonly_files.iter().map(|p| ContextRequest {
        path: p.clone(),
        access: ContextAccess::ReadOnly,
    }));

    let (outcome, new_ctx_state) = {
        let mut ctx = state.context.write().await;
        let oc = apply_context_requests_detailed(&mut ctx, &requests, &available_files, usize::MAX);
        let s = ctx.get_state();
        (oc, s)
    };

    if !outcome.added.is_empty() {
        emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": new_ctx_state })),
        );
    }
}

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
async fn clear_auto_context(state: &Arc<AppState>) {
    let new_ctx_state = {
        let mut ctx = state.context.write().await;
        ctx.clear_auto();
        ctx.get_state()
    };
    emit(
        &state.out,
        events::CONTEXT_CHANGED,
        Some(serde_json::json!({ "state": new_ctx_state })),
    );
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Plan steps ask the model to modify code: explanatory text alone, even when
/// accompanied by a terminal success tag, must not satisfy the step contract.
fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { applied, error, .. }) => {
            *applied && error.is_none()
        }
        _ => false,
    }
}

/// Returns the declared dependencies of a step that are not yet DONE.
///
/// A step may execute only once every `depends_on` identifier refers to a step
/// whose status is `Done`; an unknown identifier is treated as unmet so a typo
/// cannot silently unlock a step. This re-check complements the scope
/// evaluation performed when the batch starts.
fn unmet_dependencies(plan: &Plan, item: &PlanItem) -> Vec<String> {
    item.depends_on
        .iter()
        .filter(|dep| {
            plan.find_item(dep.as_str())
                .map(|dependency| dependency.status != PlanItemStatus::Done)
                .unwrap_or(true)
        })
        .map(|dep| format!("#{}", dep))
        .collect()
}

/// Executes one step in an isolated turn, retrying with corrective feedback while
/// the attempt budget lasts. The step becomes DONE only when a turn satisfies the
/// success contract; once all attempts are exhausted it returns to TODO.
async fn run_plan_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    batch_step_ids: &[String],
) -> Result<(), String> {
    let plan = match PlanStorage::load_plan(repo_root, plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Err(format!("Plan '{}' not found", plan_id)),
        Err(err) => return Err(format!("{:#}", err)),
    };
    let item = match plan.find_item(step_id) {
        Some(i) => i.clone(),
        None => return Err(format!("Step '{}' not found in plan '{}'", step_id, plan_id)),
    };

    match item.status {
        PlanItemStatus::Done => return Ok(()),
        PlanItemStatus::Discussion => {
            return Err(format!(
                "Step '{}' ({}) is in DISCUSSION status and cannot be executed until reviewed",
                item.id, item.title
            ));
        }
        PlanItemStatus::Blocked => {
            return Err(format!(
                "Step '{}' ({}) is BLOCKED; resolve the reported blocker and replan before executing",
                item.id, item.title
            ));
        }
        PlanItemStatus::Cancelled => {
            return Err(format!(
                "Step '{}' ({}) is CANCELLED and cannot be executed",
                item.id, item.title
            ));
        }
        PlanItemStatus::Todo | PlanItemStatus::InProgress => {}
    }

    // Re-verify DAG readiness before launching: a step may run only once every
    // declared dependency is DONE. The scope evaluation performs this check
    // when the batch starts; repeating it here guards against status drift
    // between the steps of a single run.
    let unmet = unmet_dependencies(&plan, &item);
    if !unmet.is_empty() {
        return Err(format!(
            "Step '{}' ({}) depends on step(s) that are not DONE: {}. Complete the prerequisites first",
            item.id,
            item.title,
            unmet.join(", ")
        ));
    }

    preload_step_files(state, repo_root, &item).await;
    set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::InProgress);

    let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(&plan, step_id, "") {
        Ok(p) => p,
        Err(err) => {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
            return Err(err);
        }
    };

    let (max_retries, auto_level_up) = {
        let cfg = state.config.lock().await;
        (cfg.develop.max_retries, cfg.models.auto_level_up)
    };

    let mut selection = ModelSelection::Role(ModelRole::Coder);
    let mut attempt = 0usize;

    loop {
        let workflow_name = state.config.lock().await.develop.workflow.trim().to_lowercase();
        let policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy> = match workflow_name.as_str() {
            "naive" => Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new()),
            _ => Box::new(tauqe_core::workflow::git_policy::GitTransactionPolicy::new()),
        };

        let plan_context = tauqe_protocol::ModelTurnPlanContext {
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            batch_step_ids: batch_step_ids.to_vec(),
        };

        let turn_result = crate::handlers::model::execute_model_turn(
            state,
            prompt.clone(),
            Some(selection.clone()),
            Some(plan_context),
            policy,
        )
        .await;

        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
                let op_id = format!("op-{}", crate::state::next_operation_id());
                report_step_failure(state, &op_id, &err.message);
                return Err(err.message);
            }
        };

        // Semantic block: the coder reports that the step preconditions cannot
        // be satisfied as stated. Record the step as BLOCKED, preserving the
        // coder's report, and stop without a retry cycle.
        if let Some(reason) = turn.blocked.clone() {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Blocked);
            report_step_failure(state, &turn.operation_id, &reason);
            return Err(reason);
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Done);
            return Ok(());
        }

        let reason = turn.error.clone().unwrap_or_else(|| {
            if applied {
                "Step finished without verified changes or a success tag".to_string()
            } else {
                "Step finished without applying any code changes".to_string()
            }
        });

        if turn.is_cancelled || attempt >= max_retries {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
            if turn.error.is_none() && !turn.is_cancelled {
                report_step_failure(state, &turn.operation_id, &reason);
            }
            return Err(reason);
        }

        // Reset to a clean, deterministic basis before the corrective attempt.
        clear_auto_context(state).await;
        preload_step_files(state, repo_root, &item).await;

        if auto_level_up {
            selection = ModelSelection::Role(ModelRole::Architect);
        }

        prompt = tauqe_core::plan::prompt::format_plan_step_retry_prompt(
            &turn.assistant_text,
            turn.error.as_deref(),
        );
        attempt += 1;
    }
}

/// Adapter that executes a single plan step as a [`BatchStepExecutor`] step.
///
/// Plan status synchronization (InProgress/Done/Todo) and step file preloading
/// stay inside the executor, keeping the batch supervisor agnostic of plan
/// storage.
struct PlanStepExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    plan_id: &'a str,
    batch_step_ids: &'a [String],
}

#[async_trait]
impl BatchStepExecutor for PlanStepExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        match run_plan_step(
            self.state,
            self.repo_root,
            self.plan_id,
            &step.id,
            self.batch_step_ids,
        )
        .await
        {
            Ok(()) => Ok(BatchStepOutcome::succeeded()),
            Err(reason) => Ok(BatchStepOutcome::failed(reason)),
        }
    }
}

/// Runs the plan steps through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Steps completed before the first
/// failure stay DONE.
async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    step_ids: Vec<String>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let _ = PlanStorage::save_active_id(&repo_root, Some(plan_id));

    let steps: Vec<BatchStep> = step_ids
        .iter()
        .map(|id| BatchStep::new(id.clone(), format!("Execute plan step {}", id)))
        .collect();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let mut executor = PlanStepExecutor {
        state,
        repo_root: &repo_root,
        plan_id,
        batch_step_ids: &step_ids,
    };

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx)
        .await;

    match report.stop {
        BatchStop::Completed => BatchOutcome::Completed {
            steps: report.executed.len(),
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: report.executed.len(),
        },
        BatchStop::Failed { step_id } => {
            let completed = report.executed.len().saturating_sub(1);
            let reason = report
                .executed
                .last()
                .and_then(|r| r.outcome.error.clone())
                .unwrap_or_else(|| "step failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed,
                step_id,
                reason,
            }
        }
        BatchStop::Blocked { step_id, reason } => {
            let completed = report.executed.len().saturating_sub(1);
            BatchOutcome::Blocked {
                completed,
                step_id,
                reason,
            }
        }
    }
}

pub async fn handle_plan_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan execute params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let plan = match PlanStorage::load_plan(&repo_root, &params.plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Response::err(req.id, "NOT_FOUND", format!("Plan '{}' not found", params.plan_id)),
        Err(err) => return Response::err(req.id, "PLAN_LOAD_FAILED", format!("{:#}", err)),
    };

    let steps = match plan.evaluate_scope(params.step_id.as_deref()) {
        PlanScopeEvaluation::Ready(steps) => steps,
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            return Response::err(
                req.id,
                "STEP_IN_DISCUSSION",
                format!(
                    "Scope contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::BlockedByDependencies(items) => {
            return Response::err(
                req.id,
                "STEP_BLOCKED_BY_DEPENDENCIES",
                format!(
                    "Scope contains step(s) whose prerequisites are not yet DONE: {}. Complete the prerequisites first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::StepBlocked => {
            return Response::err(
                req.id,
                "STEP_BLOCKED",
                "Target step is BLOCKED; resolve the reported blocker and replan before executing",
            );
        }
        PlanScopeEvaluation::AllCompleted => {
            return Response::err(req.id, "NOTHING_TO_EXECUTE", "All steps in this scope are already completed");
        }
        PlanScopeEvaluation::Cancelled => {
            return Response::err(req.id, "STEP_CANCELLED", "Step is CANCELLED and cannot be executed");
        }
        PlanScopeEvaluation::NotFound => {
            return Response::err(req.id, "NOT_FOUND", "Target item not found in plan");
        }
    };

    if state.active_cancel.lock().await.is_some() {
        return Response::err(
            req.id,
            "OPERATION_IN_PROGRESS",
            "Another operation is already running",
        );
    }

    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any step.\n\n{}",
            report
        );
        return Response::err(req.id, "PREFLIGHT_VERIFICATION_FAILED", message);
    }

    let step_ids: Vec<String> = steps.iter().map(|s| s.id.clone()).collect();
    let response = Response::ok(
        req.id,
        serde_json::json!({ "plan_id": plan.id, "step_ids": step_ids }),
    );

    let state = Arc::clone(state);
    let plan_id = plan.id.clone();
    tokio::spawn(async move {
        match run_plan_batch(&state, &plan_id, step_ids).await {
            BatchOutcome::Completed { steps } => {
                tracing::info!("Plan '{}' batch completed: {} step(s)", plan_id, steps);
            }
            BatchOutcome::Failed {
                completed,
                step_id,
                reason,
            } => {
                tracing::warn!(
                    "Plan '{}' batch stopped at step '{}' after {} completed step(s): {}",
                    plan_id,
                    step_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Blocked {
                completed,
                step_id,
                reason,
            } => {
                tracing::warn!(
                    "Plan '{}' batch blocked at step '{}' after {} completed step(s): {}",
                    plan_id,
                    step_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Cancelled { completed } => {
                tracing::warn!(
                    "Plan '{}' batch cancelled after {} completed step(s)",
                    plan_id,
                    completed
                );
            }
        }
    });

    response
}

pub async fn handle_plan_refine(req: Request, state: &Arc<AppState>) -> Response {
    let (plan_id, custom_model, instructions) = match req.params {
        Some(val) => {
            if let Ok(params) = serde_json::from_value::<PlanRefineParams>(val.clone()) {
                (params.plan_id, params.model, params.instructions)
            } else if let Some(obj) = val.as_object() {
                let id = obj
                    .get("plan_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let model = obj.get("model").and_then(|v| serde_json::from_value(v.clone()).ok());
                let instructions = obj
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                (id, model, instructions)
            } else {
                return Response::err(req.id, "INVALID_PARAMS", "Invalid plan refine parameters");
            }
        }
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing plan refine parameters"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let effective_plan_id = if plan_id.trim().is_empty() {
        PlanStorage::load_active_id(&repo_root)
            .ok()
            .flatten()
            .unwrap_or_default()
    } else {
        plan_id
    };

    if effective_plan_id.trim().is_empty() {
        return Response::err(
            req.id,
            "INVALID_PARAMS",
            "Missing plan ID and no active plan is set",
        );
    }

    let plan = match PlanStorage::load_plan(&repo_root, &effective_plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                "NOT_FOUND",
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, "PLAN_LOAD_FAILED", format!("{:#}", err)),
    };

    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        let mut active = state.active_cancel.lock().await;
        if active.is_some() {
            return Response::err(
                req.id,
                "OPERATION_IN_PROGRESS",
                "Another operation is already running",
            );
        }
        *active = Some(ActiveOperation {
            cancel_tx,
            abort_handle: None,
        });
    }

    let (provider, model, reasoning_effort, repomap_budget, max_discovery_rounds, max_files) = {
        let cfg = state.config.lock().await;
        let model = match custom_model {
            Some(m) => cfg.resolve_model(&m),
            None => cfg.resolve_role(ModelRole::Architect),
        };
        let reasoning_effort = cfg.models.resolve_effort(ModelRole::Architect);
        let provider: Arc<dyn LlmProvider> = match create_provider(model.provider, &cfg) {
            Ok(p) => Arc::from(p),
            Err(err) => {
                *state.active_cancel.lock().await = None;
                return Response::err(req.id, "NO_API_KEY", err.to_string());
            }
        };
        (
            provider,
            model,
            reasoning_effort,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.max_files,
        )
    };

    let op_id = format!("op-{}", crate::state::next_operation_id());

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "model": model,
        })),
    });

    let intro_header = format!(
        "# Deep Plan Refinement: [{}] {}\n*Analyzing codebase contours and decomposing architectural plan...*\n\n---\n\n",
        plan.id, plan.title
    );
    state.out.send_event(&Event {
        method: events::MODEL_TEXT_DELTA.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "delta": intro_header,
        })),
    });

    {
        let ctx = state.context.read().await;
        let context_paths: Vec<String> = ctx.get_state().items.into_iter().map(|it| it.path).collect();
        let mut history_turn_prompt = format!("# Deep Plan Refinement: [{}] {}\n", plan.id, plan.title);
        if let Some(ins) = &instructions {
            let trimmed = ins.trim();
            if !trimmed.is_empty() {
                history_turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        let _ = state.history.lock().await.record_turn(history_turn_prompt, context_paths);
    }

    let mut discovery_round = 0usize;
    let mut feedback: Option<String> = None;
    let mut accumulated_full_text = intro_header.clone();

    let output = loop {
        state.out.send_event(&Event {
            method: events::TURN_PHASE.to_string(),
            params: serde_json::to_value(TurnPhaseEvent {
                operation_id: op_id.clone(),
                phase: TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: Some("Deep Refinement".to_string()),
            })
            .ok(),
        });

        let (context_files, repo_map) = {
            let ctx = state.context.read().await;
            let files = ctx.read_context_files();
            let context_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
            let all_files =
                tauqe_core::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
            let rmap = tauqe_core::repomap::generate_repo_map(
                &repo_root,
                &all_files,
                &context_paths,
                repomap_budget,
            )
            .ok();
            (files, rmap)
        };

        let sys_prompt = tauqe_core::plan::prompt::build_plan_refine_system_prompt(&plan);
        let mut user_prompt = tauqe_core::plan::prompt::build_plan_refine_user_prompt(
            &plan,
            &context_files,
            repo_map.as_deref(),
            instructions.as_deref(),
        );

        if let Some(ref fb) = feedback {
            user_prompt.push_str("\n\n");
            user_prompt.push_str(fb);
        }

        if discovery_round >= max_discovery_rounds {
            user_prompt.push_str(&format!(
                "\n\n<context_feedback>\nReached maximum context discovery rounds limit ({}). You MUST now finalize the plan with status 'success' or 'blocked'. Do not request further files.\n</context_feedback>",
                max_discovery_rounds
            ));
        }

        let messages = vec![
            ChatMessage::system(sys_prompt),
            ChatMessage::user(user_prompt),
        ];

        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        let schema_def: JsonSchemaDefinition = match serde_json::from_value(schema_val) {
            Ok(s) => s,
            Err(err) => {
                *state.active_cancel.lock().await = None;
                return Response::err(req.id, "SCHEMA_ERROR", err.to_string());
            }
        };
        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: schema_def,
        });

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
        let model_name = model.name.clone();
        let stream_cancel = cancel_rx.clone();
        let provider_stream = Arc::clone(&provider);

        let stream_task = tokio::spawn(async move {
            provider_stream
                .stream_chat_with_effort(
                    &model_name,
                    messages,
                    response_format,
                    reasoning_effort,
                    tx,
                    stream_cancel,
                )
                .await
        });

        if let Some(op) = state.active_cancel.lock().await.as_mut() {
            op.abort_handle = Some(stream_task.abort_handle());
        }

        let mut raw_text = String::new();
        let mut usage = None;
        let mut failure = None;
        let mut passthrough: Option<bool> = None;
        let mut streamed_explanation_bytes = 0usize;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ReasoningDelta(delta) => {
                    state.out.send_event(&Event {
                        method: events::MODEL_REASONING_DELTA.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id,
                            "delta": delta,
                        })),
                    });
                }
                StreamEvent::TextDelta(delta) => {
                    raw_text.push_str(&delta);

                    if passthrough.is_none() {
                        if let Some(c) = raw_text.chars().find(|c| !c.is_whitespace()) {
                            let plain = c != '{' && c != '`';
                            passthrough = Some(plain);
                            if plain {
                                state.out.send_event(&Event {
                                    method: events::MODEL_TEXT_DELTA.to_string(),
                                    params: Some(serde_json::json!({
                                        "operation_id": op_id,
                                        "delta": raw_text.clone(),
                                    })),
                                });
                            }
                        }
                    } else if passthrough == Some(true) {
                        state.out.send_event(&Event {
                            method: events::MODEL_TEXT_DELTA.to_string(),
                            params: Some(serde_json::json!({
                                "operation_id": op_id,
                                "delta": delta,
                            })),
                        });
                    }

                    if passthrough != Some(true) {
                        let (field_delta, new_streamed, _) =
                            tauqe_core::edits::stream::json::extract_streamed_string_field(
                                &raw_text,
                                "explanation",
                                streamed_explanation_bytes,
                            );
                        if let Some(d) = field_delta {
                            if !d.is_empty() {
                                state.out.send_event(&Event {
                                    method: events::MODEL_TEXT_DELTA.to_string(),
                                    params: Some(serde_json::json!({
                                        "operation_id": op_id,
                                        "delta": d,
                                    })),
                                });
                            }
                        }
                        streamed_explanation_bytes = new_streamed;
                    }
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::Done | StreamEvent::Cancelled => break,
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                _ => {}
            }
        }

        let cancelled = *cancel_rx.borrow();
        if cancelled {
            *state.active_cancel.lock().await = None;
            state.out.send_event(&Event {
                method: events::MODEL_CANCELLED.to_string(),
                params: Some(serde_json::json!({ "operation_id": op_id })),
            });
            return Response::err(req.id, "CANCELLED", "Plan refinement was cancelled");
        }
        if let Some(err) = failure {
            *state.active_cancel.lock().await = None;
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id,
                    "message": err,
                })),
            });
            return Response::err(req.id, "MODEL_ERROR", err);
        }

        let usage = usage.unwrap_or_default();
        let cost = usage.cost.unwrap_or(0.0);
        let total_cost = {
            let mut total = state.total_cost.lock().await;
            *total += cost;
            *total
        };

        state.out.send_event(&Event {
            method: events::MODEL_USAGE.to_string(),
            params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                operation_id: op_id.clone(),
                usage: usage.clone(),
                session_total_cost: total_cost,
                current_cost: Some(cost),
            })
            .ok(),
        });

        let output = match tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text) {
            Ok(out) => out,
            Err(err) => {
                *state.active_cancel.lock().await = None;
                let err_msg = format!("Failed to parse architect refinement output: {err}");
                state.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "message": err_msg,
                    })),
                });
                return Response::err(req.id, "PARSE_ERROR", err_msg);
            }
        };

        if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
            state.out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id,
                    "delta": output.explanation.clone(),
                })),
            });
        }
        accumulated_full_text.push_str(&output.explanation);

        if output.status.eq_ignore_ascii_case("needs_context") && discovery_round < max_discovery_rounds {
            let requests: Vec<ContextRequest> = output
                .context_requests
                .iter()
                .map(|p| ContextRequest {
                    path: p.clone(),
                    access: ContextAccess::ReadOnly,
                })
                .collect();

            let (outcome, new_ctx_state) = {
                let mut ctx = state.context.write().await;
                let available_files =
                    tauqe_core::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
                let oc = apply_context_requests_detailed(
                    &mut ctx,
                    &requests,
                    &available_files,
                    max_files,
                );
                let s = ctx.get_state();
                (oc, s)
            };

            if let Some(limit) = outcome.limit_exceeded {
                let warn_msg = format!(
                    "Context files limit exceeded (max_files = {}). Architect requested {} new file(s) [{}], but context already has {} files. The plan is too broad and should be manually decomposed or scoped down.",
                    limit.max_files,
                    limit.requested_count,
                    limit.requested_files.join(", "),
                    limit.current_count
                );
                tracing::warn!("{}", warn_msg);
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "delta": format!("\n\n⚠️ {}\n\n", warn_msg),
                    })),
                });
                state.out.send_event(&Event {
                    method: events::MODEL_FINISHED.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "full_text": format!("{}\n\n⚠️ {}", accumulated_full_text, warn_msg),
                    })),
                });
                *state.active_cancel.lock().await = None;
                let _ = state.history.lock().await.record_response(
                    warn_msg.clone(),
                    None,
                    Some(format!("Plan Refinement Exceeded File Limit: {}", plan.title)),
                    Vec::new(),
                );
                return Response::ok_typed(req.id, &PlanRefineResult::Blocked { reason: warn_msg });
            }

            if !outcome.added.is_empty() {
                state.out.send_event(&Event {
                    method: events::CONTEXT_CHANGED.to_string(),
                    params: Some(serde_json::json!({ "state": new_ctx_state })),
                });
            }

            if outcome.added.is_empty() && outcome.missing.is_empty() {
                break output;
            }

            discovery_round += 1;
            state.out.send_event(&Event {
                method: events::TURN_PHASE.to_string(),
                params: serde_json::to_value(TurnPhaseEvent {
                    operation_id: op_id.clone(),
                    phase: TurnPhase::Discovery,
                    round: Some(discovery_round),
                    max_rounds: Some(max_discovery_rounds),
                    detail: Some(format!("Discovery round {}", discovery_round)),
                })
                .ok(),
            });
            let mut notes = Vec::new();
            if !outcome.added.is_empty() {
                notes.push(format!(
                    "Added {} file(s) to context: {}",
                    outcome.added.len(),
                    outcome.added.join(", ")
                ));
            }
            if !outcome.missing.is_empty() {
                notes.push(format!(
                    "Files not found in repository: {}",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() {
                notes.push(format!(
                    "Files already in context: {}",
                    outcome.already_satisfied.join(", ")
                ));
            }

            let round_banner = format!(
                "\n\n---\n**[Round {}]** {}\n\n",
                discovery_round + 1,
                notes.join("; ")
            );
            accumulated_full_text.push_str(&round_banner);
            state.out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id,
                    "delta": round_banner,
                })),
            });

            let mut fb = String::new();
            if !outcome.missing.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>\n",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the plan.\n</context_feedback>\n",
                    outcome.already_satisfied.join(", ")
                ));
            }
            feedback = if fb.is_empty() { None } else { Some(fb) };
            continue;
        }

        break output;
    };

    *state.active_cancel.lock().await = None;

    state.out.send_event(&Event {
        method: events::MODEL_FINISHED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "full_text": accumulated_full_text,
        })),
    });

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        let blocked_msg = format!("Architect reported blockers for plan [{}]:\n\n{}", plan.id, reason);
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("Plan Refinement Blocked: {}", plan.title)),
            Vec::new(),
        );
        return Response::ok_typed(req.id, &PlanRefineResult::Blocked { reason });
    }

    if output.items.is_empty() {
        return Response::err(
            req.id,
            "EMPTY_ITEMS",
            "Model returned success status but an empty items array",
        );
    }

    let mut updated_plan = plan.clone();
    updated_plan.items = output.items;
    updated_plan.refresh_parent_statuses();
    updated_plan.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (total_items, _) = updated_plan.count_stats();
    let history_response = format!(
        "Plan [{}] refined successfully into {} items.\n\n{}",
        updated_plan.id, total_items, output.explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("Plan Refinement: {}", updated_plan.title)),
        Vec::new(),
    );

    if let Err(err) = PlanStorage::save_plan(&repo_root, &updated_plan) {
        return Response::err(req.id, "PLAN_SAVE_FAILED", format!("{:#}", err));
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: updated_plan.clone(),
        })
        .ok(),
    );

    if let Ok(plans) = PlanStorage::load_all(&repo_root) {
        let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
        );
    }

    Response::ok_typed(
        req.id,
        &PlanRefineResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        },
    )
}

/// Handles an addressed (local) replanning request for a blocked plan step.
///
/// The Architect receives a targeted diagnostic packet (immutable DONE foundation,
/// blocked step diagnostics, remaining horizon, and concept invariants) and returns a
/// replacement sub-tree. The sub-tree is grafted atomically over the blocked leaf via
/// `PlanStorage::graft_subtree`, preserving the rest of the plan and re-pointing the
/// dependent tail at the new work.
pub async fn handle_plan_replan(req: Request, state: &Arc<AppState>) -> Response {
    let (plan_id, step_id, blocked_reason, custom_model, instructions) = match req.params {
        Some(val) => match val.as_object() {
            Some(obj) => {
                let plan_id = obj
                    .get("plan_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let step_id = obj
                    .get("step_id")
                    .or_else(|| obj.get("item_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let blocked_reason = obj
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let custom_model = obj
                    .get("model")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let instructions = obj
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                (plan_id, step_id, blocked_reason, custom_model, instructions)
            }
            None => {
                return Response::err(req.id, "INVALID_PARAMS", "Invalid plan replan parameters")
            }
        },
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing plan replan parameters"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let effective_plan_id = if plan_id.trim().is_empty() {
        PlanStorage::load_active_id(&repo_root)
            .ok()
            .flatten()
            .unwrap_or_default()
    } else {
        plan_id
    };

    if effective_plan_id.trim().is_empty() {
        return Response::err(
            req.id,
            "INVALID_PARAMS",
            "Missing plan ID and no active plan is set",
        );
    }
    if step_id.trim().is_empty() {
        return Response::err(req.id, "INVALID_PARAMS", "Missing blocked step ID to replan");
    }

    let plan = match PlanStorage::load_plan(&repo_root, &effective_plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                "NOT_FOUND",
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, "PLAN_LOAD_FAILED", format!("{:#}", err)),
    };

    let step = match plan.find_item(&step_id) {
        Some(item) => item.clone(),
        None => {
            return Response::err(
                req.id,
                "NOT_FOUND",
                format!("Step '{}' not found in plan '{}'", step_id, effective_plan_id),
            );
        }
    };

    if !step.is_leaf() {
        return Response::err(
            req.id,
            "NOT_A_LEAF",
            format!(
                "Step '{}' ({}) is not a leaf and cannot be replanned",
                step.id, step.title
            ),
        );
    }
    if step.status == PlanItemStatus::Done {
        return Response::err(
            req.id,
            "STEP_IMMUTABLE",
            format!(
                "Step '{}' is DONE and immutable; completed steps cannot be replanned",
                step.id
            ),
        );
    }

    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        let mut active = state.active_cancel.lock().await;
        if active.is_some() {
            return Response::err(
                req.id,
                "OPERATION_IN_PROGRESS",
                "Another operation is already running",
            );
        }
        *active = Some(ActiveOperation {
            cancel_tx,
            abort_handle: None,
        });
    }

    let (provider, model, reasoning_effort) = {
        let cfg = state.config.lock().await;
        let model = match custom_model {
            Some(m) => cfg.resolve_model(&m),
            None => cfg.resolve_role(ModelRole::Architect),
        };
        let reasoning_effort = cfg.models.resolve_effort(ModelRole::Architect);
        let provider: Arc<dyn LlmProvider> = match create_provider(model.provider, &cfg) {
            Ok(p) => Arc::from(p),
            Err(err) => {
                *state.active_cancel.lock().await = None;
                return Response::err(req.id, "NO_API_KEY", err.to_string());
            }
        };
        (provider, model, reasoning_effort)
    };

    let op_id = format!("op-{}", crate::state::next_operation_id());

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "model": model,
        })),
    });

    let intro_header = format!(
        "# Local Plan Replanning: [{}] {}\n*Replanning blocked step `{}`...*\n\n---\n\n",
        plan.id, plan.title, step.id
    );
    state.out.send_event(&Event {
        method: events::MODEL_TEXT_DELTA.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "delta": intro_header,
        })),
    });

    let sys_prompt = tauqe_core::plan::prompt::build_replan_system_prompt();
    let mut user_prompt = tauqe_core::plan::prompt::build_replan_diagnostic_packet(
        &plan,
        &step.id,
        blocked_reason.as_deref(),
    );
    if let Some(ins) = &instructions {
        let trimmed = ins.trim();
        if !trimmed.is_empty() {
            user_prompt.push_str("\n\n## Developer Replanning Directives:\n");
            user_prompt.push_str(trimmed);
            user_prompt.push('\n');
        }
    }

    let messages = vec![
        ChatMessage::system(sys_prompt),
        ChatMessage::user(user_prompt),
    ];

    let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
    let schema_def: JsonSchemaDefinition = match serde_json::from_value(schema_val) {
        Ok(s) => s,
        Err(err) => {
            *state.active_cancel.lock().await = None;
            return Response::err(req.id, "SCHEMA_ERROR", err.to_string());
        }
    };
    let response_format = Some(ResponseFormat::JsonSchema {
        json_schema: schema_def,
    });

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
    let model_name = model.name.clone();
    let stream_cancel = cancel_rx.clone();
    let provider_stream = Arc::clone(&provider);

    let stream_task = tokio::spawn(async move {
        provider_stream
            .stream_chat_with_effort(
                &model_name,
                messages,
                response_format,
                reasoning_effort,
                tx,
                stream_cancel,
            )
            .await
    });

    if let Some(op) = state.active_cancel.lock().await.as_mut() {
        op.abort_handle = Some(stream_task.abort_handle());
    }

    let mut raw_text = String::new();
    let mut usage = None;
    let mut failure = None;
    let mut passthrough: Option<bool> = None;
    let mut streamed_explanation_bytes = 0usize;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ReasoningDelta(delta) => {
                state.out.send_event(&Event {
                    method: events::MODEL_REASONING_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::TextDelta(delta) => {
                raw_text.push_str(&delta);

                if passthrough.is_none() {
                    if let Some(c) = raw_text.chars().find(|c| !c.is_whitespace()) {
                        let plain = c != '{' && c != '`';
                        passthrough = Some(plain);
                        if plain {
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id,
                                    "delta": raw_text.clone(),
                                })),
                            });
                        }
                    }
                } else if passthrough == Some(true) {
                    state.out.send_event(&Event {
                        method: events::MODEL_TEXT_DELTA.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id,
                            "delta": delta,
                        })),
                    });
                }

                if passthrough != Some(true) {
                    let (field_delta, new_streamed, _) =
                        tauqe_core::edits::stream::json::extract_streamed_string_field(
                            &raw_text,
                            "explanation",
                            streamed_explanation_bytes,
                        );
                    if let Some(d) = field_delta {
                        if !d.is_empty() {
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id,
                                    "delta": d,
                                })),
                            });
                        }
                    }
                    streamed_explanation_bytes = new_streamed;
                }
            }
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done | StreamEvent::Cancelled => break,
            StreamEvent::Error(err) => {
                failure = Some(err);
                break;
            }
            _ => {}
        }
    }

    let cancelled = *cancel_rx.borrow();
    if cancelled {
        *state.active_cancel.lock().await = None;
        state.out.send_event(&Event {
            method: events::MODEL_CANCELLED.to_string(),
            params: Some(serde_json::json!({ "operation_id": op_id })),
        });
        return Response::err(req.id, "CANCELLED", "Plan replanning was cancelled");
    }
    if let Some(err) = failure {
        *state.active_cancel.lock().await = None;
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: Some(serde_json::json!({
                "operation_id": op_id,
                "message": err,
            })),
        });
        return Response::err(req.id, "MODEL_ERROR", err);
    }

    let usage = usage.unwrap_or_default();
    let cost = usage.cost.unwrap_or(0.0);
    let total_cost = {
        let mut total = state.total_cost.lock().await;
        *total += cost;
        *total
    };

    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id.clone(),
            usage: usage.clone(),
            session_total_cost: total_cost,
            current_cost: Some(cost),
        })
        .ok(),
    });

    *state.active_cancel.lock().await = None;

    let output = match tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text) {
        Ok(out) => out,
        Err(err) => {
            let err_msg = format!("Failed to parse architect replan output: {err}");
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id,
                    "message": err_msg,
                })),
            });
            return Response::err(req.id, "PARSE_ERROR", err_msg);
        }
    };

    if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
        state.out.send_event(&Event {
            method: events::MODEL_TEXT_DELTA.to_string(),
            params: Some(serde_json::json!({
                "operation_id": op_id,
                "delta": output.explanation.clone(),
            })),
        });
    }

    let mut full_text = intro_header.clone();
    full_text.push_str(&output.explanation);
    state.out.send_event(&Event {
        method: events::MODEL_FINISHED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id,
            "full_text": full_text,
        })),
    });

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output
            .reason
            .clone()
            .unwrap_or_else(|| output.explanation.clone());
        let blocked_msg = format!(
            "Architect reported the blocked step [{}] is not replannable:\n\n{}",
            step.id, reason
        );
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("Plan Replan Blocked: {}", plan.title)),
            Vec::new(),
        );
        return Response::ok_typed(req.id, &PlanRefineResult::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Response::err(
            req.id,
            "NEEDS_CONTEXT",
            "Architect requested additional context; local replanning does not run discovery rounds",
        );
    }

    if output.items.is_empty() {
        return Response::err(
            req.id,
            "EMPTY_ITEMS",
            "Model returned success status but an empty items array",
        );
    }

    let updated_plan =
        match PlanStorage::graft_subtree(&repo_root, &plan.id, &step.id, output.items) {
            Ok(p) => p,
            Err(err) => return Response::err(req.id, "PLAN_GRAFT_FAILED", format!("{:#}", err)),
        };

    let (total_items, _) = updated_plan.count_stats();
    let history_response = format!(
        "Plan [{}] locally replanned step `{}` into {} item(s).\n\n{}",
        updated_plan.id, step.id, total_items, output.explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("Plan Replan: {}", updated_plan.title)),
        Vec::new(),
    );

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: updated_plan.clone(),
        })
        .ok(),
    );

    if let Ok(plans) = PlanStorage::load_all(&repo_root) {
        let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
        );
    }

    Response::ok_typed(
        req.id,
        &PlanRefineResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-plan-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&path);
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_item(id: &str, status: PlanItemStatus) -> tauqe_protocol::PlanItem {
        tauqe_protocol::PlanItem {
            id: id.to_string(),
            title: format!("Step {}", id),
            status,
            ..Default::default()
        }
    }

    fn test_state(dir: &TestDir) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        )
    }

    #[tokio::test]
    async fn test_plan_batch_fail_fast_resets_status() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "batch".to_string(),
            title: "Batch".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Todo),
                test_item("2", PlanItemStatus::Todo),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let state = test_state(&dir);

        // Without an API key the first turn cannot start: the batch must stop at step 1.
        let outcome =
            run_plan_batch(&state, "batch", vec!["1".to_string(), "2".to_string()]).await;
        match outcome {
            BatchOutcome::Failed {
                completed, step_id, ..
            } => {
                assert_eq!(completed, 0);
                assert_eq!(step_id, "1");
            }
            other => panic!("Expected Failed, got {:?}", other),
        }

        let reloaded = PlanStorage::load_plan(dir.path(), "batch").unwrap().unwrap();
        assert!(reloaded.items.iter().all(|i| i.status == PlanItemStatus::Todo));
        assert_eq!(
            PlanStorage::load_active_id(dir.path()).unwrap().as_deref(),
            Some("batch")
        );
    }

    #[tokio::test]
    async fn test_plan_batch_skips_done_and_stops_on_discussion() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "skip".to_string(),
            title: "Skip".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Done),
                test_item("2", PlanItemStatus::Discussion),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let state = test_state(&dir);

        let outcome = run_plan_batch(&state, "skip", vec!["1".to_string(), "2".to_string()]).await;
        match outcome {
            BatchOutcome::Failed {
                completed, step_id, ..
            } => {
                assert_eq!(completed, 1);
                assert_eq!(step_id, "2");
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
        let reloaded = PlanStorage::load_plan(dir.path(), "skip").unwrap().unwrap();
        assert_eq!(reloaded.find_item("2").unwrap().status, PlanItemStatus::Discussion);
    }

    #[tokio::test]
    async fn test_plan_batch_skips_done_and_stops_on_blocked() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "blocked-batch".to_string(),
            title: "Blocked Batch".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Done),
                test_item("2", PlanItemStatus::Blocked),
                test_item("3", PlanItemStatus::Todo),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let state = test_state(&dir);

        let outcome = run_plan_batch(
            &state,
            "blocked-batch",
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
        )
        .await;
        match outcome {
            BatchOutcome::Failed {
                completed,
                step_id,
                reason,
            } => {
                assert_eq!(completed, 1);
                assert_eq!(step_id, "2");
                assert!(reason.contains("BLOCKED"));
            }
            other => panic!("Expected Failed on blocked step, got {:?}", other),
        }
        let reloaded = PlanStorage::load_plan(dir.path(), "blocked-batch").unwrap().unwrap();
        assert_eq!(reloaded.find_item("2").unwrap().status, PlanItemStatus::Blocked);
        assert_eq!(reloaded.find_item("3").unwrap().status, PlanItemStatus::Todo);
    }

    #[tokio::test]
    async fn test_handle_plan_execute_validation() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: tauqe_protocol::RequestId::Number(id),
            method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
            params,
        };

        let res = handle_plan_execute(make_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

        let res = handle_plan_execute(
            make_req(2, Some(serde_json::json!({ "plan_id": "missing" }))),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "NOT_FOUND");

        let mut dep_item = test_item("5", PlanItemStatus::Todo);
        dep_item.depends_on = vec!["1".to_string()];

        let plan = Plan {
            id: "val".to_string(),
            title: "Validation".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Discussion),
                test_item("2", PlanItemStatus::Cancelled),
                test_item("3", PlanItemStatus::Done),
                test_item("4", PlanItemStatus::Blocked),
                dep_item,
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let cases = [
            (None, "STEP_IN_DISCUSSION"),
            (Some("1"), "STEP_IN_DISCUSSION"),
            (Some("2"), "STEP_CANCELLED"),
            (Some("3"), "NOTHING_TO_EXECUTE"),
            (Some("4"), "STEP_BLOCKED"),
            (Some("5"), "STEP_BLOCKED_BY_DEPENDENCIES"),
            (Some("9"), "NOT_FOUND"),
        ];
        for (idx, (step, code)) in cases.iter().enumerate() {
            let params = serde_json::json!({ "plan_id": "val", "step_id": step });
            let res = handle_plan_execute(make_req(10 + idx as u64, Some(params)), &state).await;
            assert_eq!(res.error.unwrap().code, *code);
        }
    }

    #[tokio::test]
    async fn test_handle_plan_refine_defaults_to_architect_role() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "refine-role".to_string(),
            title: "Refine Role".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![test_item("1", PlanItemStatus::Todo)],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let mut config = tauqe_core::config::AppConfig::default();
        config.models.architect = Some(tauqe_protocol::ModelRef::openrouter("test/architect-model"));
        config.models.advisor = Some(tauqe_protocol::ModelRef::openrouter("test/advisor-model"));

        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
            params: Some(serde_json::json!({ "plan_id": "refine-role" })),
        };
        let res = handle_plan_refine(req, &state).await;
        let err = res.error.expect("should fail without api key");
        assert_eq!(err.code, "NO_API_KEY");
    }

    #[tokio::test]
    async fn test_handle_plan_refine_validation() {
        let dir = TestDir::new();
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        );

        // Missing params
        let req_missing = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
            params: None,
        };
        let res = handle_plan_refine(req_missing, &state).await;
        assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

        // Non-existent plan
        let req_not_found = Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
            params: Some(serde_json::json!({
                "plan_id": "non-existent"
            })),
        };
        let res_nf = handle_plan_refine(req_not_found, &state).await;
        assert_eq!(res_nf.error.unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn test_plan_execute_blocked_by_failed_preflight() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "preflight".to_string(),
            title: "Preflight".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Todo),
                test_item("2", PlanItemStatus::Todo),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        // A malformed manifest makes the deterministic toolchain pipeline fail.
        std::fs::write(dir.path().join("Cargo.toml"), "[package\n").unwrap();
        let state = test_state(&dir);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
            params: Some(serde_json::json!({ "plan_id": "preflight" })),
        };
        let res = handle_plan_execute(req, &state).await;

        let err = res.error.expect("preflight must block the batch");
        assert_eq!(err.code, "PREFLIGHT_VERIFICATION_FAILED");
        assert!(err.message.contains("Pre-flight verification failed"));

        // The failure is detected before any turn starts: no active operation.
        assert!(state.active_cancel.lock().await.is_none());

        // No step was started: every status stays TODO.
        let reloaded = PlanStorage::load_plan(dir.path(), "preflight")
            .unwrap()
            .unwrap();
        assert!(reloaded.items.iter().all(|i| i.status == PlanItemStatus::Todo));
    }

    #[test]
    fn test_turn_applied_edits_requires_applied_change() {
        use crate::handlers::model::ModelTurnResult;

        let applied = ModelTurnResult {
            operation_id: "op-1".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "fix".to_string(),
                edits: vec![],
                proposal: None,
                applied: true,
                error: None,
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc".to_string()),
            }),
            assistant_text: "done".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(applied.is_success());
        assert!(turn_applied_edits(&applied));

        // Explanatory text with a terminal success tag satisfies an interactive
        // turn but must not complete a code-modifying plan step.
        let answer = ModelTurnResult {
            operation_id: "op-2".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "Nothing to change.".to_string(),
            }),
            assistant_text: "Nothing to change.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: true,
            blocked: None,
        };
        assert!(answer.is_success());
        assert!(!turn_applied_edits(&answer));

        let failed = ModelTurnResult {
            operation_id: "op-3".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "fix".to_string(),
                edits: vec![],
                proposal: None,
                applied: false,
                error: Some("compiler error".to_string()),
                changed_files: vec![],
                commit_hash: None,
            }),
            assistant_text: String::new(),
            is_cancelled: false,
            error: Some("compiler error".to_string()),
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!turn_applied_edits(&failed));
    }

    #[test]
    fn test_unmet_dependencies_reports_prerequisites_not_done() {
        let done = test_item("1", PlanItemStatus::Done);
        let in_progress = test_item("2", PlanItemStatus::InProgress);
        let mut dependent = test_item("3", PlanItemStatus::Todo);
        dependent.depends_on = vec![
            "1".to_string(),
            "2".to_string(),
            "missing".to_string(),
        ];

        let plan = Plan {
            id: "deps".to_string(),
            title: "Deps".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![done, in_progress],
            ..Default::default()
        };

        assert_eq!(
            unmet_dependencies(&plan, &dependent),
            vec!["#2".to_string(), "#missing".to_string()]
        );

        let mut satisfied = test_item("4", PlanItemStatus::Todo);
        satisfied.depends_on = vec!["1".to_string()];
        assert!(unmet_dependencies(&plan, &satisfied).is_empty());
    }

    #[tokio::test]
    async fn test_handle_plan_replan_validation() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: tauqe_protocol::RequestId::Number(id),
            method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
            params,
        };

        // Missing params entirely.
        let res = handle_plan_replan(make_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

        // Unknown plan.
        let res = handle_plan_replan(
            make_req(
                2,
                Some(serde_json::json!({ "plan_id": "missing", "step_id": "1" })),
            ),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "NOT_FOUND");

        let parent = tauqe_protocol::PlanItem {
            id: "3".to_string(),
            title: "Parent".to_string(),
            status: PlanItemStatus::Todo,
            children: vec![test_item("3.1", PlanItemStatus::Todo)],
            ..Default::default()
        };
        let plan = Plan {
            id: "replan".to_string(),
            title: "Replan".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Done),
                test_item("2", PlanItemStatus::Blocked),
                parent,
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        // Missing step id.
        let res = handle_plan_replan(
            make_req(3, Some(serde_json::json!({ "plan_id": "replan" }))),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

        // Unknown step.
        let res = handle_plan_replan(
            make_req(
                4,
                Some(serde_json::json!({ "plan_id": "replan", "step_id": "9" })),
            ),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "NOT_FOUND");

        // DONE steps are immutable.
        let res = handle_plan_replan(
            make_req(
                5,
                Some(serde_json::json!({ "plan_id": "replan", "step_id": "1" })),
            ),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "STEP_IMMUTABLE");

        // Non-leaf steps cannot be replanned.
        let res = handle_plan_replan(
            make_req(
                6,
                Some(serde_json::json!({ "plan_id": "replan", "step_id": "3" })),
            ),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "NOT_A_LEAF");
    }
}
