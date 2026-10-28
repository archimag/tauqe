//! Single plan-step execution.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::ModelsConfig;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{format_session_id, StreamEvent, SESSION_PREFIX_PLAN};
use tauqe_core::workflow::batch::{BatchStep, BatchStepExecutor, BatchStepOutcome};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, ModelRef, ModelRole, ModelSelection, ModelTurnPlanContext, PlanItem,
    PlanItemStatus, TurnIntent,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::model::{build_apply_policy, execute_model_turn, StepTurnOptions};
use crate::state::{AppState, OperationGuard};

use super::common::{
    clear_auto_context, emit, offload_plan, report_step_failure, set_step_status,
    set_step_status_checked, turn_applied_edits, unmet_dependencies, with_context_mutation,
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
        // Declared step files are never capped by an artificial quota, so both
        // the files budget and the refs budget stay unbounded here.
        let outcome = apply_context_requests_detailed(
            ctx,
            &requests,
            &available_files,
            usize::MAX,
            usize::MAX,
        );
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
#[allow(clippy::too_many_arguments)]
async fn run_plan_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    batch_step_ids: &[String],
    operation: Arc<OperationGuard>,
    model: Option<&ModelSelection>,
) -> BatchStepOutcome {
    let outcome = run_plan_step_inner(
        state,
        repo_root,
        plan_id,
        step_id,
        batch_step_ids,
        operation,
        model,
    )
    .await;
    clear_auto_context(state).await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_plan_step_inner(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    batch_step_ids: &[String],
    operation: Arc<OperationGuard>,
    model: Option<&ModelSelection>,
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
    set_step_status(
        state,
        repo_root,
        plan_id,
        step_id,
        PlanItemStatus::InProgress,
        None,
    )
    .await;
    tracing::info!(plan_id = plan_id, step_id = step_id, "plan step started");

    // Every step and retry of one plan extends the same provider prefix cache,
    // so the session identifier is derived from the plan.
    let session_id = Some(format_session_id(SESSION_PREFIX_PLAN, plan_id));

    // Generate the active turn marker before assembling prompts so the target
    // step prompt and the model turn share the same marker-bound protocol.
    let marker = state.next_turn_marker().await;
    let protocol = state.config.lock().await.config.develop.protocol;

    let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
        &plan,
        step_id,
        protocol,
        marker.as_deref(),
    ) {
        Ok(p) => p,
        Err(err) => {
            set_step_status(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Todo,
                None,
            )
            .await;
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

    // The selected coder is the starting rung of this step: an explicit
    // selection runs at its own reasoning depth, and a later escalation
    // continues above it instead of restarting the ladder at its base.
    let mut selection = match model {
        Some(selection) => selection.clone(),
        None => ModelSelection::Role(ModelRole::Coder),
    };
    let mut ladder_idx = models_config.coder_tier_index(Some(&selection));
    let mut reasoning_effort = models_config.resolve_selection_effort(ModelRole::Coder, &selection);
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
                intent: TurnIntent::IsolatedStep,
                custom_selection: Some(selection.clone()),
                plan_context: Some(plan_context),
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
                session_id: session_id.clone(),
            },
        )
        .await;

        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                set_step_status(
                    state,
                    repo_root,
                    plan_id,
                    step_id,
                    PlanItemStatus::Todo,
                    None,
                )
                .await;
                // The failed turn names the operation it belonged to, so the
                // failure event is attributable to this batch instead of a fresh
                // identifier that matches no registered operation.
                let op_id = err
                    .operation_id
                    .clone()
                    .unwrap_or_else(|| format!("op-{}", operation.id()));
                report_step_failure(state, &op_id, &err.error.message);
                return BatchStepOutcome::failed(err.error.message);
            }
        };

        // Semantic block: the coder reports that the step preconditions cannot
        // be satisfied as stated. Record the step as BLOCKED together with the
        // reported diagnostic, and stop without a retry cycle. The block is
        // reported to the supervisor as a distinct outcome so clients can tell
        // a blocked step apart from a plain failure.
        if let Some(reason) = turn.blocked.clone() {
            let blocked = set_step_status_checked(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Blocked,
                Some(reason.as_str()),
            )
            .await;
            if let Err(err) = blocked {
                // The block is only meaningful when the durable record carries
                // it: a BLOCKED status that never reached disk would let the
                // next batch schedule the same step again, so the persistence
                // failure is reported instead of a block nobody can observe.
                report_step_failure(state, &turn.operation_id, &err);
                return BatchStepOutcome::failed(err);
            }
            report_step_failure(state, &turn.operation_id, &reason);
            return BatchStepOutcome::blocked(reason);
        }

        // Unconditional abort: the coder reports that the harness cannot
        // perform the work at all and the developer must act in the host
        // environment first. Unlike a semantic block, an abort never starts an
        // architect replanning cycle, so the step returns to TODO, no coder
        // escalation is attempted, and the supervisor stops its queue. The
        // failure carries the recommendation so the developer receives the exact
        // command to run.
        if let Some(abort) = turn.abort.clone() {
            set_step_status(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Todo,
                None,
            )
            .await;
            let message = abort.to_message();
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                reason = %abort.reason,
                "plan step aborted; developer action in the host environment is required"
            );
            report_step_failure(state, &turn.operation_id, &message);
            return BatchStepOutcome::failed(message);
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            // The DONE transition is part of the success contract: when the
            // write cannot be persisted the step is reported as failed, so a
            // dependent step that re-reads the plan from disk never stalls on
            // a prerequisite the durable record still shows as InProgress.
            let done = set_step_status_checked(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Done,
                None,
            )
            .await;
            if let Err(err) = done {
                report_step_failure(state, &turn.operation_id, &err);
                return BatchStepOutcome::failed(err);
            }
            return BatchStepOutcome::succeeded();
        }

        // A cancelled turn ends the batch deterministically: the step returns to
        // TODO and the supervisor stops with `BatchStop::Cancelled` instead of
        // reporting a spurious failure.
        if turn.is_cancelled {
            set_step_status(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Todo,
                None,
            )
            .await;
            return BatchStepOutcome::cancelled();
        }

        let reason = turn.error.clone().unwrap_or_else(|| {
            if applied {
                "Step finished without verified changes or a success tag".to_string()
            } else {
                "Step finished without applying any code changes".to_string()
            }
        });
        // The rung automatic escalation would enter next. `None` means the ladder is
        // already saturated: the step sits on the strongest eligible rung, either
        // because the developer pinned a model the ladder excludes
        // (`level_up = false`) or because no stronger rung exists. Retrying would
        // only repeat the configuration that just failed, so a step on a saturated
        // ladder ends terminally instead of escalating behind the developer's back.
        let next_rung = if auto_level_up {
            models_config.next_coder_tier(ladder_idx)
        } else {
            None
        };
        if auto_level_up && next_rung.is_none() {
            set_step_status(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Todo,
                None,
            )
            .await;
            let model = models_config.resolve_model(&selection);
            let message = escalation_saturated_message(&models_config, &model, &item, &reason);
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                tier = ladder_idx,
                model = %model,
                reason = %reason,
                "plan step escalation saturated; the step ends without a stronger tier"
            );
            if turn.error.is_none() {
                report_step_failure(state, &turn.operation_id, &message);
            }
            return BatchStepOutcome::failed(message);
        }

        if attempt >= max_retries {
            set_step_status(
                state,
                repo_root,
                plan_id,
                step_id,
                PlanItemStatus::Todo,
                None,
            )
            .await;
            if turn.error.is_none() {
                report_step_failure(state, &turn.operation_id, &reason);
            }
            return BatchStepOutcome::failed(reason);
        }

        // Reset to a clean, deterministic basis before the corrective attempt.
        clear_auto_context(state).await;
        preload_step_files(state, repo_root, &item).await;

        if let Some((next_idx, tier)) = next_rung {
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                from_tier = ladder_idx,
                to_tier = next_idx,
                to_model = %tier.model,
                effort = ?tier.effort,
                reason = %reason,
                "plan step coder escalation"
            );
            ladder_idx = next_idx;
            selection = ModelSelection::Specific(tier.model);
            reasoning_effort = tier.effort;
        }

        prompt = tauqe_core::plan::prompt::format_plan_step_retry_prompt(
            &turn.assistant_text,
            turn.error.as_deref(),
            protocol,
            marker.as_deref(),
        );
        attempt += 1;
    }
}

/// Diagnostic reported when automatic escalation cannot advance beyond the rung a
/// plan step runs on.
///
/// The step ends with this failure instead of retrying, because the ladder is
/// already saturated: either the developer started the step manually on a model the
/// ladder excludes (`level_up = false`), or the run sits on the strongest eligible
/// rung. The verdict reads the typed exclusion of the coder role, never the model
/// name.
fn escalation_saturated_message(
    models_config: &ModelsConfig,
    model: &ModelRef,
    item: &PlanItem,
    reason: &str,
) -> String {
    let saturation = if models_config.excluded_from_escalation(ModelRole::Coder, model) {
        format!(
            "`{}` is excluded from the automatic ladder (`level_up = false`)",
            model
        )
    } else {
        format!(
            "`{}` is already the strongest rung of the automatic ladder",
            model
        )
    };
    format!(
        "Coder escalation is exhausted for step '{}' ({}): {}. The failure is terminal, so no stronger coder tier is attempted and the step is not retried.\n\nCause: {}",
        item.id, item.title, saturation, reason
    )
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
    /// Coder selection the batch starts from, applied to every step it runs.
    pub(super) model: Option<&'a ModelSelection>,
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
            self.model,
        )
        .await)
    }
}
