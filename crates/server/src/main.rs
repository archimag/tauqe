use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};
use workbench_core::config::{load_config, AppConfig};
use workbench_core::context::ContextManager;
use workbench_core::edits::{
    EditProtocol, WholeFileEditProtocol, XmlEditProtocol,
};
use workbench_core::model::gateway::{ChatMessage, StreamEvent};
use workbench_core::model::openrouter::OpenRouterClient;
use workbench_core::workflow::{EditWorkflow, NaiveEditWorkflow};
use workbench_protocol::{
    events, methods, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextRemoveParams, ContextSetAccessParams, EditFileDoneEvent, EditFileStartedEvent,
    EditFinishedEvent, EditHunkEvent, EditStartedEvent, Event, InitializeResult, ModelAskParams,
    ModelResultEvent, RepositoryListFilesResult, Request, RequestId, Response, ResponseError,
    PROTOCOL_VERSION,
};

struct ModelSession {
    history: Vec<ChatMessage>,
    active_cancel: Option<watch::Sender<bool>>,
    total_cost: f64,
    context_manager: ContextManager,
}

struct AppState {
    config: Mutex<AppConfig>,
    session: Mutex<ModelSession>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    workbench_core::init();

    let config = load_config(None);
    let repo_state = workbench_core::git::get_repository_state(None);
    let repo_path = PathBuf::from(repo_state.root);

