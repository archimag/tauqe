use std::collections::HashSet;
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
    errors, events, BatchFinishStatus, BatchFinishSummary, ContextAccess, Event, ModelDeltaEvent,
    ModelErrorEvent, ModelFinishedEvent, ModelRole, ModelSelection, ModelStartedEvent, Plan,
    PlanBatchFinishedEvent, PlanDeleteParams, PlanExecuteParams, PlanGetParams, PlanGetResult,
    PlanItem, PlanItemStatus, PlanListChangedEvent, PlanListResult, PlanRefineParams,
    PlanRefineResult, PlanSaveParams, PlanScopeEvaluation, PlanSetActiveParams, PlanUpdateItemParams,
    PlanUpdatedEvent, Request, Response, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::{mpsc, watch};

use crate::state::{AppState, OperationGuard};
use crate::transport::OutChannel;

fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

/// Returns the first duplicate or empty step ID found in a plan item tree, if any.
///
/// Architect and client payloads are not trusted: a repeated ID would make
/// `Plan::find_item` ambiguous and let one step silently shadow another, while an
/// empty ID cannot be addressed at all. Such a tree is rejected before persistence.
pub(crate) fn find_duplicate_item_ids(items: &[PlanItem]) -> Option<String> {
    fn walk(items: &[PlanItem], seen: &mut HashSet<String>) -> Option<String> {
        for item in items {
            let key = item.id.trim().to_ascii_lowercase();
            if key.is_empty() || !seen.insert(key) {
                return Some(item.id.clone());
            }
            if let Some(duplicate) = walk(&item.children, seen) {
                return Some(duplicate);
            }
        }
        None
    }
    walk(items, &mut HashSet::new())
}

/// Rejects plan IDs that are empty or contain path separators.
///
/// `PlanStorage` maps a plan ID onto `<repo>/.tauqe/plans/<id>.json` and sanitizes it
/// before building the path, so directory traversal is already blocked. Rejecting
/// separator-bearing and empty IDs at the boundary keeps the persisted ID and its file
/// name consistent and gives a deterministic error.
fn is_valid_plan_id(id: &str) -> bool {
    !id.trim().is_empty() && !id.contains('/') && !id.contains('\\')
}

pub async fn handle_plan_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::load_all(&repo_root) {
        Ok(plans) => {
            Response::ok_typed(req.id, &PlanListResult { plans, active_id: None })
        }
        Err(err) => Response::err(req.id, errors::PLAN_LIST_FAILED, format!("{:#}", err)),
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
            Err(err) => Response::err(req.id, errors::PLAN_GET_FAILED, format!("{:#}", err)),
        },
        None => Response::ok_typed(req.id, &PlanGetResult { plan: None }),
    }
}

pub async fn handle_plan_save(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan payload"),
    };

    if !is_valid_plan_id(&params.plan.id) {
        return Response::err(
            req.id,
            errors::INVALID_PLAN_ID,
            "Plan ID must be non-empty and must not contain path separators",
        );
    }
    if let Some(duplicate) = find_duplicate_item_ids(&params.plan.items) {
        return Response::err(
            req.id,
            errors::INVALID_PLAN_TREE,
            format!("Plan contains a duplicate or empty step ID: '{}'", duplicate),
        );
    }

    // Normalize before persisting so the emitted event matches the stored plan.
    let mut plan = params.plan;
    plan.normalize_leaf_files();

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_plan(&repo_root, &plan) {
        return Response::err(req.id, errors::PLAN_SAVE_FAILED, format!("{:#}", err));
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent { plan: plan.clone() }).ok(),
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
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid update item params"),
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
            errors::NOT_FOUND,
            format!("Item '{}' not found in plan '{}'", params.item_id, params.plan_id),
        ),
        Err(err) => Response::err(req.id, errors::PLAN_UPDATE_FAILED, format!("{:#}", err)),
    }
}

pub async fn handle_plan_delete(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan ID"),
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
        Err(err) => Response::err(req.id, errors::PLAN_DELETE_FAILED, format!("{:#}", err)),
    }
}

pub async fn handle_plan_set_active(req: Request, state: &Arc<AppState>) -> Response {
    // A present payload must parse cleanly: a silent fallback to the default would
    // clear the active plan when the payload is malformed.
    let params: PlanSetActiveParams = match req.params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid set active parameters: {}", err),
                );
            }
        },
        None => PlanSetActiveParams::default(),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_active_id(&repo_root, params.id.as_deref()) {
        return Response::err(req.id, errors::PLAN_SET_ACTIVE_FAILED, format!("{:#}", err));
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
    Stopped {
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
        serde_json::to_value(ModelErrorEvent {
            operation_id: operation_id.to_string(),
            message: message.to_string(),
        })
        .ok(),
    );
}

