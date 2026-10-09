use std::sync::Arc;

use tokio::sync::watch;
use tauqe_core::edits::{EditProtocolFactory, XmlEditProtocol};
use tauqe_core::providers::{create_provider, LlmProvider, StreamEvent};
use tauqe_core::workflow::{GitEditWorkflow, WorkflowFactory};
use tauqe_protocol::{
    events, EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent,
    EditHunkEvent, EditStartedEvent, Event, GitCommitCreatedEvent, ModelAskParams, ModelResultEvent,
    ModelUsageInfo, Request, Response, ResponseError,
};

use crate::state::{next_operation_id, ActiveOperation, AppState};
use crate::transport::OutChannel;

pub async fn handle_model_ask(req: Request, state: &Arc<AppState>) -> Response {
    let params: ModelAskParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
                    message: "Missing or invalid prompt".to_string(),
                    data: None,
                }),
            };
        }
    };

    match start_model_turn(state, params.prompt).await {
        Ok(op_id) => Response {
            id: req.id,
            result: Some(serde_json::json!({ "operation_id": op_id })),
            error: None,
        },
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}

pub async fn start_model_turn(state: &Arc<AppState>, prompt: String) -> Result<String, ResponseError> {
    let (provider, model, app_config) = {
        let cfg = state.config.lock().await;
        let model = cfg.active_model();
        let provider: Arc<dyn LlmProvider> = match create_provider(model.provider, &cfg) {
            Ok(p) => Arc::from(p),
            Err(err) => {
                return Err(ResponseError {
                    code: "NO_API_KEY".to_string(),
                    message: err.to_string(),
                    data: None,
                });
            }
        };

        (provider, model, cfg.clone())
    };

    let op_id = format!("op-{}", next_operation_id());
    let op_id_event = op_id.clone();

    let started_event = Event {
        method: events::MODEL_STARTED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id_event,
            "model": model,
        })),
    };
    state.out.send_event(&started_event);

    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        *state.active_cancel.lock().await = Some(ActiveOperation {
            cancel_tx: cancel_tx.clone(),
            abort_handle: None,
        });
    }

    let prompt_for_spawn = prompt;
    let state_for_spawn = Arc::clone(state);
    let op_id_for_spawn = op_id.clone();
    let model_for_spawn = model.clone();
    let provider_for_spawn = Arc::clone(&provider);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

    tokio::spawn(async move {
        let protocol = EditProtocolFactory::create_protocol(&app_config.develop.protocol)
            .unwrap_or_else(|_| Box::new(XmlEditProtocol));

        let workflow = WorkflowFactory::create_workflow_from_app_config(
            &app_config.develop.workflow,
            &app_config,
        )
        .unwrap_or_else(|_| {
            Box::new(GitEditWorkflow {
                options: tauqe_core::workflow::WorkflowOptions::from_app_config(&app_config),
            })
        });

        let (wf_tx, wf_rx) = (tx.clone(), cancel_rx.clone());
        drop(tx); // Drop local tx clone so rx closes when wf_task finishes

        let state_clone = Arc::clone(&state_for_spawn);
        let model_clone = model_for_spawn.clone();
        let prompt_clone = prompt_for_spawn.clone();

        let workflow_future = async move {
            let _session_guard = state_clone.session_lock.lock().await;
            let mut local_context = state_clone.context.read().await.clone();
            let mut history = state_clone.history.lock().await;
            let res = workflow
                .execute(
                    &prompt_clone,
                    provider_for_spawn.as_ref(),
                    &model_clone,
                    &mut local_context,
                    &mut history,
                    protocol.as_ref(),
                    wf_tx,
                    wf_rx,
                )
                .await;
            state_clone.context.write().await.merge_turn_context(&local_context);
            res
        };

        let wf_task = tokio::spawn(workflow_future);
        {
            if let Some(ref mut op) = *state_for_spawn.active_cancel.lock().await {
                op.abort_handle = Some(wf_task.abort_handle());
            }
        }

        let mut usage_info = None;
        let mut edit_events_sent = false;
        let mut edit_finished_sent = false;
        let mut is_cancelled = false;
        let mut is_error = false;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ReasoningDelta(delta) => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::MODEL_REASONING_DELTA.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id_for_spawn,
                            "delta": delta,
                        })),
                    });
                }
                StreamEvent::TextDelta(delta) => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::MODEL_TEXT_DELTA.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id_for_spawn,
                            "delta": delta,
                        })),
                    });
                }
                StreamEvent::ContextChanged(ctx_state) => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::CONTEXT_CHANGED.to_string(),
                        params: Some(serde_json::json!({ "state": ctx_state })),
                    });
                }
                StreamEvent::EditStarted => {
                    edit_events_sent = true;
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_STARTED.to_string(),
                        params: serde_json::to_value(EditStartedEvent {
                            operation_id: op_id_for_spawn.clone(),
                        })
                        .ok(),
                    });
                }
                StreamEvent::EditFileStarted { path, op_type } => {
                    edit_events_sent = true;
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_FILE_STARTED.to_string(),
                        params: serde_json::to_value(EditFileStartedEvent {
                            operation_id: op_id_for_spawn.clone(),
                            path,
                            op_type,
                        })
                        .ok(),
                    });
                }
                StreamEvent::EditHunk {
                    path,
                    hunk_index,
                    old_text,
                    new_text,
                } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_HUNK.to_string(),
                        params: serde_json::to_value(EditHunkEvent {
                            operation_id: op_id_for_spawn.clone(),
                            path,
                            hunk_index,
                            old_text,
                            new_text,
                        })
                        .ok(),
                    });
                }
                StreamEvent::EditFileDone {
                    path,
                    status,
                    error,
                    hunks_count,
                } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_FILE_DONE.to_string(),
                        params: serde_json::to_value(EditFileDoneEvent {
                            operation_id: op_id_for_spawn.clone(),
                            path,
                            status,
                            error,
                            hunks_count,
                        })
                        .ok(),
                    });
                }
                StreamEvent::EditFileRetrying {
                    path,
                    attempt,
                    max_retries,
                    reason,
                } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_FILE_RETRYING.to_string(),
                        params: serde_json::to_value(EditFileRetryingEvent {
                            operation_id: op_id_for_spawn.clone(),
                            path,
                            attempt,
                            max_retries,
                            reason,
                        })
                        .ok(),
                    });
                }
                StreamEvent::TurnPhase {
                    phase,
                    round,
                    max_rounds,
                    detail,
                } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::TURN_PHASE.to_string(),
                        params: serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                            operation_id: op_id_for_spawn.clone(),
                            phase,
                            round,
                            max_rounds,
                            detail,
                        })
                        .ok(),
                    });
                }
                StreamEvent::ToolchainStarted { command } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::TOOLCHAIN_STARTED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::ToolchainStartedEvent {
                            operation_id: op_id_for_spawn.clone(),
                            command,
                        })
                        .ok(),
                    });
                }
                StreamEvent::ToolchainFinished {
                    command,
                    success,
                    message,
                } => {
                    state_for_spawn.out.send_event(&Event {
                        method: events::TOOLCHAIN_FINISHED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                            operation_id: op_id_for_spawn.clone(),
                            command,
                            success,
                            message,
                        })
                        .ok(),
                    });
                }
                    StreamEvent::Usage(usage) => {
                    accumulate_usage(&mut usage_info, usage.clone());
                    let op_accumulated_cost = usage_info.as_ref().and_then(|u| u.cost);
                    let current_session_cost = {
                        let base_cost = *state_for_spawn.total_cost.lock().await;
                        base_cost + op_accumulated_cost.unwrap_or(0.0)
                    };
                    state_for_spawn.out.send_event(&Event {
                        method: events::MODEL_USAGE.to_string(),
                        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                            operation_id: op_id_for_spawn.clone(),
                            usage,
                            session_total_cost: current_session_cost,
                            current_cost: op_accumulated_cost,
                        })
                        .ok(),
                    });
                }
                StreamEvent::Done => {}
                StreamEvent::Cancelled => {
                    if edit_events_sent && !edit_finished_sent {
                        send_edit_aborted(
                            &state_for_spawn.out,
                            &op_id_for_spawn,
                            "Operation cancelled before edits were applied".to_string(),
                        );
                        edit_finished_sent = true;
                    }
                    state_for_spawn.out.send_event(&Event {
                        method: events::MODEL_CANCELLED.to_string(),
                        params: Some(serde_json::json!({ "operation_id": op_id_for_spawn })),
                    });
                    is_cancelled = true;
                    break;
                }
                StreamEvent::Error(err) => {
                    if edit_events_sent && !edit_finished_sent {
                        send_edit_aborted(
                            &state_for_spawn.out,
                            &op_id_for_spawn,
                            format!("Operation failed before edits were applied: {}", err),
                        );
                        edit_finished_sent = true;
                    }
                    state_for_spawn.out.send_event(&Event {
                        method: events::MODEL_ERROR.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id_for_spawn,
                            "message": err,
                        })),
                    });
                    is_error = true;
                    break;
                }
            }
        }

        if is_cancelled || is_error {
            let abort_handle = wf_task.abort_handle();
            if tokio::time::timeout(tokio::time::Duration::from_millis(500), wf_task)
                .await
                .is_err()
            {
                abort_handle.abort();
            }
            *state_for_spawn.active_cancel.lock().await = None;
            return;
        }

        match wf_task.await {
            Ok(Ok(wf_result)) => {
                let usage_to_send = usage_info.unwrap_or_default();
                let cost = usage_to_send.cost.unwrap_or(0.0);
                let total_cost = {
                    let mut total = state_for_spawn.total_cost.lock().await;
                    *total += cost;
                    *total
                };

                state_for_spawn.out.send_event(&Event {
                    method: events::MODEL_USAGE.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                        operation_id: op_id_for_spawn.clone(),
                        usage: usage_to_send.clone(),
                        session_total_cost: total_cost,
                        current_cost: Some(cost),
                    })
                    .ok(),
                });

                if let tauqe_protocol::ModelResult::Edit {
                    applied,
                    ref error,
                    ref changed_files,
                    ref commit_hash,
                    ref summary,
                    ..
                } = wf_result.result
                {
                    state_for_spawn.out.send_event(&Event {
                        method: events::EDIT_FINISHED.to_string(),
                        params: serde_json::to_value(EditFinishedEvent {
                            operation_id: op_id_for_spawn.clone(),
                            applied,
                            error: error.clone(),
                            changed_files: changed_files.clone(),
                            commit_hash: commit_hash.clone(),
                        })
                        .ok(),
                    });
                    edit_finished_sent = true;

                    if let Some(hash) = commit_hash {
                        state_for_spawn.out.send_event(&Event {
                            method: events::GIT_COMMIT_CREATED.to_string(),
                            params: serde_json::to_value(GitCommitCreatedEvent {
                                commit_hash: hash.clone(),
                                summary: summary.clone(),
                                changed_files: changed_files.clone(),
                            })
                            .ok(),
                        });

                        let repo_state = tauqe_core::git::get_repository_state(None);
                        state_for_spawn.out.send_event(&Event {
                            method: events::GIT_STATE_CHANGED.to_string(),
                            params: Some(serde_json::json!({ "repository": repo_state })),
                        });
                    }
                }

                let ctx_state = state_for_spawn.context.read().await.get_state();
                state_for_spawn.out.send_event(&Event {
                    method: events::CONTEXT_CHANGED.to_string(),
                    params: Some(serde_json::json!({ "state": ctx_state })),
                });

                let repo_root = state_for_spawn.context.read().await.repo_root().to_path_buf();
                let active_id = tauqe_core::plan::storage::PlanStorage::load_active_id(&repo_root).ok().flatten();
                if let Ok(plans) = tauqe_core::plan::storage::PlanStorage::load_all(&repo_root) {
                    for plan in &plans {
                        state_for_spawn.out.send_event(&Event {
                            method: events::PLAN_UPDATED.to_string(),
                            params: serde_json::to_value(tauqe_protocol::PlanUpdatedEvent { plan: plan.clone() }).ok(),
                        });
                    }
                    state_for_spawn.out.send_event(&Event {
                        method: events::PLAN_LIST_CHANGED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                            plans,
                            active_id,
                        })
                        .ok(),
                    });
                }

                let active_review_id = tauqe_core::review::storage::ReviewStorage::load_active_id(&repo_root).ok().flatten();
                if let Ok(reviews) = tauqe_core::review::storage::ReviewStorage::load_all(&repo_root) {
                    for session in &reviews {
                        state_for_spawn.out.send_event(&Event {
                            method: events::REVIEW_UPDATED.to_string(),
                            params: serde_json::to_value(tauqe_protocol::ReviewUpdatedEvent { session: session.clone() }).ok(),
                        });
                    }
                    state_for_spawn.out.send_event(&Event {
                        method: events::REVIEW_LIST_CHANGED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::ReviewListChangedEvent {
                            reviews,
                            active_id: active_review_id,
                        })
                        .ok(),
                    });
                }

                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state_for_spawn.out,
                        &op_id_for_spawn,
                        "Edits were streamed but no edit result was produced".to_string(),
                    );
                }

                state_for_spawn.out.send_event(&Event {
                    method: events::MODEL_RESULT.to_string(),
                    params: Some(serde_json::json!(ModelResultEvent {
                        operation_id: op_id_for_spawn.clone(),
                        result: wf_result.result,
                        usage: Some(usage_to_send),
                        session_total_cost: Some(total_cost),
                        current_cost: Some(cost),
                    })),
                });

                state_for_spawn.out.send_event(&Event {
                    method: events::MODEL_FINISHED.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id_for_spawn,
                        "full_text": wf_result.assistant_text,
                    })),
                });
            }
            Ok(Err(err)) => {
                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state_for_spawn.out,
                        &op_id_for_spawn,
                        format!("Workflow failed: {}", err),
                    );
                }
                state_for_spawn.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id_for_spawn,
                        "message": err.to_string(),
                    })),
                });
            }
            Err(join_err) => {
                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state_for_spawn.out,
                        &op_id_for_spawn,
                        format!("Workflow task failed: {}", join_err),
                    );
                }
                state_for_spawn.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id_for_spawn,
                        "message": join_err.to_string(),
                    })),
                });
            }
        }

        *state_for_spawn.active_cancel.lock().await = None;
    });

    Ok(op_id)
}

