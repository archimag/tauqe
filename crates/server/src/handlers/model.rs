use std::sync::Arc;

use tokio::sync::watch;
use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::xml::XmlPromptProtocol;
use tauqe_core::providers::{create_provider, LlmProvider, StreamEvent};
use tauqe_core::workflow::{ExecutionMode, SingleExecution};
use tauqe_protocol::{
    events, EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent,
    EditHunkEvent, EditStartedEvent, Event, GitCommitCreatedEvent, ModelAskParams,
    ModelResultEvent, ModelStartedEvent, ModelUsageInfo, ReasoningEffort, Request, Response,
    ResponseError,
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

    match start_model_turn(state, params.prompt, None, None).await {
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

pub async fn handle_model_select(req: Request, state: &Arc<AppState>) -> Response {
    if state.active_cancel.lock().await.is_some() {
        return Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "OPERATION_IN_PROGRESS".to_string(),
                message: "Cannot change model while an operation is in progress".to_string(),
                data: None,
            }),
        };
    }

    let params: tauqe_protocol::ModelSelectParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
                    message: "Missing or invalid model selection".to_string(),
                    data: None,
                }),
            };
        }
    };

    let (selection, effective_model, roles) = {
        let mut cur = state.current_selection.lock().await;
        *cur = params.selection.clone();
        let cfg = state.config.lock().await;
        let effective = cfg.resolve_model(&cur);
        let roles = cfg.roles_summary();
        (cur.clone(), effective, roles)
    };

    state.out.send_event(&Event {
        method: events::MODEL_CHANGED.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelChangedEvent {
            selection: selection.clone(),
            effective_model: effective_model.clone(),
            roles: Some(roles.clone()),
        })
        .ok(),
    });

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelSelectResult {
            selection,
            effective_model,
            roles: Some(roles),
        },
    )
}

pub async fn handle_model_current(req: Request, state: &Arc<AppState>) -> Response {
    let selection = state.current_selection().await;
    let cfg = state.config.lock().await;
    let effective_model = cfg.resolve_model(&selection);
    let available_models = cfg.available_models();
    let roles = cfg.roles_summary();

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelCurrentResult {
            selection,
            effective_model,
            roles: Some(roles),
            available_models,
        },
    )
}

/// Result of an executed model turn, capturing termination status, errors, and outcomes.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelTurnResult {
    pub operation_id: String,
    pub result: Option<tauqe_protocol::ModelResult>,
    pub assistant_text: String,
    pub is_cancelled: bool,
    pub error: Option<String>,
    pub has_terminal_success: bool,
    /// Coder's block report when the turn ended with a semantic `<blocked>`
    /// signal. Preserved so plan and review supervisors can react accordingly.
    pub blocked: Option<String>,
}

#[allow(dead_code)]
impl ModelTurnResult {
    /// Evaluates if the step executed successfully according to atomic completion invariants.
    ///
    /// A step is considered successful strictly when:
    /// 1. The turn was not cancelled and encountered no unhandled errors.
    /// 2. Code edits were verified and applied (`Edit` with `applied: true`).
    /// 3. Or an explicit terminal success tag was emitted (`<success ... />`).
    pub fn is_success(&self) -> bool {
        if self.is_cancelled || self.error.is_some() {
            return false;
        }
        match &self.result {
            Some(tauqe_protocol::ModelResult::Edit {
                applied: true,
                error: None,
                ..
            }) => true,
            Some(tauqe_protocol::ModelResult::Answer { .. }) => self.has_terminal_success,
            _ => false,
        }
    }
}

struct PreparedTurn {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_protocol::ModelRef,
    app_config: tauqe_core::config::AppConfig,
    selection: tauqe_protocol::ModelSelection,
    reasoning_effort: Option<ReasoningEffort>,
    op_id: String,
    cancel_rx: watch::Receiver<bool>,
    /// Active turn marker binding structural protocol tags to this turn.
    marker: Option<String>,
}