/// Loads the declared target and reference files of a step into the auto context
/// layer before the turn starts. Target files are registered as editable, while
/// reference files are registered as read-only. Declared step files are never
/// capped by an artificial quota.
pub(crate) async fn preload_step_files(state: &Arc<AppState>, repo_root: &Path, item: &PlanItem) {
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

/// Resolves the LLM provider for architect plan operations (refine and replan),
/// consulting the test override in test configurations before falling back to
/// network provider construction.
pub(crate) async fn resolve_architect_provider(
    _state: &Arc<AppState>,
    cfg: &tauqe_core::config::AppConfig,
    provider_kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    {
        if let Some(p) = _state.provider_override.lock().await.clone() {
            return Ok(p);
        }
    }
    create_provider(provider_kind, cfg)
        .map(Arc::from)
        .map_err(|err| err.to_string())
}

pub(crate) struct ArchitectStreamOutcome {
    pub(crate) raw_text: String,
    pub(crate) streamed_explanation_bytes: usize,
}

pub(crate) enum ArchitectStreamError {
    Cancelled,
    ModelError(String),
}

/// Executes a streaming structured-chat turn with the architect model, streaming
/// reasoning deltas and explanation text deltas to the client, handling cancellation
/// and errors, and committing token usage to session state.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_architect_stream(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: String,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    operation: &OperationGuard,
    op_id: &str,
) -> Result<ArchitectStreamOutcome, ArchitectStreamError> {
    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
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

    operation.set_abort_handle(stream_task.abort_handle());

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
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: raw_text.clone(),
                                })
                                .ok(),
                            });
                        }
                    }
                } else if passthrough == Some(true) {
                    state.out.send_event(&Event {
                        method: events::MODEL_TEXT_DELTA.to_string(),
                        params: serde_json::to_value(ModelDeltaEvent {
                            operation_id: op_id.to_string(),
                            delta,
                        })
                        .ok(),
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
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: d,
                                })
                                .ok(),
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
        stream_task.abort();
        state.out.send_event(&Event {
            method: events::MODEL_CANCELLED.to_string(),
            params: Some(serde_json::json!({ "operation_id": op_id })),
        });
        return Err(ArchitectStreamError::Cancelled);
    }
    if let Some(err) = failure {
        stream_task.abort();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err));
    }

    if let Err(join_err) = stream_task.await {
        let err_msg = join_err.to_string();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err_msg.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err_msg));
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
            operation_id: op_id.to_string(),
            usage,
            session_total_cost: total_cost,
            current_cost: Some(cost),
        })
        .ok(),
    });

    Ok(ArchitectStreamOutcome {
        raw_text,
        streamed_explanation_bytes,
    })
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
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
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
    let plan = match PlanStorage::load_plan(repo_root, plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return BatchStepOutcome::failed(format!("Plan '{}' not found", plan_id)),
        Err(err) => return BatchStepOutcome::failed(format!("{:#}", err)),
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
    set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::InProgress);
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
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
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
        let policy = crate::handlers::model::build_apply_policy(state, workflow);

        let plan_context = tauqe_protocol::ModelTurnPlanContext {
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            batch_step_ids: batch_step_ids.to_vec(),
        };

        let turn_result = crate::handlers::model::execute_model_turn(
            state,
            prompt.clone(),
            policy,
            crate::handlers::model::StepTurnOptions {
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
                set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
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
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Blocked);
            report_step_failure(state, &turn.operation_id, &reason);
            return BatchStepOutcome::blocked(reason);
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Done);
            return BatchStepOutcome::succeeded();
        }

        // A cancelled turn ends the batch deterministically: the step returns to
        // TODO and the supervisor stops with `BatchStop::Cancelled` instead of
        // reporting a spurious failure.
        if turn.is_cancelled {
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
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
            set_step_status(state, repo_root, plan_id, step_id, PlanItemStatus::Todo);
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
struct PlanStepExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    plan_id: &'a str,
    batch_step_ids: &'a [String],
    operation: &'a Arc<OperationGuard>,
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

/// Runs the plan steps through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Steps completed before the first
/// failure stay DONE. When a coder step reports a semantic block (`<blocked>`),
/// an automated closed-loop replanning cycle engages: the architect synthesizes a
/// new horizon around the failure diagnostics up to `max_replans` times.
/// Once the supervisor terminates, a typed `plan/batch_finished` event carries
/// the aggregate status, step counters, and elapsed time.
async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    mut current_step_ids: Vec<String>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let _ = PlanStorage::save_active_id(&repo_root, Some(plan_id));

    tracing::info!(
        plan_id = plan_id,
        scheduled = current_step_ids.len(),
        "plan batch started"
    );

    let started_at = std::time::Instant::now();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The whole batch shares one operation and one cancel channel: a cancel
    // delivered between steps is honored instead of being lost, and the slot
    // stays registered for the batch's entire lifetime.
    let cancel_rx = operation.cancel_receiver();
    // Soft stop ends the queue gracefully after the current step: the operation
    // owns the signal, and a `model/cancel` request with `mode: "soft"` raises it
    // so the current step finishes its turn, verification, and commit before the
    // queue stops.
    let soft_stop_rx = operation.soft_stop_receiver();

    let max_replans = state.config.lock().await.config.develop.max_replans;
    let mut replans_count = 0usize;
    let mut total_executed_count = 0usize;
    let mut total_completed = 0usize;
    let mut total_scheduled = current_step_ids.len();

    let final_stop;
    let mut last_error: Option<String> = None;

    loop {
        if *cancel_rx.borrow() {
            final_stop = BatchStop::Cancelled;
            break;
        }

        if current_step_ids.is_empty() {
            final_stop = BatchStop::Completed;
            break;
        }

        let steps: Vec<BatchStep> = current_step_ids
            .iter()
            .map(|id| BatchStep::new(id.clone(), format!("Execute plan step {}", id)))
            .collect();

        let mut executor = PlanStepExecutor {
            state,
            repo_root: &repo_root,
            plan_id,
            batch_step_ids: &current_step_ids,
            operation: &operation,
        };

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        let run_executed = report.executed.len();
        total_executed_count += run_executed;

        match report.stop {
            BatchStop::Completed => {
                total_completed += run_executed;
                final_stop = BatchStop::Completed;
                break;
            }
            BatchStop::Cancelled => {
                total_completed += report
                    .executed
                    .iter()
                    .filter(|r| r.outcome.is_success())
                    .count();
                final_stop = BatchStop::Cancelled;
                break;
            }
            BatchStop::Stopped => {
                total_completed += report
                    .executed
                    .iter()
                    .filter(|r| r.outcome.is_success())
                    .count();
                final_stop = BatchStop::Stopped;
                break;
            }
            BatchStop::Failed { step_id } => {
                total_completed += run_executed.saturating_sub(1);
                last_error = report.executed.last().and_then(|r| r.outcome.error.clone());
                final_stop = BatchStop::Failed { step_id };
                break;
            }
            BatchStop::Blocked { step_id, reason } => {
                total_completed += run_executed.saturating_sub(1);

                if replans_count < max_replans {
                    replans_count += 1;

                    tracing::warn!(
                        plan_id = plan_id,
                        blocked_step_id = step_id.as_str(),
                        replan_round = replans_count,
                        max_replans = max_replans,
                        reason = %reason,
                        "automated replanning started"
                    );

                    emit(
                        &state.out,
                        events::TURN_PHASE,
                        serde_json::to_value(TurnPhaseEvent {
                            operation_id: format!("op-{}", crate::state::next_operation_id()),
                            phase: TurnPhase::Proposal,
                            round: Some(replans_count),
                            max_rounds: Some(max_replans),
                            detail: Some(format!(
                                "Automated replanning round {}/{} around blocked step #{}",
                                replans_count, max_replans, step_id
                            )),
                        })
                        .ok(),
                    );

                    let plan = match PlanStorage::load_plan(&repo_root, plan_id) {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            let err_msg = format!("Plan '{}' not found", plan_id);
                            last_error = Some(err_msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: err_msg };
                            break;
                        }
                        Err(err) => {
                            let err_msg = format!("{:#}", err);
                            last_error = Some(err_msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: err_msg };
                            break;
                        }
                    };

                    let diagnostics = PlanHorizonDiagnosticsOwned {
                        blocked_step_id: step_id.clone(),
                        blocked_reason: Some(reason.clone()),
                        toolchain_status: None,
                    };

                    let res = execute_plan_horizon_synthesis(
                        tauqe_protocol::RequestId::Null,
                        state,
                        plan,
                        Some(diagnostics),
                        None,
                        None,
                        "Automated Plan Replanning",
                        &operation,
                    )
                    .await;

                    if *cancel_rx.borrow() {
                        final_stop = BatchStop::Cancelled;
                        break;
                    }

                    if let Some(err) = res.error {
                        if err.code == errors::CANCELLED {
                            final_stop = BatchStop::Cancelled;
                            break;
                        }
                        let failure_msg = format!(
                            "Automated replanning failed around step '{}': {}",
                            step_id, err.message
                        );
                        last_error = Some(failure_msg.clone());
                        final_stop = BatchStop::Blocked {
                            step_id,
                            reason: failure_msg,
                        };
                        break;
                    }

                    let refine_res = res
                        .result
                        .and_then(|v| serde_json::from_value::<PlanRefineResult>(v).ok());

                    match refine_res {
                        Some(PlanRefineResult::Success { plan: updated_plan, .. }) => {
                            match updated_plan.evaluate_scope(None) {
                                PlanScopeEvaluation::Ready(new_steps) => {
                                    current_step_ids =
                                        new_steps.into_iter().map(|s| s.id).collect();
                                    total_scheduled = total_completed + current_step_ids.len();
                                    tracing::info!(
                                        plan_id = plan_id,
                                        new_horizon_steps = current_step_ids.len(),
                                        replan_round = replans_count,
                                        "automated replanning produced a new horizon"
                                    );
                                    continue;
                                }
                                PlanScopeEvaluation::AllCompleted => {
                                    final_stop = BatchStop::Completed;
                                    break;
                                }
                                PlanScopeEvaluation::BlockedByDiscussion(items) => {
                                    let msg = format!(
                                        "Scope contains item(s) in DISCUSSION status: {}. Review them first",
                                        items.join(", ")
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                PlanScopeEvaluation::BlockedByBlockers(items) => {
                                    let msg = format!(
                                        "Scope contains step(s) in BLOCKED status: {}. Manual intervention required",
                                        items.join(", ")
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                PlanScopeEvaluation::BlockedByDependencies(items) => {
                                    let msg = format!(
                                        "Scope contains step(s) whose prerequisites are not DONE: {}",
                                        items.join(", ")
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                other => {
                                    let msg = format!(
                                        "Replanned scope cannot be executed: {:?}",
                                        other
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                            }
                        }
                        Some(PlanRefineResult::Blocked { reason: architect_reason }) => {
                            let msg = format!(
                                "Architect reported blockers during automated replanning for step '{}': {}",
                                step_id, architect_reason
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: msg };
                            break;
                        }
                        None => {
                            let msg = format!(
                                "Automated replanning returned unrecognized result for step '{}'",
                                step_id
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: msg };
                            break;
                        }
                    }
                } else {
                    let limit_msg = format!(
                        "Automated replanning limit reached (max_replans = {}). Step '{}' remains blocked: {}. Manual developer intervention required.",
                        max_replans, step_id, reason
                    );
                    last_error = Some(limit_msg.clone());
                    final_stop = BatchStop::Blocked {
                        step_id,
                        reason: limit_msg,
                    };
                    break;
                }
            }
        }
    }

    tracing::info!(
        plan_id = plan_id,
        executed = total_executed_count,
        scheduled = total_scheduled,
        stop = ?final_stop,
        "plan batch supervisor finished"
    );

    let (status, error) = match &final_stop {
        BatchStop::Completed => (BatchFinishStatus::Completed, None),
        BatchStop::Failed { .. } => (
            BatchFinishStatus::Failed,
            last_error.or_else(|| Some("step failed without diagnostics".to_string())),
        ),
        BatchStop::Blocked { reason, .. } => (BatchFinishStatus::Blocked, Some(reason.clone())),
        BatchStop::Cancelled => (BatchFinishStatus::Cancelled, None),
        BatchStop::Stopped => (BatchFinishStatus::Stopped, None),
    };

    let outcome = match final_stop {
        BatchStop::Completed => BatchOutcome::Completed {
            steps: total_completed,
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: total_completed,
        },
        BatchStop::Stopped => BatchOutcome::Stopped {
            completed: total_completed,
        },
        BatchStop::Failed { step_id } => {
            let reason = error
                .clone()
                .unwrap_or_else(|| "step failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed: total_completed,
                step_id,
                reason,
            }
        }
        BatchStop::Blocked { step_id, reason } => {
            BatchOutcome::Blocked {
                completed: total_completed,
                step_id,
                reason,
            }
        }
    };

    emit(
        &state.out,
        events::PLAN_BATCH_FINISHED,
        serde_json::to_value(PlanBatchFinishedEvent {
            operation_id: format!("op-{}", crate::state::next_operation_id()),
            summary: BatchFinishSummary {
                owner_id: plan_id.to_string(),
                status,
                executed_count: total_executed_count,
                total_count: total_scheduled.max(total_executed_count),
                elapsed_seconds: started_at.elapsed().as_secs(),
                error,
            },
        })
        .ok(),
    );

    outcome
}

pub async fn handle_plan_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan execute params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let plan = match PlanStorage::load_plan(&repo_root, &params.plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Response::err(req.id, errors::NOT_FOUND, format!("Plan '{}' not found", params.plan_id)),
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, format!("{:#}", err)),
    };

    let steps = match plan.evaluate_scope(params.step_id.as_deref()) {
        PlanScopeEvaluation::Ready(steps) => steps,
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            return Response::err(
                req.id,
                errors::STEP_IN_DISCUSSION,
                format!(
                    "Scope contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::BlockedByBlockers(items) => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED,
                format!(
                    "Scope contains step(s) in BLOCKED status: {}. Resolve the reported blockers and replan before executing",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::BlockedByDependencies(items) => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED_BY_DEPENDENCIES,
                format!(
                    "Scope contains step(s) whose prerequisites are not yet DONE: {}. Complete the prerequisites first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::StepBlocked => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED,
                "Target step is BLOCKED; resolve the reported blocker and replan before executing",
            );
        }
        PlanScopeEvaluation::AllCompleted => {
            return Response::err(req.id, errors::NOTHING_TO_EXECUTE, "All steps in this scope are already completed");
        }
        PlanScopeEvaluation::Cancelled => {
            return Response::err(req.id, errors::STEP_CANCELLED, "Step is CANCELLED and cannot be executed");
        }
        PlanScopeEvaluation::NotFound => {
            return Response::err(req.id, errors::NOT_FOUND, "Target item not found in plan");
        }
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    // Pre-flight verification runs while the batch operation stays registered,
    // so the slot is never briefly free and a concurrent `config/set`,
    // `git/undo`, or a second `plan/execute` cannot slip in mid-verification.
    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any step.\n\n{}",
            report
        );
        return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
    }

    // Execution hands control to the Coder role: the discussion focus is reset
    // unconditionally so no discussion anchor survives into code generation.
    state.clear_discussion_focus().await;

    let step_ids: Vec<String> = steps.iter().map(|s| s.id.clone()).collect();
    let response = Response::ok(
        req.id,
        serde_json::json!({ "plan_id": plan.id, "step_ids": step_ids }),
    );

    let state = Arc::clone(state);
    let plan_id = plan.id.clone();
    tokio::spawn(async move {
        match run_plan_batch(&state, &plan_id, step_ids, operation).await {
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
            BatchOutcome::Stopped { completed } => {
                tracing::info!(
                    "Plan '{}' batch stopped gracefully after {} completed step(s)",
                    plan_id,
                    completed
                );
            }
        }
    });

    response
}

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub struct PlanHorizonDiagnosticsOwned {
    pub blocked_step_id: String,
    pub blocked_reason: Option<String>,
    pub toolchain_status: Option<String>,
}

/// Unified engine for architect plan horizon synthesis.
///
/// Drives streaming structured communication with the architect model, runs multi-round
/// context discovery on `needs_context`, validates the synthesized horizon, and applies it
/// atomically via `PlanStorage::replace_uncompleted_horizon`.
///
/// Enforces strict session history transactionality: `state.history.record_response` is
/// called strictly AFTER `replace_uncompleted_horizon` succeeds on disk. Any persistence
/// or validation error returns early without poisoning the interaction audit trail.
#[allow(clippy::too_many_arguments)]
async fn execute_plan_horizon_synthesis(
    req_id: tauqe_protocol::RequestId,
    state: &Arc<AppState>,
    plan: Plan,
    diagnostics: Option<PlanHorizonDiagnosticsOwned>,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    operation_title: &str,
    operation: &OperationGuard,
) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    tracing::info!(
        plan_id = %plan.id,
        operation = operation_title,
        replanning = diagnostics.is_some(),
        blocked_step = ?diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
        "plan horizon synthesis started"
    );

    let (
        provider,
        model,
        reasoning_effort,
        alias,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        let architect_entry = cfg
            .models
            .role_entries(ModelRole::Architect)
            .into_iter()
            .next();
        let model = match &custom_model {
            Some(m) => cfg.resolve_model(m),
            None => cfg.resolve_role(ModelRole::Architect),
        };
        let reasoning_effort = cfg.models.resolve_effort(ModelRole::Architect);
        let alias = if custom_model.is_some() {
            None
        } else {
            architect_entry.and_then(|entry| entry.alias)
        };
        let provider = match resolve_architect_provider(state, &cfg, model.provider).await {
            Ok(p) => p,
            Err(err) => return Response::err(req_id, errors::NO_API_KEY, err),
        };
        (
            provider,
            model,
            reasoning_effort,
            alias,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
        )
    };

    clear_auto_context(state).await;

    let op_id = format!("op-{}", crate::state::next_operation_id());

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role: Some(ModelRole::Architect),
            alias,
            effort: reasoning_effort,
            plan_context: None,
            is_batch: false,
        })
        .ok(),
    });

    let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}] {}\n*Replanning uncompleted horizon around blocked step `{}`...*\n\n---\n\n",
            operation_title, plan.id, plan.title, diag.blocked_step_id
        ),
        None => format!(
            "# {}: [{}] {}\n*Analyzing codebase contours and decomposing architectural plan...*\n\n---\n\n",
            operation_title, plan.id, plan.title
        ),
    };
    state.out.send_event(&Event {
        method: events::MODEL_TEXT_DELTA.to_string(),
        params: serde_json::to_value(ModelDeltaEvent {
            operation_id: op_id.clone(),
            delta: intro_header.clone(),
        })
        .ok(),
    });

    {
        let ctx = state.context.read().await;
        let context_paths: Vec<String> = ctx
            .get_state()
            .items
            .into_iter()
            .map(|it| it.path)
            .collect();
        let mut history_turn_prompt =
            format!("# {}: [{}] {}\n", operation_title, plan.id, plan.title);
        if let Some(ref diag) = diagnostics {
            history_turn_prompt.push_str(&format!("\nBlocked Step: #{}", diag.blocked_step_id));
            if let Some(ref r) = diag.blocked_reason {
                history_turn_prompt.push_str(&format!("\nReason: {}", r));
            }
        }
        if let Some(ins) = &instructions {
            let trimmed = ins.trim();
            if !trimmed.is_empty() {
                history_turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        let _ = state
            .history
            .lock()
            .await
            .record_turn(history_turn_prompt, context_paths);
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
                detail: Some(operation_title.to_string()),
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

        let sys_prompt = tauqe_core::plan::prompt::build_plan_horizon_system_prompt();
        let diag_ref = diagnostics
            .as_ref()
            .map(|d| tauqe_core::plan::prompt::PlanHorizonDiagnostics {
                blocked_step_id: &d.blocked_step_id,
                blocked_reason: d.blocked_reason.as_deref(),
                toolchain_status: d.toolchain_status.as_deref(),
            });

        let mut user_prompt = tauqe_core::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            &context_files,
            repo_map.as_deref(),
            diag_ref,
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
                return Response::err(req_id, errors::SCHEMA_ERROR, err.to_string());
            }
        };
        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: schema_def,
        });

        let stream_outcome = match run_architect_stream(
            state,
            Arc::clone(&provider),
            model.name.clone(),
            messages,
            response_format,
            reasoning_effort,
            operation,
            &op_id,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(ArchitectStreamError::Cancelled) => {
                clear_auto_context(state).await;
                return Response::err(
                    req_id,
                    errors::CANCELLED,
                    format!("{} was cancelled", operation_title),
                );
            }
            Err(ArchitectStreamError::ModelError(err)) => {
                clear_auto_context(state).await;
                return Response::err(req_id, errors::MODEL_ERROR, err);
            }
        };

        let ArchitectStreamOutcome {
            raw_text,
            streamed_explanation_bytes,
        } = stream_outcome;

        let output = match tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text) {
            Ok(out) => out,
            Err(err) => {
                clear_auto_context(state).await;
                let err_msg = format!("Failed to parse architect refinement output: {err}");
                state.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: serde_json::to_value(ModelErrorEvent {
                        operation_id: op_id.clone(),
                        message: err_msg.clone(),
                    })
                    .ok(),
                });
                return Response::err(req_id, errors::PARSE_ERROR, err_msg);
            }
        };

        if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
            state.out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: output.explanation.clone(),
                })
                .ok(),
            });
        }
        accumulated_full_text.push_str(&output.explanation);

        if output.status.eq_ignore_ascii_case("needs_context")
            && discovery_round < max_discovery_rounds
        {
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
                    files_token_budget,
                );
                let s = ctx.get_state();
                (oc, s)
            };

            if let Some(limit) = outcome.limit_exceeded {
                let warn_msg = format!(
                    "Context files token budget exceeded (files_token_budget = {}). Architect requested {} new token(s) in file(s) [{}], but context already has {} tokens. The plan is too broad and should be manually decomposed or scoped down.",
                    limit.token_budget,
                    limit.requested_tokens,
                    limit.requested_files.join(", "),
                    limit.current_tokens
                );
                tracing::warn!("{}", warn_msg);
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: serde_json::to_value(ModelDeltaEvent {
                        operation_id: op_id.clone(),
                        delta: format!("\n\n⚠️ {}\n\n", warn_msg),
                    })
                    .ok(),
                });
                state.out.send_event(&Event {
                    method: events::MODEL_FINISHED.to_string(),
                    params: serde_json::to_value(ModelFinishedEvent {
                        operation_id: op_id.clone(),
                        full_text: format!("{}\n\n⚠️ {}", accumulated_full_text, warn_msg),
                    })
                    .ok(),
                });
                clear_auto_context(state).await;
                let _ = state.history.lock().await.record_response(
                    warn_msg.clone(),
                    None,
                    Some(format!(
                        "{}: Token Budget Exceeded ({})",
                        operation_title, plan.title
                    )),
                    Vec::new(),
                );
                return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason: warn_msg });
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
            tracing::info!(
                plan_id = %plan.id,
                round = discovery_round,
                added = outcome.added.len(),
                missing = outcome.missing.len(),
                "architect discovery round"
            );
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
                params: serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: round_banner,
                })
                .ok(),
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

    state.out.send_event(&Event {
        method: events::MODEL_FINISHED.to_string(),
        params: serde_json::to_value(ModelFinishedEvent {
            operation_id: op_id.clone(),
            full_text: accumulated_full_text,
        })
        .ok(),
    });

    clear_auto_context(state).await;

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        tracing::warn!(
            plan_id = %plan.id,
            reason = %reason,
            "architect reported blockers"
        );
        let blocked_msg =
            format!("Architect reported blockers for plan [{}]:\n\n{}", plan.id, reason);
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("{}: Blocked ({})", operation_title, plan.title)),
            Vec::new(),
        );
        return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Response::err(
            req_id,
            errors::NEEDS_CONTEXT,
            format!(
                "Architect requested additional context but no discovery rounds remain (limit: {}). Narrow the plan scope or raise the discovery round limit, then refine again",
                max_discovery_rounds
            ),
        );
    }

    // Accept only an explicit success: an unknown status (for example a stray
    // `needs_context` that survived the round limit) must never be treated as a
    // completed refinement, or it could overwrite the plan with an unvalidated tree.
    if !output.status.eq_ignore_ascii_case("success") {
        return Response::err(
            req_id,
            errors::UNEXPECTED_STATUS,
            format!(
                "Architect returned unsupported status '{}'; expected 'success' or 'blocked'",
                output.status
            ),
        );
    }

    if output.items.is_empty() {
        return Response::err(
            req_id,
            errors::EMPTY_ITEMS,
            "Model returned success status but an empty items array",
        );
    }

    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
        return Response::err(
            req_id,
            errors::INVALID_PLAN_TREE,
            format!(
                "Architect returned a plan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        );
    }

    // Replace the uncompleted horizon while preserving the immutable Done foundation.
    let new_steps = output.items.len();
    let updated_plan =
        match PlanStorage::replace_uncompleted_horizon(&repo_root, &plan.id, output.items) {
            Ok(p) => p,
            Err(err) => {
                return Response::err(req_id, errors::PLAN_SAVE_FAILED, format!("{:#}", err))
            }
        };

    // Strict history transactionality: record the assistant response ONLY after
    // the synthesized horizon has been atomically written to disk.
    let (total_items, completed_items) = updated_plan.count_stats();
    tracing::info!(
        plan_id = %updated_plan.id,
        total_items = total_items,
        completed_items = completed_items,
        new_steps = new_steps,
        "plan horizon synthesized"
    );
    let history_response = format!(
        "Plan [{}] horizon synthesized successfully ({} total, {} completed).\n\n{}",
        updated_plan.id, total_items, completed_items, output.explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("{}: {}", operation_title, updated_plan.title)),
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
        req_id,
        &PlanRefineResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        },
    )
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
                let model = obj
                    .get("model")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let instructions = obj
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                (id, model, instructions)
            } else {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Invalid plan refine parameters",
                );
            }
        }
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan refine parameters",
            )
        }
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
            errors::INVALID_PARAMS,
            "Missing plan ID and no active plan is set",
        );
    }

    let plan = match PlanStorage::load_plan(&repo_root, &effective_plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, format!("{:#}", err)),
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    tracing::info!(plan_id = %plan.id, "plan refine requested");

    execute_plan_horizon_synthesis(
        req.id,
        state,
        plan,
        None,
        custom_model,
        instructions,
        "Deep Plan Refinement",
        &operation,
    )
    .await
}

