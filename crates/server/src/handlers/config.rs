use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauqe_core::config::{load_config, AppConfig};
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    events, ConfigSetParams, ConfigState, CredentialsSaveParams, Event, ModelSelection, Request,
    Response, ResponseError, SystemStatusResult,
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

/// Resolves the repository-bound configuration directory from the single
/// authoritative repository root held by [`AppState`], instead of re-deriving it
/// from the process CWD. Returns `None` when no repository is available yet.
async fn resolved_repo_root(state: &Arc<AppState>) -> Option<PathBuf> {
    let root = state.repo_root().await;
    if root.as_os_str().is_empty() {
        None
    } else {
        Some(root)
    }
}

/// Assembles the authoritative [`SystemStatusResult`] from a single source so
/// derived fields such as `ready` (which requires both a Git repository and an
/// API key) stay consistent across every status-producing handler.
///
/// `config_path` and `credentials_path` may be supplied with the freshly written
/// path after a create/save operation; otherwise the discovered file is reported.
fn build_system_status(
    cfg: &AppConfig,
    selection: &ModelSelection,
    repo_opt: Option<&Path>,
    config_path: Option<String>,
    credentials_path: Option<String>,
) -> SystemStatusResult {
    let has_git = tauqe_core::git::is_git_repository(None);
    let has_api_key = tauqe_core::config::has_openrouter_key(cfg);

    let config_path = config_path.or_else(|| {
        tauqe_core::config::find_config_file(repo_opt).map(|p| p.to_string_lossy().to_string())
    });
    let credentials_path = credentials_path.or_else(|| {
        tauqe_core::config::find_credentials_file(repo_opt).map(|p| p.to_string_lossy().to_string())
    });

    SystemStatusResult {
        has_git,
        has_config: config_path.is_some(),
        config_path,
        default_config_path: tauqe_core::config::default_config_path(repo_opt)
            .to_string_lossy()
            .to_string(),
        has_api_key,
        credentials_path,
        default_credentials_path: tauqe_core::config::default_credentials_path(repo_opt)
            .to_string_lossy()
            .to_string(),
        ready: has_git && has_api_key,
        model: cfg.resolve_model(selection),
        selection: Some(selection.clone()),
        roles: Some(cfg.roles_summary()),
        history_model: Some(cfg.history_model()),
        available_models: cfg.available_models(),
    }
}

pub async fn handle_config_get(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.config.lock().await;
    let result = config_state(&guard.config, Some(&guard.selection));
    Response::ok_typed(req.id, &result)
}

pub async fn handle_config_set(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
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
        let mut guard = state.config.lock().await;

        // Validate every requested field into local candidates before mutating
        // shared state, so a single invalid field cannot leave the config
        // partially applied and desynchronized from the client.
        let mut new_selection = guard.selection.clone();
        if let Some(ref sel) = params.selection {
            let effective = guard.config.resolve_model(sel);
            let available = guard.config.available_models();
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
            new_selection = sel.clone();
        } else if let Some(ref requested_model) = params.model {
            let available = guard.config.available_models();
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
            new_selection = ModelSelection::Specific(requested_model.clone());
        }

        let mut new_scribe = guard.config.models.scribe.clone();
        if let Some(ref hist_model) = params.history_model {
            let available = guard.config.available_models();
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
            new_scribe = Some(hist_model.clone());
        }

        let mut new_workflow = guard.config.develop.workflow.clone();
        if let Some(ref wf) = params.workflow {
            if !WorkflowFactory::is_valid(wf) {
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
            new_workflow = wf.clone();
        }

        let mut new_protocol = guard.config.develop.protocol.clone();
        if let Some(ref proto) = params.edit_protocol {
            match EditProtocolFactory::canonical_name(proto) {
                Some(canonical) => {
                    new_protocol = canonical;
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

        // Every requested field is valid: commit the whole change atomically.
        guard.selection = new_selection;
        guard.config.models.scribe = new_scribe;
        guard.config.develop.workflow = new_workflow;
        guard.config.develop.protocol = new_protocol;

        config_state(&guard.config, Some(&guard.selection))
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    Response::ok_typed(req.id, &updated_state)
}

pub async fn handle_system_status(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &guard.selection, repo_opt, None, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_reload(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "OPERATION_IN_PROGRESS".to_string(),
                message: "Cannot reload configuration while a model operation is in progress"
                    .to_string(),
                data: None,
            }),
        };
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let new_cfg = load_config(repo_opt);
    let (updated_state, cur_sel_val) = {
        let mut guard = state.config.lock().await;
        guard.selection = new_cfg.default_selection();
        guard.config = new_cfg;
        let st = config_state(&guard.config, Some(&guard.selection));
        (st, guard.selection.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &cur_sel_val, repo_opt, None, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_create(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "OPERATION_IN_PROGRESS".to_string(),
                message: "Cannot create configuration while a model operation is in progress"
                    .to_string(),
                data: None,
            }),
        };
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

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
        let mut guard = state.config.lock().await;
        guard.selection = new_cfg.default_selection();
        guard.config = new_cfg;
        let st = config_state(&guard.config, Some(&guard.selection));
        (st, guard.selection.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let guard = state.config.lock().await;
    let config_path = Some(target_path.to_string_lossy().to_string());
    let status = build_system_status(&guard.config, &cur_sel_val, repo_opt, config_path, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_save(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let params: CredentialsSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
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

    let api_key = params.api_key.trim();

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
        let mut guard = state.config.lock().await;
        guard.config = new_cfg;
        let st = config_state(&guard.config, Some(&guard.selection));
        (st, guard.selection.clone())
    };

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let guard = state.config.lock().await;
    let credentials_path = Some(target_path.to_string_lossy().to_string());
    let status = build_system_status(&guard.config, &cur_sel_val, repo_opt, None, credentials_path);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_create_stub(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

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

    let guard = state.config.lock().await;
    let credentials_path = Some(target_path.to_string_lossy().to_string());
    let status =
        build_system_status(&guard.config, &guard.selection, repo_opt, None, credentials_path);

    Response::ok_typed(req.id, &status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelRef, ModelRole};

    #[tokio::test]
    async fn test_config_state_roles_and_scribe_history() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("anthropic/claude-3.5-haiku"));
        cfg.models.coder = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet").into());
        cfg.models.architect = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet").into());
        cfg.models.advisor = Some(ModelRef::openrouter("google/gemini-2.5-flash"));
        cfg.models.reviewer = Some(ModelRef::openrouter("anthropic/claude-3.7-sonnet").into());

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
        cfg.models.coder = Some(ModelRef::openrouter("model/coder").into());

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