    let state = Arc::new(AppState {
        config: Mutex::new(config),
        session: Mutex::new(ModelSession {
            history: Vec::new(),
            active_cancel: None,
            total_cost: 0.0,
            context_manager: ContextManager::new(repo_path),
        }),
    });

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(err) => {
                let err_resp = Response {
                    id: RequestId::Number(0),
                    result: None,
                    error: Some(ResponseError {
                        code: "PARSE_ERROR".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
                let resp_str = serde_json::to_string(&err_resp)? + "\n";
                stdout.write_all(resp_str.as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };

        let response = handle_request(request, &state).await;
        let resp_str = serde_json::to_string(&response)? + "\n";
        stdout.write_all(resp_str.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn handle_request(req: Request, state: &Arc<AppState>) -> Response {
    match req.method.as_str() {
        methods::CLIENT_INITIALIZE => {
            let repo_state = workbench_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let mut cfg = state.config.lock().await;
            if !repo_path.as_os_str().is_empty() {
                *cfg = load_config(Some(repo_path));
                let mut session = state.session.lock().await;
                session.context_manager.set_repo_root(repo_path.to_path_buf());
            }

            let result = InitializeResult {
                protocol_version: PROTOCOL_VERSION.to_string(),
                server_name: "workbench-server".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                repository: Some(repo_state),
                model: Some(cfg.models.default.clone()),
            };
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            }
        }
        methods::REPOSITORY_GET_STATE => {
            let repo_state = workbench_core::git::get_repository_state(None);
            Response {
                id: req.id,
                result: Some(serde_json::to_value(repo_state).unwrap()),
                error: None,
            }
        }
        methods::REPOSITORY_LIST_FILES => {
            let repo_state = workbench_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);
            match workbench_core::git::list_repository_files(Some(repo_dir)) {
                Ok(files) => Response {
                    id: req.id,
                    result: Some(
                        serde_json::to_value(RepositoryListFilesResult { files }).unwrap(),
                    ),
                    error: None,
                },
                Err(err) => Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "LIST_FILES_FAILED".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                },
            }
        }
        methods::CONTEXT_GET => {
            let session = state.session.lock().await;
            let ctx_state = session.context_manager.get_state();
            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_ADD => {
            let params: ContextAddParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
            {
                Some(p) => p,
                None => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "INVALID_PARAMS".to_string(),
                            message: "Missing or invalid path/access".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let ctx_state = {
                let mut session = state.session.lock().await;
                match session.context_manager.add_file(&params.path, params.access) {
                    Ok(_) => session.context_manager.get_state(),
                    Err(err) => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "ADD_FAILED".to_string(),
                                message: err.to_string(),
                                data: None,
                            }),
                        };
                    }
                }
            };

            send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            }).await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_ADD_PATTERN => {
            let params: ContextAddPatternParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
            {
                Some(p) => p,
                None => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "INVALID_PARAMS".to_string(),
                            message: "Missing or invalid pattern/access".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let (added_count, added_tokens, ctx_state) = {
                let repo_state = workbench_core::git::get_repository_state(None);
                let repo_dir = Path::new(&repo_state.root);
                let available_files = workbench_core::git::list_repository_files(Some(repo_dir))
                    .unwrap_or_default();

                let mut session = state.session.lock().await;
                match session
                    .context_manager
                    .add_files_by_pattern(&params.pattern, params.access, &available_files)
                {
                    Ok((items, tokens)) => {
                        let state = session.context_manager.get_state();
                        (items.len(), tokens, state)
                    }
                    Err(err) => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "ADD_PATTERN_FAILED".to_string(),
                                message: err.to_string(),
                                data: None,
                            }),
                        };
                    }
                }
            };

            send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            }).await;

            let result = ContextAddPatternResult {
                added_count,
                added_tokens,
                state: ctx_state,
            };

            Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_REMOVE => {
            let params: ContextRemoveParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
            {
                Some(p) => p,
                None => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "INVALID_PARAMS".to_string(),
                            message: "Missing or invalid path".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let ctx_state = {
                let mut session = state.session.lock().await;
                match session.context_manager.remove_file(&params.path) {
                    Ok(_) => session.context_manager.get_state(),
                    Err(err) => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "REMOVE_FAILED".to_string(),
                                message: err.to_string(),
                                data: None,
                            }),
                        };
                    }
                }
            };

            send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            }).await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_SET_ACCESS => {
            let params: ContextSetAccessParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
            {
                Some(p) => p,
                None => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "INVALID_PARAMS".to_string(),
                            message: "Missing or invalid parameters".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let ctx_state = {
                let mut session = state.session.lock().await;
                match session.context_manager.set_access(&params.path, params.access) {
                    Ok(_) => session.context_manager.get_state(),
                    Err(err) => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "SET_ACCESS_FAILED".to_string(),
                                message: err.to_string(),
                                data: None,
                            }),
                        };
                    }
                }
            };

            send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            }).await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_CLEAR => {
            let ctx_state = {
                let mut session = state.session.lock().await;
                session.context_manager.clear();
                session.context_manager.get_state()
            };

            send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            }).await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::MODEL_ASK => {
            let params: ModelAskParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
            {
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

            let (api_key, model, edit_config) = {
                let cfg = state.config.lock().await;
                let provider_cfg = match cfg.providers.openrouter.clone() {
                    Some(c) => c,
                    None => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "NO_PROVIDER".to_string(),
                                message: "No OpenRouter provider configured".to_string(),
                                data: None,
                            }),
                        };
                    }
                };

                let key = match provider_cfg.api_key {
                    Some(k) if !k.trim().is_empty() => k,
                    _ => {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "NO_API_KEY".to_string(),
                                message: "OpenRouter API key is not set".to_string(),
                                data: None,
                            }),
                        };
                    }
                };

                (key, cfg.models.default.clone(), cfg.edit.clone())
            };

            let op_id = format!("op-{}", next_operation_id().await);
            let op_id_event = op_id.clone();

            let started_event = Event {
                method: events::MODEL_STARTED.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id_event,
                    "model": model,
                })),
            };
            send_event(&started_event).await;

            let (cancel_tx, cancel_rx) = watch::channel(false);
            {
                let mut session = state.session.lock().await;
                session.active_cancel = Some(cancel_tx.clone());
            }

            let client = OpenRouterClient::new(api_key);
            let prompt_for_spawn = params.prompt.clone();
            let state_for_spawn = Arc::clone(state);
            let op_id_for_spawn = op_id.clone();
            let model_for_spawn = model.clone();

            let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

            tokio::spawn(async move {
                // Select edit protocol according to configuration
                let protocol: Box<dyn EditProtocol> = match edit_config.protocol.as_str() {
                    "whole_file" => Box::new(WholeFileEditProtocol),
                    _ => Box::new(XmlEditProtocol),
                };

                // Select workflow (defaulting to NaiveEditWorkflow)
                let workflow: Box<dyn EditWorkflow> = match edit_config.workflow.as_str() {
                    _ => Box::new(NaiveEditWorkflow),
                };

                // Run workflow task concurrently with draining streaming events
                let (wf_tx, wf_rx) = (tx.clone(), cancel_rx.clone());
                let state_clone = Arc::clone(&state_for_spawn);
                let model_clone = model_for_spawn.clone();
                let prompt_clone = prompt_for_spawn.clone();

                let workflow_future = async move {
                    let mut session = state_clone.session.lock().await;
                    let history = session.history.clone();
                    workflow
                        .execute(
                            &prompt_clone,
                            &client,
                            &model_clone,
                            &mut session.context_manager,
                            &history,
                            protocol.as_ref(),
                            wf_tx,
                            wf_rx,
                        )
                        .await
                };

                let wf_task = tokio::spawn(workflow_future);

                let mut usage_info = None;

                // Drain events concurrently as they arrive
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::ReasoningDelta(delta) => {
                            let ev = Event {
                                method: events::MODEL_REASONING_DELTA.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id_for_spawn,
                                    "delta": delta,
                                })),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::TextDelta(delta) => {
                            let ev = Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id_for_spawn,
                                    "delta": delta,
                                })),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditStarted => {
                            let ev = Event {
                                method: events::EDIT_STARTED.to_string(),
                                params: Some(serde_json::to_value(EditStartedEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                }).unwrap()),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditFileStarted { path, op_type } => {
                            let ev = Event {
                                method: events::EDIT_FILE_STARTED.to_string(),
                                params: Some(serde_json::to_value(EditFileStartedEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                    path,
                                    op_type,
                                }).unwrap()),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditHunk { path, hunk_index, old_text, new_text } => {
                            let ev = Event {
                                method: events::EDIT_HUNK.to_string(),
                                params: Some(serde_json::to_value(EditHunkEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                    path,
                                    hunk_index,
                                    old_text,
                                    new_text,
                                }).unwrap()),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditFileDone { path, status, error, hunks_count } => {
                            let ev = Event {
                                method: events::EDIT_FILE_DONE.to_string(),
                                params: Some(serde_json::to_value(EditFileDoneEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                    path,
                                    status,
                                    error,
                                    hunks_count,
                                }).unwrap()),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::Usage(usage) => {
                            usage_info = Some(usage);
                        }
                        StreamEvent::Done => break,
                        StreamEvent::Cancelled => {
                            let cancelled_event = Event {
                                method: events::MODEL_CANCELLED.to_string(),
                                params: Some(serde_json::json!({ "operation_id": op_id_for_spawn })),
                            };
                            send_event(&cancelled_event).await;
                            let mut session = state_for_spawn.session.lock().await;
                            session.active_cancel = None;
                            return;
                        }
                        StreamEvent::Error(err) => {
                            let err_event = Event {
                                method: events::MODEL_ERROR.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id_for_spawn,
                                    "message": err,
                                })),
                            };
                            send_event(&err_event).await;
                            let mut session = state_for_spawn.session.lock().await;
                            session.active_cancel = None;
                            return;
                        }
                    }
                }

                match wf_task.await {
                    Ok(Ok(wf_result)) => {
                        let mut session = state_for_spawn.session.lock().await;

                        let usage_to_send = usage_info.unwrap_or_default();
                        let cost = usage_to_send.cost.unwrap_or(0.0);
                        session.total_cost += cost;
                        let total_cost = session.total_cost;

                        let usage_event = Event {
                            method: events::MODEL_USAGE.to_string(),
                            params: Some(serde_json::json!({
                                "operation_id": op_id_for_spawn,
                                "usage": usage_to_send,
                                "session_total_cost": total_cost,
                            })),
                        };
                        send_event(&usage_event).await;

                        // Emit EDIT_FINISHED if edits were part of the result
                        if let workbench_protocol::ModelResult::Edit {
                            applied,
                            ref error,
                            ref changed_files,
                            ..
                        } = wf_result.result
                        {
                            let finished_edit_event = Event {
                                method: events::EDIT_FINISHED.to_string(),
                                params: Some(serde_json::to_value(EditFinishedEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                    applied,
                                    error: error.clone(),
                                    changed_files: changed_files.clone(),
                                }).unwrap()),
                            };
                            send_event(&finished_edit_event).await;
                        }

                        // Notify context changed if edits were applied to disk
                        let ctx_state = session.context_manager.get_state();
                        send_event(&Event {
                            method: events::CONTEXT_CHANGED.to_string(),
                            params: Some(serde_json::json!({ "state": ctx_state })),
                        }).await;

                        let result_event = Event {
                            method: events::MODEL_RESULT.to_string(),
                            params: Some(serde_json::json!(ModelResultEvent {
                                operation_id: op_id_for_spawn.clone(),
                                result: wf_result.result,
                                usage: Some(usage_to_send),
                                session_total_cost: Some(total_cost),
                            })),
                        };
                        send_event(&result_event).await;

                        if let Some((user_msg, asst_msg)) = wf_result.session_history_update {
                            session.history.push(user_msg);
                            if !asst_msg.content.is_empty() {
                                session.history.push(asst_msg);
                            }
                        }

                        let finished_event = Event {
                            method: events::MODEL_FINISHED.to_string(),
                            params: Some(serde_json::json!({
                                "operation_id": op_id_for_spawn,
                                "full_text": wf_result.assistant_text,
                            })),
                        };
                        send_event(&finished_event).await;
                    }
                    Ok(Err(err)) => {
                        let err_event = Event {
                            method: events::MODEL_ERROR.to_string(),
                            params: Some(serde_json::json!({
                                "operation_id": op_id_for_spawn,
                                "message": err.to_string(),
                            })),
                        };
                        send_event(&err_event).await;
                    }
                    Err(join_err) => {
                        let err_event = Event {
                            method: events::MODEL_ERROR.to_string(),
                            params: Some(serde_json::json!({
                                "operation_id": op_id_for_spawn,
                                "message": join_err.to_string(),
                            })),
                        };
                        send_event(&err_event).await;
                    }
                }

                let mut session = state_for_spawn.session.lock().await;
                session.active_cancel = None;
            });

            Response {
                id: req.id,
                result: Some(serde_json::json!({ "operation_id": op_id })),
                error: None,
            }
        }
        methods::MODEL_CANCEL => {
            let cancel_tx = {
                let mut session = state.session.lock().await;
                session.active_cancel.take()
            };
            if let Some(tx) = cancel_tx {
                let _ = tx.send(true);
            }
            Response {
                id: req.id,
                result: Some(serde_json::json!({ "cancelled": true })),
                error: None,
            }
        }
        methods::MODEL_CLEAR_HISTORY => {
            let mut session = state.session.lock().await;
            session.history.clear();
            Response {
                id: req.id,
                result: Some(serde_json::json!({ "cleared": true })),
                error: None,
            }
        }
        _ => Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "METHOD_NOT_FOUND".to_string(),
                message: format!("Method '{}' not found", req.method),
                data: None,
            }),
        },
    }
}

async fn send_event(event: &Event) {
    let mut stdout = tokio::io::stdout();
    let line = serde_json::to_string(event).unwrap() + "\n";
    let _ = stdout.write_all(line.as_bytes()).await;
    let _ = stdout.flush().await;
}

use std::sync::atomic::{AtomicU64, Ordering};

static OP_COUNTER: AtomicU64 = AtomicU64::new(1);

async fn next_operation_id() -> u64 {
    OP_COUNTER.fetch_add(1, Ordering::SeqCst)
}
