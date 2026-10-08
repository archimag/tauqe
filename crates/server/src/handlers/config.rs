use std::path::Path;
use std::sync::Arc;

use tauqe_core::config::{load_config, AppConfig};
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    events, ConfigSetParams, ConfigState, Event, Request, Response, ResponseError,
};

use crate::state::AppState;

pub fn config_state(cfg: &AppConfig) -> ConfigState {
    ConfigState {
        workflow: cfg.develop.workflow.clone(),
        edit_protocol: cfg.develop.protocol.clone(),
        model: cfg.active_model(),
        history_model: Some(cfg.history_model()),
        available_workflows: WorkflowFactory::available_workflows(),
        available_edit_protocols: EditProtocolFactory::available_protocols(),
        available_models: cfg.available_models(),
    }
}

pub async fn handle_config_get(req: Request, state: &Arc<AppState>) -> Response {
    let cfg = state.config.lock().await;
    let result = config_state(&cfg);
    Response {
        id: req.id,
        result: serde_json::to_value(result).ok(),
        error: None,
    }
}

pub async fn handle_config_set(req: Request, state: &Arc<AppState>) -> Response {
    if state.active_cancel.lock().await.is_some() {
        return Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "OPERATION_IN_PROGRESS".to_string(),
                message:
                    "Cannot change workflow, edit protocol or model while a model operation is in progress"
                        .to_string(),
                data: None,
            }),
        };
    }

    let params: ConfigSetParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
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
            let available = cfg.available_models();
            if !available.contains(requested_model) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_MODEL".to_string(),
                        message: format!(
                            "Unknown model '{}'. Available models: {}",
                            requested_model,
                            available
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        data: None,
                    }),
                };
            }
        }
        if let Some(hist_model) = params.history_model.as_ref() {
            let available = cfg.available_models();
            if !available.contains(hist_model) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_MODEL".to_string(),
                        message: format!(
                            "Unknown history model '{}'. Available models: {}",
                            hist_model,
                            available
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
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
            cfg.develop.workflow = wf;
        }
        if let Some(proto) = params.edit_protocol {
            match EditProtocolFactory::canonical_name(&proto) {
                Some(canonical) => {
                    cfg.develop.protocol = canonical;
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
            cfg.set_active_model(requested_model);
        }
        if let Some(hist_model) = params.history_model {
            cfg.history.model = Some(hist_model);
        }
        config_state(&cfg)
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    Response {
        id: req.id,
        result: serde_json::to_value(updated_state).ok(),
        error: None,
    }
}

pub async fn handle_system_status(req: Request, state: &Arc<AppState>) -> Response {
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

    let has_git = tauqe_core::git::is_git_repository(None);
    let ready = has_git && has_api_key;

    let status_json = serde_json::json!({
        "has_git": has_git,
        "has_config": has_config,
        "config_path": config_path,
        "default_config_path": default_config_path,
        "has_api_key": has_api_key,
        "credentials_path": credentials_path,
        "default_credentials_path": default_credentials_path,
        "ready": ready,
        "model": cfg.active_model(),
        "history_model": cfg.history_model(),
        "available_models": cfg.available_models(),
    });

    Response {
        id: req.id,
        result: Some(status_json),
        error: None,
    }
}

pub async fn handle_config_reload(req: Request, state: &Arc<AppState>) -> Response {
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = Path::new(&repo_state.root);
    let repo_opt = if !repo_path.as_os_str().is_empty() {
        Some(repo_path)
    } else {
        None
    };

    let new_cfg = load_config(repo_opt);
    {
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
    }
    let cfg = state.config.lock().await;
    let updated_state = config_state(&cfg);
    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

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

    let has_git = tauqe_core::git::is_git_repository(None);
    let ready = has_git && has_api_key;

    let status_json = serde_json::json!({
        "reloaded": true,
        "has_git": has_git,
        "has_config": has_config,
        "config_path": config_path,
        "default_config_path": default_config_path,
        "has_api_key": has_api_key,
        "credentials_path": credentials_path,
        "default_credentials_path": default_credentials_path,
        "ready": ready,
        "model": cfg.active_model(),
        "history_model": cfg.history_model(),
        "available_models": cfg.available_models(),
    });

    Response {
        id: req.id,
        result: Some(status_json),
        error: None,
    }
}

pub async fn handle_config_create(req: Request, state: &Arc<AppState>) -> Response {
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = Path::new(&repo_state.root);
    let repo_opt = if !repo_path.as_os_str().is_empty() {
        Some(repo_path)
    } else {
        None
    };

    let target_path = tauqe_core::config::default_config_path(repo_opt);
    if let Err(err) = tauqe_core::config::write_default_config(&target_path) {
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

    let new_cfg = load_config(repo_opt);
    {
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
    }
    let cfg = state.config.lock().await;
    let updated_state = config_state(&cfg);
    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

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
            "model": cfg.active_model(),
            "history_model": cfg.history_model(),
            "available_models": cfg.available_models(),
        })),
        error: None,
    }
}

pub async fn handle_credentials_save(req: Request, state: &Arc<AppState>) -> Response {
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
                message: format!(
                    "Failed to write credentials file at {:?}: {}",
                    target_path, err
                ),
                data: None,
            }),
        };
    }

    let new_cfg = load_config(repo_opt);
    {
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
    }
    let cfg = state.config.lock().await;
    let updated_state = config_state(&cfg);
    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

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
            "model": cfg.active_model(),
            "history_model": cfg.history_model(),
            "available_models": cfg.available_models(),
        })),
        error: None,
    }
}

pub async fn handle_credentials_create_stub(req: Request, state: &Arc<AppState>) -> Response {
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
                message: format!(
                    "Failed to write credentials stub at {:?}: {}",
                    target_path, err
                ),
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
            "model": cfg.active_model(),
            "history_model": cfg.history_model(),
            "available_models": cfg.available_models(),
        })),
        error: None,
    }
}