/// Builds a real network provider from configuration, mapping failures to a
/// `NO_API_KEY` response error.
fn build_configured_provider(
    config: &tauqe_core::config::AppConfig,
    kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, ResponseError> {
    create_provider(kind, config)
        .map(Arc::from)
        .map_err(|err| ResponseError {
            code: "NO_API_KEY".to_string(),
            message: err.to_string(),
            data: None,
        })
}

/// Resolves the provider and model for a turn, emits `model/started`, and
/// registers the cancellable active operation. `is_batch` marks turns that run
/// as members of a batch supervisor so clients can suppress per-step signals.
async fn prepare_turn(
    state: &Arc<AppState>,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    reasoning_effort: Option<ReasoningEffort>,
    is_batch: bool,
    marker: Option<String>,
) -> Result<PreparedTurn, ResponseError> {
    let (provider, model, app_config, selection) = {
        let cfg = state.config.lock().await;
        let selection = match custom_selection {
            Some(s) => s,
            None => state.current_selection.lock().await.clone(),
        };
        let model = cfg.resolve_model(&selection);
        // Deterministic unit tests inject a scripted provider through the
        // state's test seam; production builds always construct a real
        // network provider from configuration.
        #[cfg(test)]
        let provider: Arc<dyn LlmProvider> = match state.provider_override.lock().await.clone() {
            Some(provider) => provider,
            None => build_configured_provider(&cfg, model.provider)?,
        };

        #[cfg(not(test))]
        let provider: Arc<dyn LlmProvider> = build_configured_provider(&cfg, model.provider)?;

        (provider, model, cfg.clone(), selection)
    };

    let op_id = format!("op-{}", next_operation_id());

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.to_string(),
            plan_context,
            is_batch,
        })
        .ok(),
    });

    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        *state.active_cancel.lock().await = Some(ActiveOperation {
            cancel_tx,
            abort_handle: None,
        });
    }

    Ok(PreparedTurn {
        provider,
        model,
        app_config,
        selection,
        reasoning_effort,
        op_id,
        cancel_rx,
        marker,
    })
}