pub async fn handle_model_cancel(req: Request, state: &Arc<AppState>) -> Response {
    let active_op = state.active_cancel.lock().await.take();
    if let Some(op) = active_op {
        let _ = op.cancel_tx.send(true);
        if let Some(handle) = op.abort_handle {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(600)).await;
                handle.abort();
            });
        }
    }
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cancelled": true })),
        error: None,
    }
}

pub async fn handle_model_clear_history(req: Request, state: &Arc<AppState>) -> Response {
    let mut history = state.history.lock().await;
    let _ = history.clear();
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cleared": true })),
        error: None,
    }
}

fn accumulate_usage(accum: &mut Option<ModelUsageInfo>, new_usage: ModelUsageInfo) {
    if let Some(existing) = accum.as_mut() {
        existing.prompt_tokens += new_usage.prompt_tokens;
        existing.completion_tokens += new_usage.completion_tokens;
        existing.total_tokens += new_usage.total_tokens;

        if let Some(r) = new_usage.reasoning_tokens {
            *existing.reasoning_tokens.get_or_insert(0) += r;
        }
        if let Some(c) = new_usage.cached_tokens {
            *existing.cached_tokens.get_or_insert(0) += c;
        }
        if let Some(c) = new_usage.cost {
            *existing.cost.get_or_insert(0.0) += c;
        }
    } else {
        *accum = Some(new_usage);
    }
}

fn send_edit_aborted(out: &OutChannel, op_id: &str, error: String) {
    out.send_event(&Event {
        method: events::EDIT_FINISHED.to_string(),
        params: serde_json::to_value(EditFinishedEvent {
            operation_id: op_id.to_string(),
            applied: false,
            error: Some(error),
            changed_files: Vec::new(),
            commit_hash: None,
        })
        .ok(),
    });
}
