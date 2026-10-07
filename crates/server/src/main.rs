use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};
use tauqe_core::config::{load_config, AppConfig};
use tauqe_core::context::ContextManager;
use tauqe_core::edits::{EditProtocolFactory, XmlEditProtocol};
use tauqe_core::history::HistoryManager;
use tauqe_core::model::gateway::StreamEvent;
use tauqe_core::model::openrouter::OpenRouterClient;
use tauqe_core::workflow::{GitEditWorkflow, WorkflowFactory};
use tauqe_protocol::{
    events, methods, ConfigSetParams, ConfigState, ContextAddParams, ContextAddPatternParams,
    ContextAddPatternResult, ContextRemoveParams, ContextSetAccessParams, EditFileDoneEvent,
    EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, EditStartedEvent,
    Event, GitCommitCreatedEvent, GitDiffParams, GitDiffResult, HistoryEntryAddedEvent,
    HistoryGetParams, HistoryGetResult, InitializeResult, ModelAskParams, ModelResultEvent,
    ModelUsageInfo, RepositoryListFilesResult, Request, RequestId, Response, ResponseError,
    ToolchainResultEvent, ToolchainStartedEvent, UiHistoryItem, PROTOCOL_VERSION,
};

struct ModelSession {
    history_manager: HistoryManager,
    context_manager: ContextManager,
}

struct ActiveOperation {
    cancel_tx: watch::Sender<bool>,
    abort_handle: Option<tokio::task::AbortHandle>,
}

struct AppState {
    config: Mutex<AppConfig>,
    /// Held by the running workflow for its whole duration; never lock it from the event forwarder.
    session: Mutex<ModelSession>,
    active_cancel: Mutex<Option<ActiveOperation>>,
    total_cost: Mutex<f64>,
    history_sender: tokio::sync::mpsc::UnboundedSender<UiHistoryItem>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tauqe_core::init();

    let config = load_config(None);
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = PathBuf::from(&repo_state.root);

    let (history_tx, mut history_rx) = tokio::sync::mpsc::unbounded_channel::<UiHistoryItem>();
    let history_sender_init = history_tx.clone();
    let mut initial_history_manager = HistoryManager::new(repo_path.clone());
    initial_history_manager.set_listener(move |item| {
        let _ = history_sender_init.send(item);
    });

    let state = Arc::new(AppState {
        config: Mutex::new(config),
        session: Mutex::new(ModelSession {
            history_manager: initial_history_manager,
            context_manager: ContextManager::new(repo_path),
        }),
        active_cancel: Mutex::new(None),
        total_cost: Mutex::new(0.0),
        history_sender: history_tx,
    });