async fn run_prepared_turn(
    state: &Arc<AppState>,
    prompt: String,
    prepared: PreparedTurn,
    mut policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
) -> Result<ModelTurnResult, ResponseError> {
    let PreparedTurn {
        provider,
        model,
        app_config,
        selection,
        reasoning_effort,
        op_id,
        cancel_rx,
        marker,
    } = prepared;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

    let protocol_str = app_config.develop.protocol.trim().to_lowercase();
    let base_protocol: Box<dyn PromptProtocol> = match protocol_str.as_str() {
        "structured" | "json" => Box::new(JsonPromptProtocol),
        _ => Box::new(XmlPromptProtocol),
    };
    // Bind the active turn marker so structural protocol tags carry a per-turn
    // suffix and literal source tags are never mistaken for protocol commands.
    let protocol: Box<dyn PromptProtocol> = match marker {
        Some(marker) => base_protocol
            .with_turn_marker(&marker)
            .unwrap_or(base_protocol),
        None => base_protocol,
    };

    let mut workflow_opts = tauqe_core::workflow::WorkflowOptions::from_app_config(&app_config);
    workflow_opts.reasoning_effort = reasoning_effort.or_else(|| match &selection {
        tauqe_protocol::ModelSelection::Role(role) => app_config.models.resolve_effort(*role),
        tauqe_protocol::ModelSelection::Specific(_) => None,
    });
    workflow_opts.initial_selection = Some(selection);
    let execution = SingleExecution::new(workflow_opts);
    let (wf_tx, wf_rx) = (tx.clone(), cancel_rx.clone());
    drop(tx);

    let state_clone = Arc::clone(state);
    let model_clone = model.clone();
    let prompt_clone = prompt.clone();

    let workflow_future = async move {
        let _session_guard = state_clone.session_lock.lock().await;
        let mut local_context = state_clone.context.read().await.clone();
        let mut history = state_clone.history.lock().await;
        let res = execution
            .execute(
                policy.as_mut(),
                &prompt_clone,
                provider.as_ref(),
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
        if let Some(ref mut op) = *state.active_cancel.lock().await {
            op.abort_handle = Some(wf_task.abort_handle());
        }
    }

    let mut raw_streamed_text = String::new();
    let mut usage_info = None;
    let mut edit_events_sent = false;
    let mut edit_finished_sent = false;
    let mut is_cancelled = false;
    let mut is_error = false;
    let mut stream_error_msg = None;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ReasoningDelta(delta) => {
                state.out.send_event(&Event {
                    method: events::MODEL_REASONING_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::TextDelta(delta) => {
                raw_streamed_text.push_str(&delta);
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::ContextChanged(ctx_state) => {
                state.out.send_event(&Event {
                    method: events::CONTEXT_CHANGED.to_string(),
                    params: Some(serde_json::json!({ "state": ctx_state })),
                });
            }
            StreamEvent::EditStarted => {
                edit_events_sent = true;
                state.out.send_event(&Event {
                    method: events::EDIT_STARTED.to_string(),
                    params: serde_json::to_value(EditStartedEvent {
                        operation_id: op_id.clone(),
                    })
                    .ok(),
                });
            }
            StreamEvent::EditFileStarted { path, op_type } => {
                edit_events_sent = true;
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_STARTED.to_string(),
                    params: serde_json::to_value(EditFileStartedEvent {
                        operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::EDIT_HUNK.to_string(),
                    params: serde_json::to_value(EditHunkEvent {
                        operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_DONE.to_string(),
                    params: serde_json::to_value(EditFileDoneEvent {
                        operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_RETRYING.to_string(),
                    params: serde_json::to_value(EditFileRetryingEvent {
                        operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::TURN_PHASE.to_string(),
                    params: serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                        operation_id: op_id.clone(),
                        phase,
                        round,
                        max_rounds,
                        detail,
                    })
                    .ok(),
                });
            }
            StreamEvent::RepoMapTokens(tokens) => {
                state.out.send_event(&Event {
                    method: events::TURN_REPOMAP.to_string(),
                    params: serde_json::to_value(tauqe_protocol::RepoMapInfoEvent {
                        operation_id: op_id.clone(),
                        tokens,
                    })
                    .ok(),
                });
            }
            StreamEvent::ToolchainStarted { command } => {
                state.out.send_event(&Event {
                    method: events::TOOLCHAIN_STARTED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ToolchainStartedEvent {
                        operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::TOOLCHAIN_FINISHED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                        operation_id: op_id.clone(),
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
                    let base_cost = *state.total_cost.lock().await;
                    base_cost + op_accumulated_cost.unwrap_or(0.0)
                };
                state.out.send_event(&Event {
                    method: events::MODEL_USAGE.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                        operation_id: op_id.clone(),
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
                        &state.out,
                        &op_id,
                        "Operation cancelled before edits were applied".to_string(),
                    );
                    edit_finished_sent = true;
                }
                state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
                });
                is_cancelled = true;
                break;
            }
            StreamEvent::Error(err) => {
                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state.out,
                        &op_id,
                        format!("Operation failed before edits were applied: {}", err),
                    );
                    edit_finished_sent = true;
                }
                state.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "message": err.clone(),
                    })),
                });
                stream_error_msg = Some(err);
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
        *state.active_cancel.lock().await = None;

        let error = if is_cancelled {
            Some("Operation cancelled".to_string())
        } else {
            stream_error_msg
        };

        return Ok(ModelTurnResult {
            operation_id: op_id,
            result: None,
            assistant_text: raw_streamed_text,
            is_cancelled,
            error,
            has_terminal_success: false,
            blocked: None,
        });
    }

    let turn_result = match wf_task.await {
        Ok(Ok(wf_result)) => {
            let usage_to_send = usage_info.unwrap_or_default();
            let cost = usage_to_send.cost.unwrap_or(0.0);
            let total_cost = {
                let mut total = state.total_cost.lock().await;
                *total += cost;
                *total
            };

            state.out.send_event(&Event {
                method: events::MODEL_USAGE.to_string(),
                params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                    operation_id: op_id.clone(),
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
                state.out.send_event(&Event {
                    method: events::EDIT_FINISHED.to_string(),
                    params: serde_json::to_value(EditFinishedEvent {
                        operation_id: op_id.clone(),
                        applied,
                        error: error.clone(),
                        changed_files: changed_files.clone(),
                        commit_hash: commit_hash.clone(),
                    })
                    .ok(),
                });
                edit_finished_sent = true;

                if let Some(hash) = commit_hash {
                    state.out.send_event(&Event {
                        method: events::GIT_COMMIT_CREATED.to_string(),
                        params: serde_json::to_value(GitCommitCreatedEvent {
                            commit_hash: hash.clone(),
                            summary: summary.clone(),
                            changed_files: changed_files.clone(),
                        })
                        .ok(),
                    });

                    let repo_state = tauqe_core::git::get_repository_state(None);
                    state.out.send_event(&Event {
                        method: events::GIT_STATE_CHANGED.to_string(),
                        params: Some(serde_json::json!({ "repository": repo_state })),
                    });
                }
            }

            let ctx_state = state.context.read().await.get_state();
            state.out.send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            });

            let repo_root = state.context.read().await.repo_root().to_path_buf();
            let active_id = tauqe_core::plan::storage::PlanStorage::load_active_id(&repo_root)
                .ok()
                .flatten();
            if let Ok(plans) = tauqe_core::plan::storage::PlanStorage::load_all(&repo_root) {
                for plan in &plans {
                    state.out.send_event(&Event {
                        method: events::PLAN_UPDATED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::PlanUpdatedEvent {
                            plan: plan.clone(),
                        })
                        .ok(),
                    });
                }
                state.out.send_event(&Event {
                    method: events::PLAN_LIST_CHANGED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                        plans,
                        active_id,
                    })
                    .ok(),
                });
            }

            let active_review_id =
                tauqe_core::review::storage::ReviewStorage::load_active_id(&repo_root)
                    .ok()
                    .flatten();
            if let Ok(reviews) = tauqe_core::review::storage::ReviewStorage::load_all(&repo_root) {
                for session in &reviews {
                    state.out.send_event(&Event {
                        method: events::REVIEW_UPDATED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::ReviewUpdatedEvent {
                            session: session.clone(),
                        })
                        .ok(),
                    });
                }
                state.out.send_event(&Event {
                    method: events::REVIEW_LIST_CHANGED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ReviewListChangedEvent {
                        reviews,
                        active_id: active_review_id,
                    })
                    .ok(),
                });
            }

            if should_report_aborted_edits(edit_events_sent, edit_finished_sent, &wf_result.result) {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    "Edits were streamed but no edit result was produced".to_string(),
                );
            }

            state.out.send_event(&Event {
                method: events::MODEL_RESULT.to_string(),
                params: Some(serde_json::json!(ModelResultEvent {
                    operation_id: op_id.clone(),
                    result: wf_result.result.clone(),
                    usage: Some(usage_to_send),
                    session_total_cost: Some(total_cost),
                    current_cost: Some(cost),
                })),
            });

            state.out.send_event(&Event {
                method: events::MODEL_FINISHED.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "full_text": wf_result.assistant_text.clone(),
                })),
            });

            let has_terminal_success =
                tauqe_core::edits::protocol::xml::extract_success(&raw_streamed_text)
                    .is_some()
                    || tauqe_core::edits::protocol::xml::extract_success(
                        &wf_result.assistant_text,
                    )
                    .is_some();

            let turn_error = match &wf_result.result {
                tauqe_protocol::ModelResult::Edit {
                    error,
                    applied: false,
                    ..
                } => error.clone(),
                _ => None,
            };

            ModelTurnResult {
                operation_id: op_id,
                result: Some(wf_result.result),
                assistant_text: wf_result.assistant_text,
                is_cancelled: false,
                error: turn_error,
                has_terminal_success,
                blocked: wf_result.blocked,
            }
        }
        Ok(Err(err)) => {
            if edit_events_sent && !edit_finished_sent {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    format!("Workflow failed: {}", err),
                );
            }
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "message": err.to_string(),
                })),
            });

            ModelTurnResult {
                operation_id: op_id,
                result: None,
                assistant_text: raw_streamed_text,
                is_cancelled: false,
                error: Some(err.to_string()),
                has_terminal_success: false,
                blocked: None,
            }
        }
        Err(join_err) => {
            if edit_events_sent && !edit_finished_sent {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    format!("Workflow task failed: {}", join_err),
                );
            }
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "message": join_err.to_string(),
                })),
            });

            ModelTurnResult {
                operation_id: op_id,
                result: None,
                assistant_text: raw_streamed_text,
                is_cancelled: false,
                error: Some(join_err.to_string()),
                has_terminal_success: false,
                blocked: None,
            }
        }
    };

    *state.active_cancel.lock().await = None;
    Ok(turn_result)
}

