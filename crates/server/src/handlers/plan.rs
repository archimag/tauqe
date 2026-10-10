use std::sync::Arc;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    create_provider, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, Event, PlanDeleteParams, PlanExecuteStepParams, PlanGetParams,
    PlanGetResult, PlanListChangedEvent, PlanListResult, PlanRefineParams, PlanRefineResult,
    PlanSaveParams, PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent, Request, Response,
    TurnPhase, TurnPhaseEvent,
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

pub async fn handle_plan_execute_step(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecuteStepParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan execute step params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let plan = match PlanStorage::load_plan(&repo_root, &params.plan_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Response::err(req.id, "NOT_FOUND", format!("Plan '{}' not found", params.plan_id)),
        Err(err) => return Response::err(req.id, "PLAN_LOAD_FAILED", format!("{:#}", err)),
    };

    let item = match plan.find_item(&params.step_id) {
        Some(i) => i,
        None => return Response::err(req.id, "NOT_FOUND", format!("Step '{}' not found in plan '{}'", params.step_id, params.plan_id)),
    };

    if !item.is_leaf() {
        return Response::err(
            req.id,
            "INVALID_PARAMS",
            format!("Step '{}' ({}) is not a leaf item; execute individual sub-tasks instead", item.id, item.title),
        );
    }

    if item.status == tauqe_protocol::PlanItemStatus::Discussion {
        return Response::err(
            req.id,
            "STEP_IN_DISCUSSION",
            format!("Step '{}' ({}) is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)", item.id, item.title),
        );
    }

    if item.status == tauqe_protocol::PlanItemStatus::Cancelled {
        return Response::err(
            req.id,
            "STEP_CANCELLED",
            format!("Step '{}' ({}) is CANCELLED and cannot be executed", item.id, item.title),
        );
    }

    if let Ok(Some(updated_plan)) = PlanStorage::update_item(
        &repo_root,
        &params.plan_id,
        &params.step_id,
        Some(tauqe_protocol::PlanItemStatus::InProgress),
    ) {
        let _ = PlanStorage::save_active_id(&repo_root, Some(&params.plan_id));
        emit(
            &state.out,
            events::PLAN_UPDATED,
            serde_json::to_value(PlanUpdatedEvent { plan: updated_plan }).ok(),
        );
        if let Ok(plans) = PlanStorage::load_all(&repo_root) {
            emit(
                &state.out,
                events::PLAN_LIST_CHANGED,
                serde_json::to_value(PlanListChangedEvent {
                    plans,
                    active_id: Some(params.plan_id.clone()),
                })
                .ok(),
            );
        }
    }

    let prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(&plan, &params.step_id, "") {
        Ok(p) => p,
        Err(err) => return Response::err(req.id, "PROMPT_BUILD_FAILED", err),
    };

    match crate::handlers::model::start_model_turn(state, prompt, None).await {
        Ok(op_id) => Response::ok(req.id, serde_json::json!({ "operation_id": op_id, "step_id": params.step_id })),
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
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

    let (provider, model, repomap_budget, max_discovery_rounds, max_files) = {
        let cfg = state.config.lock().await;
        let model = match custom_model {
            Some(m) => cfg.resolve_model(&m),
            None => {
                let cur = state.current_selection.lock().await;
                cfg.resolve_model(&cur)
            }
        };
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
                .stream_chat(&model_name, messages, response_format, tx, stream_cancel)
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-plan-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
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
}