/// Handles an addressed replanning request for a blocked plan step.
///
/// Synthesizes the unfinished horizon of the plan around the failure diagnostics of the
/// blocked step, preserving the completed (DONE) foundation and updating the plan
/// atomically via `execute_plan_horizon_synthesis`.
pub async fn handle_plan_replan(req: Request, state: &Arc<AppState>) -> Response {
    let (plan_id, step_id, blocked_reason, custom_model, instructions) = match req.params {
        Some(val) => match serde_json::from_value::<tauqe_protocol::PlanReplanParams>(val.clone()) {
            Ok(params) => (
                params.plan_id,
                params.step_id,
                params.reason,
                params.model,
                params.instructions,
            ),
            Err(_) => match val.as_object() {
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
                    return Response::err(
                        req.id,
                        errors::INVALID_PARAMS,
                        "Invalid plan replan parameters",
                    )
                }
            },
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan replan parameters",
            )
        }
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
            errors::INVALID_PARAMS,
            "Missing plan ID and no active plan is set",
        );
    }
    if step_id.trim().is_empty() {
        return Response::err(req.id, errors::INVALID_PARAMS, "Missing blocked step ID to replan");
    }

    let plan = match PlanStorage::load_plan(&repo_root, &effective_plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, format!("{:#}", err)),
    };

    let step = match plan.find_item(&step_id) {
        Some(item) => item.clone(),
        None => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Step '{}' not found in plan '{}'", step_id, effective_plan_id),
            );
        }
    };

    if !step.is_leaf() {
        return Response::err(
            req.id,
            errors::NOT_A_LEAF,
            format!(
                "Step '{}' ({}) is not a leaf and cannot be replanned",
                step.id, step.title
            ),
        );
    }
    if step.status == PlanItemStatus::Done {
        return Response::err(
            req.id,
            errors::STEP_IMMUTABLE,
            format!(
                "Step '{}' is DONE and immutable; completed steps cannot be replanned",
                step.id
            ),
        );
    }

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    let diagnostics = PlanHorizonDiagnosticsOwned {
        blocked_step_id: step.id.clone(),
        blocked_reason,
        toolchain_status: None,
    };

    tracing::info!(
        plan_id = %plan.id,
        step_id = step.id.as_str(),
        "plan replan requested"
    );

    execute_plan_horizon_synthesis(
        req.id,
        state,
        plan,
        Some(diagnostics),
        custom_model,
        instructions,
        "Plan Replanning",
        &operation,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};

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

    async fn test_state_with_provider(
        dir: &TestDir,
        config: tauqe_core::config::AppConfig,
        provider: Arc<dyn tauqe_core::providers::LlmProvider>,
    ) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
        *state.provider_override.lock().await = Some(provider);
        state
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
        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome = run_plan_batch(
            &state,
            "batch",
            vec!["1".to_string(), "2".to_string()],
            operation,
        )
        .await;
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
    async fn test_plan_batch_emits_finished_event() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "emit".to_string(),
            title: "Emit".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Todo),
                test_item("2", PlanItemStatus::Todo),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let (out, mut rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        );

        // Without an API key the first turn cannot start: the batch fails at step 1.
        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome =
            run_plan_batch(&state, "emit", vec!["1".to_string(), "2".to_string()], operation)
                .await;
        assert!(matches!(outcome, BatchOutcome::Failed { .. }));

        // The supervisor publishes a single typed completion event for the whole
        // batch, carrying the aggregate status and step counters.
        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }
        let finished = raw_events
            .iter()
            .find(|e| e.contains(events::PLAN_BATCH_FINISHED))
            .expect("plan/batch_finished must be emitted");
        assert!(finished.contains("\"emit\""));
        assert!(finished.contains("\"failed\""));
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

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome =
            run_plan_batch(&state, "skip", vec!["1".to_string(), "2".to_string()], operation)
                .await;
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

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome = run_plan_batch(
            &state,
            "blocked-batch",
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
            operation,
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
        config.models.architect =
            Some(vec![tauqe_protocol::ModelRef::openrouter("test/architect-model").into()]);
        config.models.primary =
            vec![tauqe_protocol::ModelRef::openrouter("test/primary-model").into()];

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
    async fn test_handle_plan_refine_clears_auto_context() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "refine-auto".to_string(),
            title: "Refine Auto".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![test_item("1", PlanItemStatus::Todo)],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        std::fs::write(dir.path().join("file.rs"), "pub fn foo() {}\n").unwrap();

        let refine_output = serde_json::json!({
            "status": "success",
            "explanation": "Refined successfully",
            "items": [
                {
                    "id": "1",
                    "title": "Refined step 1",
                    "status": "todo",
                    "files": ["file.rs"],
                    "readonly_files": [],
                    "concept_refs": [],
                    "depends_on": [],
                    "children": []
                }
            ]
        });
        let mock_provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(refine_output.to_string()),
        ]));

        let config = tauqe_core::config::AppConfig::default();
        let state = test_state_with_provider(&dir, config, mock_provider).await;

        state
            .context
            .write()
            .await
            .add_auto_file("file.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert_eq!(state.context.read().await.auto_files().len(), 1);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
            params: Some(serde_json::json!({ "plan_id": "refine-auto" })),
        };
        let res = handle_plan_refine(req, &state).await;
        assert!(res.error.is_none(), "plan_refine failed: {:?}", res.error);

        assert!(
            state.context.read().await.auto_files().is_empty(),
            "auto context should be cleared after plan refinement"
        );
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
        assert!(!state.is_operation_active());

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
                outcome: tauqe_protocol::EditOutcome {
                    applied: true,
                    error: None,
                    changed_files: vec!["src/lib.rs".to_string()],
                    commit_hash: Some("abc".to_string()),
                },
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
                outcome: tauqe_protocol::EditOutcome {
                    applied: false,
                    error: Some("compiler error".to_string()),
                    changed_files: vec![],
                    commit_hash: None,
                },
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

    #[tokio::test]
    async fn test_handle_plan_replan_with_mock_provider() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "replan-mock".to_string(),
            title: "Replan Mock".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Done),
                test_item("2", PlanItemStatus::Blocked),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let replan_output = serde_json::json!({
            "status": "success",
            "explanation": "Replanned step 2",
            "items": [
                {
                    "id": "2.1",
                    "title": "Replacement step 2.1",
                    "status": "todo",
                    "files": [],
                    "readonly_files": [],
                    "concept_refs": [],
                    "depends_on": [],
                    "children": []
                }
            ]
        });
        let mock_provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(replan_output.to_string()),
        ]));

        let config = tauqe_core::config::AppConfig::default();
        let state = test_state_with_provider(&dir, config, mock_provider).await;

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
            params: Some(serde_json::json!({
                "plan_id": "replan-mock",
                "step_id": "2",
                "reason": "Missing dependency"
            })),
        };
        let res = handle_plan_replan(req, &state).await;
        assert!(res.error.is_none(), "replan failed: {:?}", res.error);

        let reloaded = PlanStorage::load_plan(dir.path(), "replan-mock").unwrap().unwrap();
        // The Done foundation is preserved, the uncompleted horizon is replaced.
        assert_eq!(reloaded.find_item("1").unwrap().status, PlanItemStatus::Done);
        assert!(reloaded.find_item("2.1").is_some());
        assert!(reloaded.find_item("2").is_none());
    }

    #[tokio::test]
    async fn test_plan_horizon_synthesis_history_transactionality() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "tx-history".to_string(),
            title: "Tx History".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![test_item("1", PlanItemStatus::Todo)],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        // Synthesize an invalid horizon that introduces a circular dependency
        // to force `replace_uncompleted_horizon` to fail during persistence.
        let cyclic_output = serde_json::json!({
            "status": "success",
            "explanation": "Cyclic horizon",
            "items": [
                {
                    "id": "a",
                    "title": "Step A",
                    "status": "todo",
                    "depends_on": ["b"],
                    "children": []
                },
                {
                    "id": "b",
                    "title": "Step B",
                    "status": "todo",
                    "depends_on": ["a"],
                    "children": []
                }
            ]
        });
        let mock_provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(cyclic_output.to_string()),
        ]));

        let config = tauqe_core::config::AppConfig::default();
        let state = test_state_with_provider(&dir, config, mock_provider).await;

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
            params: Some(serde_json::json!({ "plan_id": "tx-history" })),
        };
        let res = handle_plan_refine(req, &state).await;

        let err = res.error.expect("should fail due to dependency cycle");
        assert_eq!(err.code, errors::PLAN_SAVE_FAILED);
        assert!(err.message.contains("Dependency cycle detected"));

        // Strict history transactionality invariant: no assistant response was recorded
        // into session history because persistence failed.
        let entries = state.history.lock().await.get_entries().unwrap();
        let responses = entries
            .iter()
            .filter(|e| matches!(e, tauqe_core::history::HistoryEntry::Response { .. }))
            .count();
        assert_eq!(responses, 0, "no response entry should be committed on persistence failure");
    }

    #[tokio::test]
    async fn test_plan_step_execution_uses_turn_marker() {
        let dir = TestDir::new();
        tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::write(dir.path().join("src.rs"), "pub fn compute() -> i32 { 0 }\n").unwrap();
        tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add src", false)
            .unwrap();

        let mut item = test_item("1", PlanItemStatus::Todo);
        item.title = "Implement compute".to_string();
        item.files = vec!["src.rs".to_string()];
        let plan = Plan {
            id: "marker-plan".to_string(),
            title: "Marker Plan".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![item],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let marker = "TESTMARKER1";
        let response = format!(
            "<tauqe_edits_{m} summary=\"Implement compute\">\n<edit_{m} path=\"src.rs\">\n<search_{m}>\npub fn compute() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
            m = marker
        );
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        config.models.auto_level_up = false;

        let state = test_state_with_provider(&dir, config, provider.clone()).await;
        *state.marker_override.lock().await = Some(marker.to_string());

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome = run_plan_batch(&state, "marker-plan", vec!["1".to_string()], operation).await;
        assert!(
            matches!(outcome, BatchOutcome::Completed { steps: 1 }),
            "expected a completed batch, got {:?}",
            outcome
        );

        // Both the system prompt and the step prompt must carry the marker suffix.
        let request = &provider.requests()[0];
        assert!(request.messages[0]
            .content
            .contains(&format!("scratchpad_{}", marker)));
        assert!(request.messages[0]
            .content
            .contains(&format!("success_{}", marker)));

        // The marker-bound edits were applied and the step reached DONE.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
        let reloaded = PlanStorage::load_plan(dir.path(), "marker-plan")
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.find_item("1").unwrap().status, PlanItemStatus::Done);
    }

    #[tokio::test]
    async fn test_plan_batch_automated_replanning_on_blocked_step() {
        let dir = TestDir::new();
        tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
        tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

        let mut item = test_item("1", PlanItemStatus::Todo);
        item.title = "Step 1".to_string();
        item.files = vec!["src.rs".to_string()];
        let plan = Plan {
            id: "auto-replan".to_string(),
            title: "Auto Replan".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![item],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let marker = "AUTOREPLANM1";
        // 1. Coder turn returns a semantic blocked outcome:
        let blocked_response = format!(
            "<blocked_{m} reason=\"Precondition missing: value signature invalid\" />",
            m = marker
        );
        // 2. Architect replanning turn returns a new horizon with replacement step "1.1":
        let replan_output = serde_json::json!({
            "status": "success",
            "explanation": "Replanned step 1 into replacement 1.1",
            "items": [
                {
                    "id": "1.1",
                    "title": "Step 1.1 replacement",
                    "status": "todo",
                    "files": ["src.rs"],
                    "readonly_files": [],
                    "concept_refs": [],
                    "depends_on": [],
                    "children": []
                }
            ]
        });
        // 3. Coder turn for step "1.1" succeeds:
        let success_response = format!(
            "<tauqe_edits_{m} summary=\"Fix value\">\n<edit_{m} path=\"src.rs\">\n<search_{m}>\npub fn value() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn value() -> i32 {{ 100 }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
            m = marker
        );

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(blocked_response),
            MockResponse::text(replan_output.to_string()),
            MockResponse::text(success_response),
        ]));

        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        config.develop.max_replans = 2;
        config.models.auto_level_up = false;

        let state = test_state_with_provider(&dir, config, provider).await;
        *state.marker_override.lock().await = Some(marker.to_string());

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome = run_plan_batch(&state, "auto-replan", vec!["1".to_string()], operation).await;

        assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

        let reloaded = PlanStorage::load_plan(dir.path(), "auto-replan").unwrap().unwrap();
        assert!(reloaded.find_item("1.1").is_some());
        assert_eq!(reloaded.find_item("1.1").unwrap().status, PlanItemStatus::Done);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
            "pub fn value() -> i32 { 100 }\n"
        );
    }

    #[tokio::test]
    async fn test_plan_batch_automated_replanning_limit_exhausted() {
        let dir = TestDir::new();
        tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
        tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

        let mut item = test_item("1", PlanItemStatus::Todo);
        item.title = "Step 1".to_string();
        item.files = vec!["src.rs".to_string()];
        let plan = Plan {
            id: "limit-replan".to_string(),
            title: "Limit Replan".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![item],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let marker = "LIMITREPLANM1";
        // 1. First coder turn blocks:
        let blocked_1 = format!("<blocked_{m} reason=\"First blocker\" />", m = marker);
        // 2. Architect replanning replaces step 1 with 1.1:
        let replan_output = serde_json::json!({
            "status": "success",
            "explanation": "Replanned into 1.1",
            "items": [
                {
                    "id": "1.1",
                    "title": "Step 1.1",
                    "status": "todo",
                    "files": ["src.rs"],
                    "readonly_files": [],
                    "concept_refs": [],
                    "depends_on": [],
                    "children": []
                }
            ]
        });
        // 3. Second coder turn on step 1.1 also blocks:
        let blocked_2 = format!("<blocked_{m} reason=\"Second blocker\" />", m = marker);

        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(blocked_1),
            MockResponse::text(replan_output.to_string()),
            MockResponse::text(blocked_2),
        ]));

        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        // Limit max_replans to 1:
        config.develop.max_replans = 1;
        config.models.auto_level_up = false;

        let state = test_state_with_provider(&dir, config, provider).await;
        *state.marker_override.lock().await = Some(marker.to_string());

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        let outcome = run_plan_batch(&state, "limit-replan", vec!["1".to_string()], operation).await;

        match outcome {
            BatchOutcome::Blocked {
                completed,
                step_id,
                reason,
            } => {
                assert_eq!(completed, 0);
                assert_eq!(step_id, "1.1");
                assert!(
                    reason.contains("Automated replanning limit reached (max_replans = 1)"),
                    "unexpected block reason: {}",
                    reason
                );
                assert!(reason.contains("Manual developer intervention required"));
            }
            other => panic!("Expected Blocked outcome with limit reached, got {:?}", other),
        }

        let reloaded = PlanStorage::load_plan(dir.path(), "limit-replan").unwrap().unwrap();
        assert_eq!(reloaded.find_item("1.1").unwrap().status, PlanItemStatus::Blocked);
    }

    #[tokio::test]
    async fn test_plan_batch_stops_gracefully_on_soft_stop() {
        let dir = TestDir::new();
        let plan = Plan {
            id: "soft-stop".to_string(),
            title: "Soft Stop".to_string(),
            created_at: 1,
            updated_at: 1,
            items: vec![
                test_item("1", PlanItemStatus::Todo),
                test_item("2", PlanItemStatus::Todo),
            ],
            ..Default::default()
        };
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let (out, mut rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        );

        let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
        // A soft stop requested before the batch starts stops the queue before
        // the first step runs.
        state.soft_stop_active_operation();

        let outcome = run_plan_batch(
            &state,
            "soft-stop",
            vec!["1".to_string(), "2".to_string()],
            operation,
        )
        .await;

        assert_eq!(outcome, BatchOutcome::Stopped { completed: 0 });

        // No step was started: every status stays TODO.
        let reloaded = PlanStorage::load_plan(dir.path(), "soft-stop").unwrap().unwrap();
        assert!(reloaded.items.iter().all(|i| i.status == PlanItemStatus::Todo));

        // The supervisor publishes a single typed completion event carrying the
        // graceful "stopped" status.
        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }
        let finished = raw_events
            .iter()
            .find(|e| e.contains(events::PLAN_BATCH_FINISHED))
            .expect("plan/batch_finished must be emitted");
        assert!(finished.contains("\"soft-stop\""));
        assert!(finished.contains("\"stopped\""));
    }
}