/// Initiates an asynchronous model turn in the background and immediately returns its operation ID.
///
/// Designed as a lightweight non-blocking wrapper for interactive single-prompt RPC requests.
pub async fn start_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
) -> Result<String, ResponseError> {
    let marker = state.next_turn_marker().await;
    let prepared =
        prepare_turn(state, custom_selection, plan_context, None, false, marker).await?;
    let op_id = prepared.op_id.clone();
    let state_clone = Arc::clone(state);
    let workflow_name = prepared.app_config.develop.workflow.trim().to_lowercase();
    tokio::spawn(async move {
        let policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy> = match workflow_name.as_str() {
            "naive" => Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new()),
            _ => Box::new(tauqe_core::workflow::git_policy::GitTransactionPolicy::new()),
        };
        let _ = run_prepared_turn(&state_clone, prompt, prepared, policy).await;
    });
    Ok(op_id)
}

/// Executes an isolated model turn to completion and awaits its outcome.
///
/// Returns the full `ModelTurnResult` representing the atomic turn lifecycle,
/// enabling server-side step orchestration to inspect step success deterministically.
///
/// An explicit `reasoning_effort` overrides the effort derived from the
/// selection, letting callers target a specific coder-ladder rung without
/// changing the semantic role. Turns started here always execute inside a batch
/// supervisor, so they are marked with `is_batch`. An explicit `marker` binds
/// the turn's structural protocol tags to a per-turn suffix so literal source
/// tags are never mistaken for protocol commands.
#[allow(dead_code)]
pub async fn execute_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    reasoning_effort: Option<ReasoningEffort>,
    marker: Option<String>,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
) -> Result<ModelTurnResult, ResponseError> {
    // Programmatic step turns always run inside a batch supervisor, so they are
    // marked as batch members to let clients suppress per-step signals.
    let prepared = prepare_turn(
        state,
        custom_selection,
        plan_context,
        reasoning_effort,
        true,
        marker,
    )
    .await?;
    run_prepared_turn(state, prompt, prepared, policy).await
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

fn should_report_aborted_edits(
    edit_events_sent: bool,
    edit_finished_sent: bool,
    result: &tauqe_protocol::ModelResult,
) -> bool {
    if !edit_events_sent || edit_finished_sent {
        return false;
    }
    !matches!(result, tauqe_protocol::ModelResult::Answer { .. })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_turn_result_success_evaluation() {
        // 1. Successful edit with applied: true
        let ok_edit = ModelTurnResult {
            operation_id: "op-1".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Fixed bug".to_string(),
                edits: vec![],
                proposal: None,
                applied: true,
                error: None,
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc".to_string()),
            }),
            assistant_text: "Done.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(ok_edit.is_success());

        // 2. Failed edit with applied: false
        let failed_edit = ModelTurnResult {
            operation_id: "op-2".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Failed".to_string(),
                edits: vec![],
                proposal: None,
                applied: false,
                error: Some("compiler error".to_string()),
                changed_files: vec![],
                commit_hash: None,
            }),
            assistant_text: "Failed.".to_string(),
            is_cancelled: false,
            error: Some("compiler error".to_string()),
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!failed_edit.is_success());

        // 3. Conversational answer without terminal success tag -> false (c-2 invariant)
        let conversational_answer = ModelTurnResult {
            operation_id: "op-3".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "I think you should change file X.".to_string(),
            }),
            assistant_text: "I think you should change file X.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!conversational_answer.is_success());

        // 4. Conversational answer WITH terminal success tag -> true
        let verified_answer = ModelTurnResult {
            operation_id: "op-4".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "Inspection verified cleanly.".to_string(),
            }),
            assistant_text: "Inspection verified cleanly.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: true,
            blocked: None,
        };
        assert!(verified_answer.is_success());

        // 5. Cancelled turn -> false
        let cancelled_turn = ModelTurnResult {
            operation_id: "op-5".to_string(),
            result: None,
            assistant_text: "".to_string(),
            is_cancelled: true,
            error: Some("Operation cancelled".to_string()),
            has_terminal_success: true,
            blocked: None,
        };
        assert!(!cancelled_turn.is_success());

        // 6. A semantically blocked turn is never a success.
        let blocked_turn = ModelTurnResult {
            operation_id: "op-6".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Execution blocked".to_string(),
                edits: vec![],
                proposal: None,
                applied: false,
                error: Some("precondition missing".to_string()),
                changed_files: vec![],
                commit_hash: None,
            }),
            assistant_text: "Blocked.".to_string(),
            is_cancelled: false,
            error: Some("precondition missing".to_string()),
            has_terminal_success: false,
            blocked: Some("precondition missing".to_string()),
        };
        assert!(!blocked_turn.is_success());
    }

    #[test]
    fn test_should_report_aborted_edits() {
        let answer = tauqe_protocol::ModelResult::Answer { text: "text".to_string() };
        let edit = tauqe_protocol::ModelResult::Edit {
            summary: "sum".to_string(),
            edits: vec![],
            proposal: None,
            applied: false,
            error: None,
            changed_files: vec![],
            commit_hash: None,
        };
        
        // Never report if no edit events were sent
        assert!(!should_report_aborted_edits(false, false, &answer));
        assert!(!should_report_aborted_edits(false, false, &edit));
        
        // Never report if already sent
        assert!(!should_report_aborted_edits(true, true, &answer));
        assert!(!should_report_aborted_edits(true, true, &edit));
        
        // If edit events were sent but not finished, report ONLY if it's not an Answer
        assert!(!should_report_aborted_edits(true, false, &answer));
        assert!(should_report_aborted_edits(true, false, &edit));
    }

    #[tokio::test]
    async fn test_handle_model_current_and_select_roles_summary() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.models.scribe = Some(tauqe_protocol::ModelRef::openrouter("test/scribe"));
        config.models.coder = Some(tauqe_protocol::ModelRef::openrouter("test/coder").into());
        config.models.architect = Some(tauqe_protocol::ModelRef::openrouter("test/architect"));
        config.models.advisor = Some(tauqe_protocol::ModelRef::openrouter("test/advisor"));
        config.models.reviewer = Some(tauqe_protocol::ModelRef::openrouter("test/reviewer"));

        let state = AppState::new(
            config,
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        // 1. Initial selection defaults to Advisor
        let req_current = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::MODEL_CURRENT.to_string(),
            params: None,
        };
        let res_current = handle_model_current(req_current, &state).await;
        assert!(res_current.error.is_none());
        let current_val: tauqe_protocol::ModelCurrentResult =
            serde_json::from_value(res_current.result.unwrap()).unwrap();
        assert_eq!(
            current_val.selection,
            tauqe_protocol::ModelSelection::Role(tauqe_protocol::ModelRole::Advisor)
        );
        let roles = current_val.roles.unwrap();
        assert_eq!(roles.scribe, tauqe_protocol::ModelRef::openrouter("test/scribe"));
        assert_eq!(roles.coder, tauqe_protocol::ModelRef::openrouter("test/coder"));
        assert_eq!(roles.architect, tauqe_protocol::ModelRef::openrouter("test/architect"));
        assert_eq!(roles.advisor, tauqe_protocol::ModelRef::openrouter("test/advisor"));
        assert_eq!(roles.reviewer, tauqe_protocol::ModelRef::openrouter("test/reviewer"));

        // 2. Select Coder role
        let req_select = Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: tauqe_protocol::methods::MODEL_SELECT.to_string(),
            params: Some(serde_json::json!({
                "selection": "coder"
            })),
        };
        let res_select = handle_model_select(req_select, &state).await;
        assert!(res_select.error.is_none());
        let select_val: tauqe_protocol::ModelSelectResult =
            serde_json::from_value(res_select.result.unwrap()).unwrap();
        assert_eq!(
            select_val.selection,
            tauqe_protocol::ModelSelection::Role(tauqe_protocol::ModelRole::Coder)
        );
        assert_eq!(
            select_val.effective_model,
            tauqe_protocol::ModelRef::openrouter("test/coder")
        );
        let selected_roles = select_val.roles.unwrap();
        assert_eq!(selected_roles.advisor, tauqe_protocol::ModelRef::openrouter("test/advisor"));
    }

    #[tokio::test]
    async fn test_turn_preparation_no_api_key() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let res_start = start_model_turn(&state, "test prompt".to_string(), None, None).await;
        assert!(res_start.is_err());
        assert_eq!(res_start.unwrap_err().code, "NO_API_KEY");

        let policy = Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new());
        let res_exec =
            execute_model_turn(&state, "test prompt".to_string(), None, None, None, None, policy)
                .await;
        assert!(res_exec.is_err());
        assert_eq!(res_exec.unwrap_err().code, "NO_API_KEY");
    }
}