    tokio::spawn(async move {
        while let Some(item) = history_rx.recv().await {
            let ev = Event {
                method: events::HISTORY_ENTRY_ADDED.to_string(),
                params: Some(serde_json::to_value(HistoryEntryAddedEvent { item }).unwrap()),
            };
            send_event(&ev).await;
        }
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
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let mut cfg = state.config.lock().await;
            if !repo_path.as_os_str().is_empty() {
                *cfg = load_config(Some(repo_path));
                let mut session = state.session.lock().await;
                session
                    .context_manager
                    .set_repo_root(repo_path.to_path_buf());
                let mut hm = HistoryManager::new(repo_path.to_path_buf());
                let sender = state.history_sender.clone();
                hm.set_listener(move |item| {
                    let _ = sender.send(item);
                });
                session.history_manager = hm;
            }

            let result = InitializeResult {
                protocol_version: PROTOCOL_VERSION.to_string(),
                server_name: "tauqe-server".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                repository: Some(repo_state),
                model: Some(cfg.models.default.clone()),
                workflow: Some(cfg.edit.workflow.clone()),
                edit_protocol: Some(cfg.edit.protocol.clone()),
                available_models: cfg.models.available.clone(),
                available_workflows: WorkflowFactory::available_workflows(),
                available_edit_protocols: EditProtocolFactory::available_protocols(),
            };
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            }
        }
        methods::REPOSITORY_GET_STATE => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            Response {
                id: req.id,
                result: Some(serde_json::to_value(repo_state).unwrap()),
                error: None,
            }
        }
        methods::REPOSITORY_LIST_FILES => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);
            match tauqe_core::git::list_repository_files(Some(repo_dir)) {
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
        methods::HISTORY_GET => {
            let params: HistoryGetParams = req
                .params
                .and_then(|p| serde_json::from_value(p).ok())
                .unwrap_or_default();
            let limit = params.limit.unwrap_or(10);
            let session = state.session.lock().await;
            match session.history_manager.get_ui_slice(limit, params.before_id) {
                Ok((items, has_more, total_count)) => {
                    let result = HistoryGetResult {
                        items,
                        has_more,
                        total_count,
                    };
                    Response {
                        id: req.id,
                        result: Some(serde_json::to_value(result).unwrap()),
                        error: None,
                    }
                }
                Err(err) => Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "HISTORY_GET_FAILED".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                },
            }
        }
        methods::GIT_UNDO => {
            {
                if state.active_cancel.lock().await.is_some() {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "OPERATION_IN_PROGRESS".to_string(),
                            message: "Cannot undo while a model operation is in progress"
                                .to_string(),
                            data: None,
                        }),
                    };
                }
            }

            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);

            match tauqe_core::git::undo_last_ai_commit(repo_dir) {
                Ok(undo_result) => {
                    let ctx_state = {
                        let mut session = state.session.lock().await;
                        let _ = session.history_manager.record_undo(
                            &undo_result.undone_commit,
                            undo_result.restored_checkpoint,
                        );
                        session.context_manager.prune_missing_files();
                        session.context_manager.get_state()
                    };

                    let new_repo_state = tauqe_core::git::get_repository_state(Some(repo_dir));
                    send_event(&Event {
                        method: events::GIT_STATE_CHANGED.to_string(),
                        params: Some(serde_json::json!({ "repository": new_repo_state })),
                    })
                    .await;

                    send_event(&Event {
                        method: events::CONTEXT_CHANGED.to_string(),
                        params: Some(serde_json::json!({ "state": ctx_state })),
                    })
                    .await;

                    send_event(&Event {
                        method: events::GIT_UNDO_COMPLETED.to_string(),
                        params: Some(serde_json::to_value(&undo_result).unwrap()),
                    })
                    .await;

                    Response {
                        id: req.id,
                        result: Some(serde_json::to_value(undo_result).unwrap()),
                        error: None,
                    }
                }
                Err(err) => Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "UNDO_FAILED".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                },
            }
        }
        methods::GIT_GET_DIFF => {
            let params: Option<GitDiffParams> =
                req.params.and_then(|p| serde_json::from_value(p).ok());
            let target_path = params.as_ref().and_then(|p| p.path.as_deref());

            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);

            match tauqe_core::git::get_diff(repo_dir, target_path) {
                Ok(diff) => Response {
                    id: req.id,
                    result: Some(serde_json::to_value(GitDiffResult { diff }).unwrap()),
                    error: None,
                },
                Err(err) => Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "GET_DIFF_FAILED".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                },
            }
        }
        methods::GIT_SQUASH_PREVIEW => {
            if state.active_cancel.lock().await.is_some() {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "OPERATION_IN_PROGRESS".to_string(),
                        message: "Cannot squash commits while a model operation is in progress".to_string(),
                        data: None,
                    }),
                };
            }

            let params: Option<tauqe_protocol::GitSquashPreviewParams> =
                req.params.and_then(|p| serde_json::from_value(p).ok());

            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);

            let configured_upstream = {
                let cfg = state.config.lock().await;
                cfg.git.upstream.clone()
            };

            let base_ref = params
                .and_then(|p| p.base_ref)
                .filter(|b| !b.trim().is_empty())
                .or_else(|| tauqe_core::git::detect_upstream_branch(repo_dir, configured_upstream.as_deref()))
                .unwrap_or_else(|| "HEAD~1".to_string());

            let commits_ahead = match tauqe_core::git::get_commits_ahead(repo_dir, &base_ref) {
                Ok(c) => c,
                Err(err) => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "COMMITS_AHEAD_FAILED".to_string(),
                            message: format!("Failed to get commits ahead of {}: {}", base_ref, err),
                            data: None,
                        }),
                    };
                }
            };

            if commits_ahead.is_empty() {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "NO_COMMITS_TO_SQUASH".to_string(),
                        message: format!("No commits to squash ahead of '{}'", base_ref),
                        data: None,
                    }),
                };
            }

            let cumulative_diff = tauqe_core::git::get_cumulative_diff(repo_dir, &base_ref).unwrap_or_default();

            let (pinned_files, target_lang) = {
                let session = state.session.lock().await;
                let state_ctx = session.context_manager.get_state();
                let pinned_paths: std::collections::HashSet<String> = state_ctx
                    .items
                    .into_iter()
                    .filter(|i| i.layer == tauqe_protocol::ContextLayer::Pinned)
                    .map(|i| i.path)
                    .collect();
                let all_files = session.context_manager.read_context_files();
                let pinned: Vec<_> = all_files.into_iter().filter(|f| pinned_paths.contains(&f.path)).collect();
                let lang = session.history_manager.detected_language().map(|s| s.to_string());
                (pinned, lang)
            };

            let prompt = tauqe_core::prompt::build_squash_commit_prompt(
                &commits_ahead,
                &cumulative_diff,
                &pinned_files,
                target_lang.as_deref(),
            );

            let (api_key, model) = {
                let cfg = state.config.lock().await;
                let key = cfg.providers.openrouter.as_ref().and_then(|o| o.api_key.clone()).unwrap_or_default();
                let m = cfg.models.default.clone();
                (key, m)
            };

            let suggested_message = if !api_key.is_empty() {
                let client = tauqe_core::model::openrouter::OpenRouterClient::new(api_key);
                let (tx, mut rx) = tokio::sync::mpsc::channel(50);
                let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                let msgs = vec![tauqe_core::model::gateway::ChatMessage::user(&prompt)];
                let stream_task = tokio::spawn(async move {
                    client.stream_chat(&model, msgs, tx, cancel_rx).await
                });
                let mut full_text = String::new();
                while let Some(ev) = rx.recv().await {
                    if let tauqe_core::model::gateway::StreamEvent::TextDelta(delta) = ev {
                        full_text.push_str(&delta);
                    }
                }
                let _ = stream_task.await;
                let trimmed = full_text.trim();
                let cleaned = trimmed
                    .strip_prefix("```")
                    .and_then(|s| s.strip_suffix("```"))
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string();
                if cleaned.is_empty() {
                    commits_ahead.first().map(|c| c.subject.clone()).unwrap_or_else(|| "Squashed commits".to_string())
                } else {
                    cleaned
                }
            } else {
                commits_ahead.first().map(|c| c.subject.clone()).unwrap_or_else(|| "Squashed commits".to_string())
            };

            let items: Vec<tauqe_protocol::GitSquashCommitItem> = commits_ahead
                .into_iter()
                .map(|c| tauqe_protocol::GitSquashCommitItem {
                    hash: c.hash,
                    author: c.author,
                    date: c.date,
                    subject: c.subject,
                })
                .collect();

            let diff_lines = cumulative_diff.lines().count();
            let diff_stat = format!("{} lines of diff", diff_lines);

            let preview_res = tauqe_protocol::GitSquashPreviewResult {
                base_ref,
                commits: items,
                suggested_message,
                diff_stat,
            };

            Response {
                id: req.id,
                result: Some(serde_json::to_value(preview_res).unwrap()),
                error: None,
            }
        }
        methods::GIT_SQUASH_APPLY => {
            if state.active_cancel.lock().await.is_some() {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "OPERATION_IN_PROGRESS".to_string(),
                        message: "Cannot squash commits while a model operation is in progress".to_string(),
                        data: None,
                    }),
                };
            }

            let params: tauqe_protocol::GitSquashApplyParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
                Some(p) => p,
                None => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "INVALID_PARAMS".to_string(),
                            message: "Missing base_ref or message".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_dir = Path::new(&repo_state.root);

            let commits_ahead = match tauqe_core::git::get_commits_ahead(repo_dir, &params.base_ref) {
                Ok(c) => c,
                Err(err) => {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "COMMITS_AHEAD_FAILED".to_string(),
                            message: format!("Failed to get commits ahead of {}: {}", params.base_ref, err),
                            data: None,
                        }),
                    };
                }
            };

            let commits_count = commits_ahead.len();

            match tauqe_core::git::squash_to_single_commit(repo_dir, &params.base_ref, &params.message) {
                Ok(squashed_hash) => {
                    let first_line = params.message.lines().next().unwrap_or("Squash commits").to_string();
                    let msg_text = format!("Squashed {} commits ahead of {} into {}.", commits_count, params.base_ref, squashed_hash);

                    {
                        let mut session = state.session.lock().await;
                        let _ = session.history_manager.record_response(
                            &msg_text,
                            Some(squashed_hash.clone()),
                            Some(first_line),
                            Vec::new(),
                        );
                    }

                    let new_repo_state = tauqe_core::git::get_repository_state(Some(repo_dir));
                    send_event(&Event {
                        method: events::GIT_STATE_CHANGED.to_string(),
                        params: Some(serde_json::json!({ "repository": new_repo_state })),
                    })
                    .await;

                    let apply_result = tauqe_protocol::GitSquashApplyResult {
                        squashed_commit: squashed_hash,
                        message: params.message,
                        base_ref: params.base_ref,
                    };

                    send_event(&Event {
                        method: events::GIT_SQUASH_COMPLETED.to_string(),
                        params: Some(serde_json::to_value(&apply_result).unwrap()),
                    })
                    .await;

                    Response {
                        id: req.id,
                        result: Some(serde_json::to_value(apply_result).unwrap()),
                        error: None,
                    }
                }
                Err(err) => Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "SQUASH_FAILED".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                },
            }
        }
        "system/status" => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let repo_opt = if !repo_path.as_os_str().is_empty() {
                Some(repo_path)
            } else {
                None
            };
            let cfg = state.config.lock().await;
            let config_file = tauqe_core::config::find_config_file(repo_opt);
            let has_config = config_file.is_some();
            let config_path = config_file.map(|p| p.to_string_lossy().to_string());
            let default_config_path = tauqe_core::config::default_config_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
            let credentials_file = tauqe_core::config::find_credentials_file(repo_opt);
            let credentials_path = credentials_file.map(|p| p.to_string_lossy().to_string());
            let default_credentials_path = tauqe_core::config::default_credentials_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let ready = has_api_key;

            let status_json = serde_json::json!({
                "has_config": has_config,
                "config_path": config_path,
                "default_config_path": default_config_path,
                "has_api_key": has_api_key,
                "credentials_path": credentials_path,
                "default_credentials_path": default_credentials_path,
                "ready": ready,
                "model": cfg.models.default,
                "available_models": cfg.models.available,
            });

            Response {
                id: req.id,
                result: Some(status_json),
                error: None,
            }
        }
        "config/reload" => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let repo_opt = if !repo_path.as_os_str().is_empty() {
                Some(repo_path)
            } else {
                None
            };
            let new_cfg = tauqe_core::config::load_config(repo_opt);
            {
                let mut cfg = state.config.lock().await;
                *cfg = new_cfg;
            }
            let cfg = state.config.lock().await;
            let updated_state = config_state(&cfg);
            send_event(&Event {
                method: events::CONFIG_CHANGED.to_string(),
                params: Some(serde_json::to_value(&updated_state).unwrap()),
            })
            .await;

            let config_file = tauqe_core::config::find_config_file(repo_opt);
            let has_config = config_file.is_some();
            let config_path = config_file.map(|p| p.to_string_lossy().to_string());
            let default_config_path = tauqe_core::config::default_config_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
            let credentials_file = tauqe_core::config::find_credentials_file(repo_opt);
            let credentials_path = credentials_file.map(|p| p.to_string_lossy().to_string());
            let default_credentials_path = tauqe_core::config::default_credentials_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let ready = has_api_key;

            let status_json = serde_json::json!({
                "reloaded": true,
                "has_config": has_config,
                "config_path": config_path,
                "default_config_path": default_config_path,
                "has_api_key": has_api_key,
                "credentials_path": credentials_path,
                "default_credentials_path": default_credentials_path,
                "ready": ready,
                "model": cfg.models.default,
                "available_models": cfg.models.available,
            });

            Response {
                id: req.id,
                result: Some(status_json),
                error: None,
            }
        }
        "config/create" => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let repo_opt = if !repo_path.as_os_str().is_empty() {
                Some(repo_path)
            } else {
                None
            };
            let model = req
                .params
                .as_ref()
                .and_then(|p| p.get("model"))
                .and_then(|m| m.as_str())
                .unwrap_or("anthropic/claude-3.7-sonnet");

            let target_path = tauqe_core::config::default_config_path(repo_opt);
            if let Err(err) = tauqe_core::config::write_default_config(&target_path, model) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "CONFIG_CREATE_FAILED".to_string(),
                        message: format!("Failed to create config file at {:?}: {}", target_path, err),
                        data: None,
                    }),
                };
            }

            let new_cfg = tauqe_core::config::load_config(repo_opt);
            {
                let mut cfg = state.config.lock().await;
                *cfg = new_cfg;
            }
            let cfg = state.config.lock().await;
            let updated_state = config_state(&cfg);
            send_event(&Event {
                method: events::CONFIG_CHANGED.to_string(),
                params: Some(serde_json::to_value(&updated_state).unwrap()),
            })
            .await;

            let config_path = Some(target_path.to_string_lossy().to_string());
            let default_config_path = target_path.to_string_lossy().to_string();

            let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
            let credentials_file = tauqe_core::config::find_credentials_file(repo_opt);
            let credentials_path = credentials_file.map(|p| p.to_string_lossy().to_string());
            let default_credentials_path = tauqe_core::config::default_credentials_path(repo_opt)
                .to_string_lossy()
                .to_string();

            Response {
                id: req.id,
                result: Some(serde_json::json!({
                    "created": true,
                    "has_config": true,
                    "config_path": config_path,
                    "default_config_path": default_config_path,
                    "has_api_key": has_api_key,
                    "credentials_path": credentials_path,
                    "default_credentials_path": default_credentials_path,
                    "ready": has_api_key,
                    "model": cfg.models.default,
                    "available_models": cfg.models.available,
                })),
                error: None,
            }
        }
        "credentials/save" => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let repo_opt = if !repo_path.as_os_str().is_empty() {
                Some(repo_path)
            } else {
                None
            };
            let api_key = req
                .params
                .as_ref()
                .and_then(|p| p.get("api_key"))
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .trim();

            if api_key.is_empty() {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_KEY".to_string(),
                        message: "API key cannot be empty".to_string(),
                        data: None,
                    }),
                };
            }

            let target_path = tauqe_core::config::default_credentials_path(repo_opt);
            if let Err(err) = tauqe_core::config::write_credentials_file(&target_path, api_key) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "CREDENTIALS_SAVE_FAILED".to_string(),
                        message: format!("Failed to write credentials file at {:?}: {}", target_path, err),
                        data: None,
                    }),
                };
            }

            let new_cfg = tauqe_core::config::load_config(repo_opt);
            {
                let mut cfg = state.config.lock().await;
                *cfg = new_cfg;
            }
            let cfg = state.config.lock().await;
            let updated_state = config_state(&cfg);
            send_event(&Event {
                method: events::CONFIG_CHANGED.to_string(),
                params: Some(serde_json::to_value(&updated_state).unwrap()),
            })
            .await;

            let config_file = tauqe_core::config::find_config_file(repo_opt);
            let has_config = config_file.is_some();
            let config_path = config_file.map(|p| p.to_string_lossy().to_string());
            let default_config_path = tauqe_core::config::default_config_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
            let credentials_path = Some(target_path.to_string_lossy().to_string());
            let default_credentials_path = target_path.to_string_lossy().to_string();

            Response {
                id: req.id,
                result: Some(serde_json::json!({
                    "saved": true,
                    "has_config": has_config,
                    "config_path": config_path,
                    "default_config_path": default_config_path,
                    "has_api_key": has_api_key,
                    "credentials_path": credentials_path,
                    "default_credentials_path": default_credentials_path,
                    "ready": has_api_key,
                    "model": cfg.models.default,
                    "available_models": cfg.models.available,
                })),
                error: None,
            }
        }
        "credentials/create_stub" => {
            let repo_state = tauqe_core::git::get_repository_state(None);
            let repo_path = Path::new(&repo_state.root);
            let repo_opt = if !repo_path.as_os_str().is_empty() {
                Some(repo_path)
            } else {
                None
            };
            let target_path = tauqe_core::config::default_credentials_path(repo_opt);
            if let Err(err) = tauqe_core::config::write_credentials_stub(&target_path) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "CREDENTIALS_STUB_FAILED".to_string(),
                        message: format!("Failed to write credentials stub at {:?}: {}", target_path, err),
                        data: None,
                    }),
                };
            }

            let cfg = state.config.lock().await;
            let config_file = tauqe_core::config::find_config_file(repo_opt);
            let has_config = config_file.is_some();
            let config_path = config_file.map(|p| p.to_string_lossy().to_string());
            let default_config_path = tauqe_core::config::default_config_path(repo_opt)
                .to_string_lossy()
                .to_string();

            let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
            let credentials_path = Some(target_path.to_string_lossy().to_string());
            let default_credentials_path = target_path.to_string_lossy().to_string();

            Response {
                id: req.id,
                result: Some(serde_json::json!({
                    "stub_created": true,
                    "has_config": has_config,
                    "config_path": config_path,
                    "default_config_path": default_config_path,
                    "has_api_key": has_api_key,
                    "credentials_path": credentials_path,
                    "default_credentials_path": default_credentials_path,
                    "ready": has_api_key,
                    "model": cfg.models.default,
                    "available_models": cfg.models.available,
                })),
                error: None,
            }
        }
        methods::CONFIG_GET => {
            let cfg = state.config.lock().await;
            let result = config_state(&cfg);
            Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            }
        }
        methods::CONFIG_SET => {
            {
                if state.active_cancel.lock().await.is_some() {
                    return Response {
                        id: req.id,
                        result: None,
                        error: Some(ResponseError {
                            code: "OPERATION_IN_PROGRESS".to_string(),
                            message: "Cannot change workflow, edit protocol or model while a model operation is in progress".to_string(),
                            data: None,
                        }),
                    };
                }
            }

            let params: ConfigSetParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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

            let updated_state = {
                let mut cfg = state.config.lock().await;
                if let Some(requested_model) = params.model.as_ref() {
                    let requested = requested_model.trim();
                    if !cfg.models.available.iter().any(|m| m == requested) {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "INVALID_MODEL".to_string(),
                                message: format!(
                                    "Unknown model '{}'. Available models: {}",
                                    requested,
                                    cfg.models.available.join(", ")
                                ),
                                data: None,
                            }),
                        };
                    }
                }
                if let Some(wf) = params.workflow {
                    if !WorkflowFactory::is_valid(&wf) {
                        return Response {
                            id: req.id,
                            result: None,
                            error: Some(ResponseError {
                                code: "INVALID_WORKFLOW".to_string(),
                                message: format!(
                                    "Unknown workflow '{}'. Available workflows: {}",
                                    wf,
                                    WorkflowFactory::available_workflows().join(", ")
                                ),
                                data: None,
                            }),
                        };
                    }
                    cfg.edit.workflow = wf;
                }
                if let Some(proto) = params.edit_protocol {
                    match EditProtocolFactory::canonical_name(&proto) {
                        Some(canonical) => {
                            cfg.edit.protocol = canonical;
                        }
                        None => {
                            return Response {
                                id: req.id,
                                result: None,
                                error: Some(ResponseError {
                                    code: "INVALID_PROTOCOL".to_string(),
                                    message: format!(
                                        "Unknown edit protocol '{}'. Available protocols: {}",
                                        proto,
                                        EditProtocolFactory::available_protocols().join(", ")
                                    ),
                                    data: None,
                                }),
                            };
                        }
                    }
                }
                if let Some(requested_model) = params.model {
                    cfg.models.default = requested_model.trim().to_string();
                }
                config_state(&cfg)
            };

            send_event(&Event {
                method: events::CONFIG_CHANGED.to_string(),
                params: Some(serde_json::to_value(&updated_state).unwrap()),
            })
            .await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(updated_state).unwrap()),
                error: None,
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
            let params: ContextAddParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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
                match session
                    .context_manager
                    .add_file(&params.path, params.access)
                {
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
            })
            .await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_ADD_PATTERN => {
            let params: ContextAddPatternParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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
                let repo_state = tauqe_core::git::get_repository_state(None);
                let repo_dir = Path::new(&repo_state.root);
                let available_files =
                    tauqe_core::git::list_repository_files(Some(repo_dir)).unwrap_or_default();

                let mut session = state.session.lock().await;
                match session.context_manager.add_files_by_pattern(
                    &params.pattern,
                    params.access,
                    &available_files,
                ) {
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
            })
            .await;

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
            let params: ContextRemoveParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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
            })
            .await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::CONTEXT_SET_ACCESS => {
            let params: ContextSetAccessParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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
                match session
                    .context_manager
                    .set_access(&params.path, params.access)
                {
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
            })
            .await;

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
            })
            .await;

            Response {
                id: req.id,
                result: Some(serde_json::to_value(ctx_state).unwrap()),
                error: None,
            }
        }
        methods::MODEL_ASK => {
            let params: ModelAskParams =
                match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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

            let (api_key, model, edit_config, toolchain_config) = {
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

                (
                    key,
                    cfg.models.default.clone(),
                    cfg.edit.clone(),
                    cfg.toolchain.clone(),
                )
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
                *state.active_cancel.lock().await = Some(ActiveOperation {
                    cancel_tx: cancel_tx.clone(),
                    abort_handle: None,
                });
            }

            let client = OpenRouterClient::new(api_key);
            let prompt_for_spawn = params.prompt.clone();
            let state_for_spawn = Arc::clone(state);
            let op_id_for_spawn = op_id.clone();
            let model_for_spawn = model.clone();

            let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

            tokio::spawn(async move {
                let protocol = EditProtocolFactory::create_protocol(&edit_config.protocol)
                    .unwrap_or_else(|_| Box::new(XmlEditProtocol));

                let workflow = WorkflowFactory::create_workflow_with_edit_config(
                    &edit_config.workflow,
                    &toolchain_config,
                    Some(&edit_config),
                )
                .unwrap_or_else(|_| {
                    Box::new(GitEditWorkflow::new(
                        Some(edit_config.max_retries).or(toolchain_config.max_retries),
                    ))
                });

                let (wf_tx, wf_rx) = (tx.clone(), cancel_rx.clone());
                drop(tx); // Drop local tx clone so rx closes when wf_task finishes

                let state_clone = Arc::clone(&state_for_spawn);
                let model_clone = model_for_spawn.clone();
                let prompt_clone = prompt_for_spawn.clone();

                let workflow_future = async move {
                    let mut session = state_clone.session.lock().await;
                    let ModelSession {
                        ref mut context_manager,
                        ref mut history_manager,
                        ..
                    } = *session;
                    workflow
                        .execute(
                            &prompt_clone,
                            &client,
                            &model_clone,
                            context_manager,
                            history_manager,
                            protocol.as_ref(),
                            wf_tx,
                            wf_rx,
                        )
                        .await
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
                        StreamEvent::ContextChanged(state) => {
                            let ev = Event {
                                method: events::CONTEXT_CHANGED.to_string(),
                                params: Some(serde_json::json!({ "state": state })),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditStarted => {
                            edit_events_sent = true;
                            let ev = Event {
                                method: events::EDIT_STARTED.to_string(),
                                params: Some(
                                    serde_json::to_value(EditStartedEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditFileStarted { path, op_type } => {
                            edit_events_sent = true;
                            let ev = Event {
                                method: events::EDIT_FILE_STARTED.to_string(),
                                params: Some(
                                    serde_json::to_value(EditFileStartedEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        path,
                                        op_type,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditHunk {
                            path,
                            hunk_index,
                            old_text,
                            new_text,
                        } => {
                            let ev = Event {
                                method: events::EDIT_HUNK.to_string(),
                                params: Some(
                                    serde_json::to_value(EditHunkEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        path,
                                        hunk_index,
                                        old_text,
                                        new_text,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditFileDone {
                            path,
                            status,
                            error,
                            hunks_count,
                        } => {
                            let ev = Event {
                                method: events::EDIT_FILE_DONE.to_string(),
                                params: Some(
                                    serde_json::to_value(EditFileDoneEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        path,
                                        status,
                                        error,
                                        hunks_count,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::EditFileRetrying {
                            path,
                            attempt,
                            max_retries,
                            reason,
                        } => {
                            let ev = Event {
                                method: events::EDIT_FILE_RETRYING.to_string(),
                                params: Some(
                                    serde_json::to_value(EditFileRetryingEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        path,
                                        attempt,
                                        max_retries,
                                        reason,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::ToolchainStarted { command } => {
                            let ev = Event {
                                method: events::TOOLCHAIN_STARTED.to_string(),
                                params: Some(
                                    serde_json::to_value(ToolchainStartedEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        command,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::ToolchainResult {
                            command,
                            success,
                            output,
                        } => {
                            let ev = Event {
                                method: events::TOOLCHAIN_RESULT.to_string(),
                                params: Some(
                                    serde_json::to_value(ToolchainResultEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        command,
                                        success,
                                        output,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::Usage(usage) => {
                            let round_cost = usage.cost;
                            accumulate_usage(&mut usage_info, usage.clone());
                            let current_session_cost = {
                                let base_cost = *state_for_spawn.total_cost.lock().await;
                                base_cost + usage_info.as_ref().and_then(|u| u.cost).unwrap_or(0.0)
                            };
                            let ev = Event {
                                method: events::MODEL_USAGE.to_string(),
                                params: Some(
                                    serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        usage,
                                        session_total_cost: current_session_cost,
                                        current_cost: round_cost,
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&ev).await;
                        }
                        StreamEvent::Done => {
                            // Single stream done; do not break, wait until all pipeline events are received
                        }
                        StreamEvent::Cancelled => {
                            if edit_events_sent && !edit_finished_sent {
                                send_edit_aborted(
                                    &op_id_for_spawn,
                                    "Operation cancelled before edits were applied".to_string(),
                                )
                                .await;
                                edit_finished_sent = true;
                            }
                            let cancelled_event = Event {
                                method: events::MODEL_CANCELLED.to_string(),
                                params: Some(
                                    serde_json::json!({ "operation_id": op_id_for_spawn }),
                                ),
                            };
                            send_event(&cancelled_event).await;
                            is_cancelled = true;
                            break;
                        }
                        StreamEvent::Error(err) => {
                            if edit_events_sent && !edit_finished_sent {
                                send_edit_aborted(
                                    &op_id_for_spawn,
                                    format!("Operation failed before edits were applied: {}", err),
                                )
                                .await;
                                edit_finished_sent = true;
                            }
                            let err_event = Event {
                                method: events::MODEL_ERROR.to_string(),
                                params: Some(serde_json::json!({
                                    "operation_id": op_id_for_spawn,
                                    "message": err,
                                })),
                            };
                            send_event(&err_event).await;
                            is_error = true;
                            break;
                        }
                    }
                }

                if is_cancelled || is_error {
                    let abort_handle = wf_task.abort_handle();
                    if tokio::time::timeout(tokio::time::Duration::from_millis(500), wf_task).await.is_err() {
                        abort_handle.abort();
                    }
                    *state_for_spawn.active_cancel.lock().await = None;
                    return;
                }

                match wf_task.await {
                    Ok(Ok(wf_result)) => {
                        let session = state_for_spawn.session.lock().await;

                        let usage_to_send = usage_info.unwrap_or_default();
                        let cost = usage_to_send.cost.unwrap_or(0.0);
                        let total_cost = {
                            let mut total = state_for_spawn.total_cost.lock().await;
                            *total += cost;
                            *total
                        };

                        let usage_event = Event {
                            method: events::MODEL_USAGE.to_string(),
                            params: Some(
                                serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                                    operation_id: op_id_for_spawn.clone(),
                                    usage: usage_to_send.clone(),
                                    session_total_cost: total_cost,
                                    current_cost: None,
                                })
                                .unwrap(),
                            ),
                        };
                        send_event(&usage_event).await;

                        if let tauqe_protocol::ModelResult::Edit {
                            applied,
                            ref error,
                            ref changed_files,
                            ref commit_hash,
                            ref summary,
                            ..
                        } = wf_result.result
                        {
                            let finished_edit_event = Event {
                                method: events::EDIT_FINISHED.to_string(),
                                params: Some(
                                    serde_json::to_value(EditFinishedEvent {
                                        operation_id: op_id_for_spawn.clone(),
                                        applied,
                                        error: error.clone(),
                                        changed_files: changed_files.clone(),
                                        commit_hash: commit_hash.clone(),
                                    })
                                    .unwrap(),
                                ),
                            };
                            send_event(&finished_edit_event).await;
                            edit_finished_sent = true;

                            if let Some(hash) = commit_hash {
                                send_event(&Event {
                                    method: events::GIT_COMMIT_CREATED.to_string(),
                                    params: Some(
                                        serde_json::to_value(GitCommitCreatedEvent {
                                            commit_hash: hash.clone(),
                                            summary: summary.clone(),
                                            changed_files: changed_files.clone(),
                                        })
                                        .unwrap(),
                                    ),
                                })
                                .await;

                                let repo_state = tauqe_core::git::get_repository_state(None);
                                send_event(&Event {
                                    method: events::GIT_STATE_CHANGED.to_string(),
                                    params: Some(serde_json::json!({ "repository": repo_state })),
                                })
                                .await;
                            }
                        }

                        let ctx_state = session.context_manager.get_state();
                        send_event(&Event {
                            method: events::CONTEXT_CHANGED.to_string(),
                            params: Some(serde_json::json!({ "state": ctx_state })),
                        })
                        .await;

                        if edit_events_sent && !edit_finished_sent {
                            send_edit_aborted(
                                &op_id_for_spawn,
                                "Edits were streamed but no edit result was produced".to_string(),
                            )
                            .await;
                        }

                        let result_event = Event {
                            method: events::MODEL_RESULT.to_string(),
                            params: Some(serde_json::json!(ModelResultEvent {
                                operation_id: op_id_for_spawn.clone(),
                                result: wf_result.result,
                                usage: Some(usage_to_send),
                                session_total_cost: Some(total_cost),
                                current_cost: None,
                            })),
                        };
                        send_event(&result_event).await;

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
                        if edit_events_sent && !edit_finished_sent {
                            send_edit_aborted(
                                &op_id_for_spawn,
                                format!("Workflow failed: {}", err),
                            )
                            .await;
                        }
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
                        if edit_events_sent && !edit_finished_sent {
                            send_edit_aborted(
                                &op_id_for_spawn,
                                format!("Workflow task failed: {}", join_err),
                            )
                            .await;
                        }
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

                *state_for_spawn.active_cancel.lock().await = None;
            });

            Response {
                id: req.id,
                result: Some(serde_json::json!({ "operation_id": op_id })),
                error: None,
            }
        }
        methods::MODEL_CANCEL => {
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
        methods::MODEL_CLEAR_HISTORY => {
            let mut session = state.session.lock().await;
            let _ = session.history_manager.clear();
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

fn config_state(cfg: &AppConfig) -> ConfigState {
    ConfigState {
        workflow: cfg.edit.workflow.clone(),
        edit_protocol: cfg.edit.protocol.clone(),
        model: cfg.models.default.clone(),
        available_workflows: WorkflowFactory::available_workflows(),
        available_edit_protocols: EditProtocolFactory::available_protocols(),
        available_models: cfg.models.available.clone(),
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

/// Emits a failed `edit/finished` so clients never keep an edit block in a pending state.
async fn send_edit_aborted(op_id: &str, error: String) {
    send_event(&Event {
        method: events::EDIT_FINISHED.to_string(),
        params: Some(
            serde_json::to_value(EditFinishedEvent {
                operation_id: op_id.to_string(),
                applied: false,
                error: Some(error),
                changed_files: Vec::new(),
                commit_hash: None,
            })
            .unwrap(),
        ),
    })
    .await;
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
