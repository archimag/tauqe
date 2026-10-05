use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};
use workbench_core::config::{load_config, AppConfig};
use workbench_core::context::ContextManager;
use workbench_core::model::gateway::{ChatMessage, StreamEvent};
use workbench_core::model::openrouter::OpenRouterClient;
use workbench_core::prompt::PromptAssembly;
use workbench_protocol::{
    events, methods, ContextAddParams, ContextRemoveParams, ContextSetAccessParams, Event,
    InitializeResult, ModelAskParams, RepositoryListFilesResult, Request, RequestId, Response,
    ResponseError, PROTOCOL_VERSION,
};

struct ModelSession {
    history: Vec<ChatMessage>,
    active_cancel: Option<watch::Sender<bool>>,
    total_cost: f64,
    context_manager: ContextManager,
}

struct AppState {
    config: AppConfig,
    session: Mutex<ModelSession>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    workbench_core::init();

    let config = load_config(None);
    let repo_state = workbench_core::git::get_repository_state(None);
    let repo_path = PathBuf::from(repo_state.root);

    let state = Arc::new(AppState {
        config,
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
            let mut config = state.config.clone();
            if !repo_path.as_os_str().is_empty() {
                config = load_config(Some(repo_path));
                let mut session = state.session.lock().await;
                session.context_manager.set_repo_root(repo_path.to_path_buf());
            }

            let result = InitializeResult {
                protocol_version: PROTOCOL_VERSION.to_string(),
                server_name: "workbench-server".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                repository: Some(repo_state),
                model: Some(config.models.default),
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

            let provider_cfg = match state.config.providers.openrouter.clone() {
                Some(cfg) => cfg,
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

            let api_key = match provider_cfg.api_key {
                Some(key) if !key.trim().is_empty() => key,
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

            let model = state.config.models.default.clone();
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

            // Assemble layered prompt with context snapshot & history
            let assembled_messages = {
                let session = state.session.lock().await;
                let repo_state = workbench_core::git::get_repository_state(None);
                let ctx_state = session.context_manager.get_state();
                let context_files = session.context_manager.read_context_files();

                let assembly = PromptAssembly::new(
                    Some(repo_state),
                    ctx_state.revision,
                    context_files,
                );

                assembly.assemble_chat_messages(&session.history, &params.prompt)
            };

            let prompt_for_history = params.prompt.clone();
            let state_for_spawn = Arc::clone(state);
            let op_id_for_spawn = op_id.clone();
            let model_for_spawn = model.clone();

            let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);
            let tx_for_spawn = tx.clone();

            tokio::spawn(async move {
                let result = client
                    .stream_chat(&model_for_spawn, assembled_messages, tx_for_spawn, cancel_rx)
                    .await;

                let mut session = state_for_spawn.session.lock().await;
                if let Err(err) = result {
                    let err_event = Event {
                        method: events::MODEL_ERROR.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id_for_spawn,
                            "message": err.to_string(),
                        })),
                    };
                    send_event(&err_event).await;
                }

                let mut assistant_text = String::new();
                let mut usage_info = None;
                let mut cancelled = false;

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
                            assistant_text.push_str(&delta);
                            let ev = Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id_for_spawn,
                                    "delta": delta,
                                })),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::Usage(usage) => usage_info = Some(usage),
                        StreamEvent::Done => break,
                        StreamEvent::Cancelled => {
                            cancelled = true;
                            break;
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
                            return;
                        }
                    }
                }

                if cancelled {
                    let cancelled_event = Event {
                        method: events::MODEL_CANCELLED.to_string(),
                        params: Some(serde_json::json!({ "operation_id": op_id_for_spawn })),
                    };
                    send_event(&cancelled_event).await;
                } else {
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

                    // Append user query and assistant response to session dialogue history
                    session.history.push(ChatMessage {
                        role: "user".to_string(),
                        content: prompt_for_history,
                    });

                    if !assistant_text.is_empty() {
                        session.history.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: assistant_text.clone(),
                        });
                    }

                    let finished_event = Event {
                        method: events::MODEL_FINISHED.to_string(),
                        params: Some(serde_json::json!({
                            "operation_id": op_id_for_spawn,
                            "full_text": assistant_text,
                        })),
                    };
                    send_event(&finished_event).await;
                }

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
