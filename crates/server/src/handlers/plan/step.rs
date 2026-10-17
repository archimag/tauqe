//! Single plan-step execution.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::batch::{BatchStep, BatchStepExecutor, BatchStepOutcome};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, ModelSelection, ModelRole, ModelTurnPlanContext, PlanItem,
    PlanItemStatus,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::model::{build_apply_policy, execute_model_turn, StepTurnOptions};
use crate::state::{AppState, OperationGuard};

use super::common::{
    clear_auto_context, emit, offload_plan, report_step_failure, set_step_status,
    turn_applied_edits, unmet_dependencies, with_context_mutation,
};

/// Loads the declared target and reference files of a step into the auto context
/// layer before the turn starts. Target files are registered as editable, while
/// reference files are registered as read-only. Declared step files are never
/// capped by an artificial quota.
pub(crate) async fn preload_step_files(state: &Arc<AppState>, repo_root: &Path, item: &PlanItem) {
    if item.files.is_empty() && item.readonly_files.is_empty() {
        return;
    }
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

    let root = repo_root.to_path_buf();
    let preloaded = with_context_mutation(state, move |ctx| {
        let available_files = tauqe_core::git::list_repository_files(Some(&root))
            .ok()
            .unwrap_or_default();
        let outcome = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
        let new_state = ctx.get_state();
        (outcome, new_state)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = preloaded {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Executes one step in an isolated turn, retrying with corrective feedback while
/// the attempt budget lasts. The step becomes DONE only when a turn satisfies the
/// success contract; once all attempts are exhausted it returns to TODO. The step
/// reports a typed [`BatchStepOutcome`] so the supervisor can distinguish a
/// completed step from a failed, semantically blocked, or cancelled one.
async fn run_plan_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    batch_step_ids: &[String],
    operation: Arc<OperationGuard>,
) -> BatchStepOutcome {
    let outcome =
        run_plan_step_inner(state, repo_root, plan_id, step_id, batch_step_ids, operation).await;
    clear_auto_context(state).await;
    outcome
}

async fn run_plan_step_inner(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    batch_step_ids: &[String],
    operation: Arc<OperationGuard>,
) -> BatchStepOutcome {
    let load_root = repo_root.to_path_buf();
    let load_plan_id = plan_id.to_string();
    let plan = match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
        Ok(Some(p)) => p,
        Ok(None) => return BatchStepOutcome::failed(format!("Plan '{}' not found", plan_id)),
        Err(err) => return BatchStepOutcome::failed(err),
    };
    let item = match plan.find_item(step_id) {
        Some(i) => i.clone(),
        None => {
            return BatchStepOutcome::failed(format!(
                "Step '{}' not found in plan '{}'",
                step_id, plan_id
            ))
        }
    };

    match item.status {
        PlanItemStatus::Done => return BatchStepOutcome::succeeded(),
        PlanItemStatus::Discussion => {
            return BatchStepOutcome::failed(format!(
                "Step '{}' ({}) is in DISCUSSION status and cannot be executed until reviewed",
                item.id, item.title
            ));
        }
        PlanItemStatus::Blocked => {
            return BatchStepOutcome::failed(format!(
                "Step '{}' ({}) is BLOCKED; resolve the reported blocker and replan before executing",
                item.id, item.title
            ));
        }
        PlanItemStatus::Cancelled => {
            return BatchStepOutcome::failed(format!(
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
        return BatchStepOutcome::failed(format!(
            "Step '{}' ({}) depends on step(s) that are not DONE: {}. Complete the prerequisites first",
            item.id,
            item.title,
            unmet.join(", ")
        ));
    }

    preload_step_files(state, repo_root, &item).await;
    set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::InProgress).await;
    tracing::info!(plan_id = plan_id, step_id = step_id, "plan step started");

    // Generate the active turn marker before assembling prompts so the target
    // step prompt and the model turn share the same marker-bound protocol.
    let marker = state.next_turn_marker().await;
    let marker_suffix = marker
        .as_ref()
        .map(|m| format!("_{}", m))
        .unwrap_or_default();

    let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
        &plan,
        step_id,
        &marker_suffix,
    ) {
        Ok(p) => p,
        Err(err) => {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo).await;
            return BatchStepOutcome::failed(err);
        }
    };

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    let mut selection = ModelSelection::Role(ModelRole::Coder);
    let mut reasoning_effort = None;
    let mut ladder_idx = 0usize;
    let mut attempt = 0usize;

    loop {
        let workflow = state.config.lock().await.config.develop.workflow;
        let policy = build_apply_policy(state, workflow);

        let plan_context = ModelTurnPlanContext {
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            batch_step_ids: batch_step_ids.to_vec(),
        };

        let turn_result = execute_model_turn(
            state,
            prompt.clone(),
            policy,
            StepTurnOptions {
                custom_selection: Some(selection.clone()),
                plan_context: Some(plan_context),
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
            },
        )
        .await;

        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo).await;
                let op_id = format!("op-{}", crate::state::next_operation_id());
                report_step_failure(state, &op_id, &err.message);
                return BatchStepOutcome::failed(err.message);
            }
        };

        // Semantic block: the coder reports that the step preconditions cannot
        // be satisfied as stated. Record the step as BLOCKED, preserving the
        // coder's report, and stop without a retry cycle. The block is reported
        // to the supervisor as a distinct outcome so clients can tell a blocked
        // step apart from a plain failure.
        if let Some(reason) = turn.blocked.clone() {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Blocked).await;
            report_step_failure(state, &turn.operation_id, &reason);
            return BatchStepOutcome::blocked(reason);
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Done).await;
            return BatchStepOutcome::succeeded();
        }

        // A cancelled turn ends the batch deterministically: the step returns to
        // TODO and the supervisor stops with `BatchStop::Cancelled` instead of
        // reporting a spurious failure.
        if turn.is_cancelled {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo).await;
            return BatchStepOutcome::cancelled();
        }

        let reason = turn.error.clone().unwrap_or_else(|| {
            if applied {
                "Step finished without verified changes or a success tag".to_string()
            } else {
                "Step finished without applying any code changes".to_string()
            }
        });

        if attempt >= max_retries {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo).await;
            if turn.error.is_none() {
                report_step_failure(state, &turn.operation_id, &reason);
            }
            return BatchStepOutcome::failed(reason);
        }

        // Reset to a clean, deterministic basis before the corrective attempt.
        clear_auto_context(state).await;
        preload_step_files(state, repo_root, &item).await;

        if auto_level_up {
            let from_tier = ladder_idx;
            ladder_idx += 1;
            let tier = models_config.coder_tier(ladder_idx);
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                from_tier = from_tier,
                to_tier = ladder_idx,
                to_model = %tier.model,
                effort = ?tier.effort,
                reason = %reason,
                "plan step coder escalation"
            );
            selection = ModelSelection::Specific(tier.model);
            reasoning_effort = tier.effort;
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
pub(super) struct PlanStepExecutor<'a> {
    pub(super) state: &'a Arc<AppState>,
    pub(super) repo_root: &'a Path,
    pub(super) plan_id: &'a str,
    pub(super) batch_step_ids: &'a [String],
    pub(super) operation: &'a Arc<OperationGuard>,
}

#[async_trait]
impl BatchStepExecutor for PlanStepExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        Ok(run_plan_step(
            self.state,
            self.repo_root,
            self.plan_id,
            &step.id,
            self.batch_step_ids,
            Arc::clone(self.operation),
        )
        .await)
    }
}
