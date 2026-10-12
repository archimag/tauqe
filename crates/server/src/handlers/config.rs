use std::path::Path;
use std::sync::Arc;

use tauqe_core::config::{load_config, AppConfig};
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    events, ConfigSetParams, ConfigState, Event, ModelSelection, Request, Response, ResponseError,
};

use crate::state::AppState;

pub fn config_state(cfg: &AppConfig, selection: Option<&ModelSelection>) -> ConfigState {
    let sel = selection.cloned().unwrap_or_else(|| cfg.default_selection());
    let effective = cfg.resolve_model(&sel);
    let roles = cfg.roles_summary();
    ConfigState {
        workflow: cfg.develop.workflow.clone(),
        edit_protocol: cfg.develop.protocol.clone(),
        model: effective,
        selection: Some(sel),
        roles: Some(roles),
        history_model: Some(cfg.history_model()),
        available_workflows: WorkflowFactory::available_workflows(),
        available_edit_protocols: EditProtocolFactory::available_protocols(),
        available_models: cfg.available_models(),
    }
}

pub async fn handle_config_get(req: Request, state: &Arc<AppState>) -> Response {
    let cur_sel = state.current_selection.lock().await;
    let cfg = state.config.lock().await;
    let result = config_state(&cfg, Some(&cur_sel));
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
        let mut cur_sel = state.current_selection.lock().await;
        let mut cfg = state.config.lock().await;
        if let Some(ref sel) = params.selection {
            let effective = cfg.resolve_model(sel);
            let available = cfg.available_models();
            if !available.contains(&effective) {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_MODEL".to_string(),
                        message: format!(
                            "Unknown model '{}'. Available models: {}",
                            effective,
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
            *cur_sel = sel.clone();
        } else if let Some(ref requested_model) = params.model {
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
            *cur_sel = ModelSelection::Specific(requested_model.clone());
        }

        if let Some(ref hist_model) = params.history_model {
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
            cfg.models.scribe = Some(hist_model.clone());
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

        config_state(&cfg, Some(&cur_sel))
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

    let cur_sel = state.current_selection.lock().await;
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
    let effective_model = cfg.resolve_model(&cur_sel);

    let status_json = serde_json::json!({
        "has_git": has_git,
        "has_config": has_config,
        "config_path": config_path,
        "default_config_path": default_config_path,
        "has_api_key": has_api_key,
        "credentials_path": credentials_path,
        "default_credentials_path": default_credentials_path,
        "ready": ready,
        "model": effective_model,
        "selection": *cur_sel,
        "roles": cfg.roles_summary(),
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
    let (updated_state, cur_sel_val) = {
        let mut cur_sel = state.current_selection.lock().await;
        *cur_sel = new_cfg.default_selection();
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
        let st = config_state(&cfg, Some(&cur_sel));
        (st, cur_sel.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

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
    let effective_model = cfg.resolve_model(&cur_sel_val);

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
        "model": effective_model,
        "selection": cur_sel_val,
        "roles": cfg.roles_summary(),
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
    let (updated_state, cur_sel_val) = {
        let mut cur_sel = state.current_selection.lock().await;
        *cur_sel = new_cfg.default_selection();
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
        let st = config_state(&cfg, Some(&cur_sel));
        (st, cur_sel.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let cfg = state.config.lock().await;
    let config_path = Some(target_path.to_string_lossy().to_string());
    let default_config_path = target_path.to_string_lossy().to_string();

    let has_api_key = tauqe_core::config::has_openrouter_key(&cfg);
    let credentials_file = tauqe_core::config::find_credentials_file(repo_opt);
    let credentials_path = credentials_file.map(|p| p.to_string_lossy().to_string());
    let default_credentials_path = tauqe_core::config::default_credentials_path(repo_opt)
        .to_string_lossy()
        .to_string();

    let effective_model = cfg.resolve_model(&cur_sel_val);

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
            "model": effective_model,
            "selection": cur_sel_val,
            "roles": cfg.roles_summary(),
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
    let (updated_state, cur_sel_val) = {
        let cur_sel = state.current_selection.lock().await;
        let mut cfg = state.config.lock().await;
        *cfg = new_cfg;
        let st = config_state(&cfg, Some(&cur_sel));
        (st, cur_sel.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

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
    let effective_model = cfg.resolve_model(&cur_sel_val);

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
            "model": effective_model,
            "selection": cur_sel_val,
            "roles": cfg.roles_summary(),
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

    let cur_sel = state.current_selection.lock().await;
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
    let effective_model = cfg.resolve_model(&cur_sel);

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
            "model": effective_model,
            "selection": *cur_sel,
            "roles": cfg.roles_summary(),
            "history_model": cfg.history_model(),
            "available_models": cfg.available_models(),
        })),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelRef, ModelRole};

    #[tokio::test]
    async fn test_config_state_roles_and_scribe_history() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("anthropic/claude-3.5-haiku"));
        cfg.models.coder = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet"));
        cfg.models.architect = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet"));
        cfg.models.advisor = Some(ModelRef::openrouter("google/gemini-2.5-flash"));
        cfg.models.reviewer = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet"));

        let st = config_state(&cfg, None);
        assert_eq!(
            st.history_model,
            Some(ModelRef::openrouter("anthropic/claude-3.5-haiku"))
        );

        let roles = st.roles.expect("roles summary must be present");
        assert_eq!(
            roles.scribe,
            ModelRef::openrouter("anthropic/claude-3.5-haiku")
        );
        assert_eq!(
            roles.coder,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
        assert_eq!(
            roles.architect,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
        assert_eq!(
            roles.advisor,
            ModelRef::openrouter("google/gemini-2.5-flash")
        );
        assert_eq!(
            roles.reviewer,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
    }

    #[tokio::test]
    async fn test_handle_config_set_model_role_selection() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("model/scribe"));
        cfg.models.coder = Some(ModelRef::openrouter("model/coder"));

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: "config/set".to_string(),
            params: Some(serde_json::to_value(ConfigSetParams {
                workflow: None,
                edit_protocol: None,
                model: None,
                selection: Some(ModelSelection::Role(ModelRole::Scribe)),
                history_model: None,
            }).unwrap()),
        };

        let resp = handle_config_set(req, &state).await;
        assert!(resp.error.is_none());

        let res_state: ConfigState = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(
            res_state.selection,
            Some(ModelSelection::Role(ModelRole::Scribe))
        );
        assert_eq!(res_state.model, ModelRef::openrouter("model/scribe"));
    }
}
