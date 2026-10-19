use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauqe_core::config::{load_config_with_options, AppConfig, ConfigLoadOptions};
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    errors, events, ConfigSetParams, ConfigState, CredentialsSaveParams, Event, ModelRole,
    ModelRolesEffort, ModelRolesEntries, ModelRolesSummary, ModelSelection, Request, Response,
    SystemStatusResult,
};

use crate::state::AppState;

/// Builds the client-facing role summary, enriching each role's effective
/// concrete model with the reasoning effort resolved from its role list.
///
/// Every role-bearing response and event carries this summary so a client can
/// render each semantic role and its reasoning depth without re-reading the
/// configuration.
pub(crate) fn effective_roles_summary(cfg: &AppConfig) -> ModelRolesSummary {
    let mut summary = cfg.roles_summary();
    summary.efforts = Some(ModelRolesEffort {
        scribe: cfg.models.resolve_effort(ModelRole::Scribe),
        coder: cfg.models.resolve_effort(ModelRole::Coder),
        architect: cfg.models.resolve_effort(ModelRole::Architect),
        primary: cfg.models.resolve_effort(ModelRole::Primary),
        reviewer: cfg.models.resolve_effort(ModelRole::Reviewer),
    });
    summary.entries = Some(ModelRolesEntries {
        scribe: cfg.models.role_entries(ModelRole::Scribe),
        coder: cfg.models.role_entries(ModelRole::Coder),
        architect: cfg.models.role_entries(ModelRole::Architect),
        primary: cfg.models.role_entries(ModelRole::Primary),
        reviewer: cfg.models.role_entries(ModelRole::Reviewer),
    });
    summary
}

pub fn config_state(cfg: &AppConfig, selection: Option<&ModelSelection>) -> ConfigState {
    let sel = selection.cloned().unwrap_or_else(|| cfg.default_selection());
    let effective = cfg.resolve_model(&sel);
    let roles = effective_roles_summary(cfg);
    ConfigState {
        workflow: cfg.develop.workflow,
        edit_protocol: cfg.develop.protocol,
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

/// Returns the explicit `--config` and `--credentials` paths captured by the
/// server binary at startup.
///
/// The core engine never reads process arguments; the binary parses them once
/// and records them in the loaded [`AppConfig`] so every later reload keeps
/// honoring the same overrides.
pub(crate) async fn explicit_cli_paths(
    state: &Arc<AppState>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let guard = state.config.lock().await;
    (
        guard.config.explicit_config_path.clone(),
        guard.config.explicit_credentials_path.clone(),
    )
}

/// Builds the fallback configuration used when a reload fails.
///
/// The daemon's `--config` and `--credentials` overrides are stored inside
/// [`AppConfig`], so falling back to bare defaults would permanently erase them
/// for the rest of the daemon's lifetime. Re-attaching the captured overrides
/// keeps every later reload honoring the same CLI paths.
fn fallback_config_with_cli_paths(
    explicit_config: Option<PathBuf>,
    explicit_credentials: Option<PathBuf>,
) -> AppConfig {
    AppConfig {
        explicit_config_path: explicit_config,
        explicit_credentials_path: explicit_credentials,
        ..AppConfig::default()
    }
}

/// Reloads configuration for `repo_root`, re-applying the explicit path
/// overrides captured at startup.
pub(crate) async fn reload_config_with_overrides(
    state: &Arc<AppState>,
    repo_root: Option<&Path>,
) -> AppConfig {
    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
    // Keep the CLI overrides alongside the closure inputs so a failed reload
    // can restore them: callers replace `state.config` with whatever this
    // function returns, so the `--config` / `--credentials` paths captured at
    // daemon startup must survive on every code path, including the fallback.
    let fallback_config = explicit_config.clone();
    let fallback_credentials = explicit_credentials.clone();
    let repo_root = repo_root.map(Path::to_path_buf);
    // Loading configuration reads files from disk; offload it so the async
    // worker thread is never blocked.
    match super::offload(move || {
        load_config_with_options(ConfigLoadOptions {
            repo_root: repo_root.as_deref(),
            explicit_config: explicit_config.as_deref(),
            explicit_credentials: explicit_credentials.as_deref(),
        })
    })
    .await
    {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Failed to reload configuration; keeping defaults with CLI overrides preserved"
            );
            fallback_config_with_cli_paths(fallback_config, fallback_credentials)
        }
    }
}

/// Probes whether the repository is a Git working tree, off the async worker.
///
/// The probe shells out to git, so it runs on the blocking pool and resolves
/// the repository from the authoritative shared root instead of the process CWD.
async fn repo_has_git(repo_opt: Option<&Path>) -> bool {
    let root = repo_opt.map(Path::to_path_buf);
    super::offload(move || tauqe_core::git::is_git_repository(root.as_deref()))
        .await
        .unwrap_or(false)
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
    has_git: bool,
    config_path: Option<String>,
    credentials_path: Option<String>,
) -> SystemStatusResult {
    let has_api_key = tauqe_core::config::has_openrouter_key(cfg);

    let explicit_config = cfg.explicit_config_path.as_deref();
    let explicit_credentials = cfg.explicit_credentials_path.as_deref();

    let config_path = config_path.or_else(|| {
        tauqe_core::config::find_config_file(repo_opt, explicit_config)
            .map(|p| p.to_string_lossy().to_string())
    });
    let credentials_path = credentials_path.or_else(|| {
        tauqe_core::config::find_credentials_file(repo_opt, explicit_credentials)
            .map(|p| p.to_string_lossy().to_string())
    });

    SystemStatusResult {
        has_git,
        has_config: config_path.is_some(),
        config_path,
        default_config_path: tauqe_core::config::default_config_path(repo_opt, explicit_config)
            .to_string_lossy()
            .to_string(),
        has_api_key,
        credentials_path,
        default_credentials_path: tauqe_core::config::default_credentials_path(
            repo_opt,
            explicit_credentials,
        )
        .to_string_lossy()
        .to_string(),
        ready: has_git && has_api_key,
        model: cfg.resolve_model(selection),
        selection: Some(selection.clone()),
        roles: Some(effective_roles_summary(cfg)),
        history_model: Some(cfg.history_model()),
        available_models: cfg.available_models(),
        config_error: cfg.config_error.clone(),
    }
}

pub async fn handle_config_get(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.config.lock().await;
    let result = config_state(&guard.config, Some(&guard.selection));
    Response::ok_typed(req.id, &result)
}

pub async fn handle_config_set(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot change workflow, edit protocol or model while a model operation is in progress",
        );
    }

    let params: ConfigSetParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid parameters");
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
                return Response::err(
                    req.id,
                    errors::INVALID_MODEL,
                    format!(
                        "Unknown model '{}'. Available models: {}",
                        effective,
                        available
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            new_selection = sel.clone();
        } else if let Some(ref requested_model) = params.model {
            let available = guard.config.available_models();
            if !available.contains(requested_model) {
                return Response::err(
                    req.id,
                    errors::INVALID_MODEL,
                    format!(
                        "Unknown model '{}'. Available models: {}",
                        requested_model,
                        available
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            new_selection = ModelSelection::Specific(requested_model.clone());
        }

        let mut new_scribe = guard.config.models.scribe.clone();
        if let Some(ref hist_model) = params.history_model {
            let available = guard.config.available_models();
            if !available.contains(hist_model) {
                return Response::err(
                    req.id,
                    errors::INVALID_MODEL,
                    format!(
                        "Unknown history model '{}'. Available models: {}",
                        hist_model,
                        available
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            new_scribe = Some(hist_model.clone());
        }

        let mut new_workflow = guard.config.develop.workflow;
        if let Some(wf) = params.workflow {
            new_workflow = wf;
        }

        let mut new_protocol = guard.config.develop.protocol;
        if let Some(proto) = params.edit_protocol {
            new_protocol = proto;
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

    let has_git = repo_has_git(repo_opt).await;
    let guard = state.config.lock().await;
    let status =
        build_system_status(&guard.config, &guard.selection, repo_opt, has_git, None, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_reload(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot reload configuration while a model operation is in progress",
        );
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let new_cfg = reload_config_with_overrides(state, repo_opt).await;
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

    let has_git = repo_has_git(repo_opt).await;
    let guard = state.config.lock().await;
    let status =
        build_system_status(&guard.config, &cur_sel_val, repo_opt, has_git, None, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_create(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot create configuration while a model operation is in progress",
        );
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let (explicit_config, _) = explicit_cli_paths(state).await;
    let target_path =
        tauqe_core::config::default_config_path(repo_opt, explicit_config.as_deref());
    let write_path = target_path.clone();
    let write_outcome = match super::offload(move || {
        tauqe_core::config::write_default_config(&write_path)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            return Response::err(req.id, errors::CONFIG_CREATE_FAILED, err);
        }
    };
    if let Err(err) = write_outcome {
        return Response::err(
            req.id,
            errors::CONFIG_CREATE_FAILED,
            format!("Failed to create config file at {:?}: {}", target_path, err),
        );
    }

    let new_cfg = reload_config_with_overrides(state, repo_opt).await;
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

    let has_git = repo_has_git(repo_opt).await;
    let guard = state.config.lock().await;
    let config_path = Some(target_path.to_string_lossy().to_string());
    let status =
        build_system_status(&guard.config, &cur_sel_val, repo_opt, has_git, config_path, None);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_save(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot save credentials while a model operation is in progress",
        );
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let params: CredentialsSaveParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid parameters",
                );
            }
        };

    let api_key = params.api_key.trim();
    if api_key.is_empty() {
        return Response::err(req.id, errors::INVALID_KEY, "API key cannot be empty");
    }

    let (_, explicit_credentials) = explicit_cli_paths(state).await;
    let target_path =
        tauqe_core::config::default_credentials_path(repo_opt, explicit_credentials.as_deref());
    let write_path = target_path.clone();
    let api_key_owned = api_key.to_string();
    let write_outcome = match super::offload(move || {
        tauqe_core::config::write_credentials_file(&write_path, &api_key_owned)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            return Response::err(req.id, errors::CREDENTIALS_SAVE_FAILED, err);
        }
    };
    if let Err(err) = write_outcome {
        return Response::err(
            req.id,
            errors::CREDENTIALS_SAVE_FAILED,
            format!(
                "Failed to write credentials file at {:?}: {}",
                target_path, err
            ),
        );
    }

    let new_cfg = reload_config_with_overrides(state, repo_opt).await;
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

    let has_git = repo_has_git(repo_opt).await;
    let guard = state.config.lock().await;
    let credentials_path = Some(target_path.to_string_lossy().to_string());
    let status = build_system_status(
        &guard.config,
        &cur_sel_val,
        repo_opt,
        has_git,
        None,
        credentials_path,
    );

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_create_stub(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response::err(
            req.id,
            errors::OPERATION_IN_PROGRESS,
            "Cannot create a credentials stub while a model operation is in progress",
        );
    }

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let (_, explicit_credentials) = explicit_cli_paths(state).await;
    let target_path =
        tauqe_core::config::default_credentials_path(repo_opt, explicit_credentials.as_deref());
    let write_path = target_path.clone();
    let write_outcome = match super::offload(move || {
        tauqe_core::config::write_credentials_stub(&write_path)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            return Response::err(req.id, errors::CREDENTIALS_STUB_FAILED, err);
        }
    };
    if let Err(err) = write_outcome {
        return Response::err(
            req.id,
            errors::CREDENTIALS_STUB_FAILED,
            format!(
                "Failed to write credentials stub at {:?}: {}",
                target_path, err
            ),
        );
    }

    let has_git = repo_has_git(repo_opt).await;
    let guard = state.config.lock().await;
    let credentials_path = Some(target_path.to_string_lossy().to_string());
    let status = build_system_status(
        &guard.config,
        &guard.selection,
        repo_opt,
        has_git,
        None,
        credentials_path,
    );

    Response::ok_typed(req.id, &status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{EditProtocolKind, ModelRef, ModelRole, WorkflowKind};

    #[tokio::test]
    async fn test_config_state_roles_and_scribe_history() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("anthropic/claude-3.5-haiku"));
        cfg.models.coder = Some(vec![ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()]);
        cfg.models.architect =
            Some(vec![ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()]);
        cfg.models.primary = vec![ModelRef::openrouter("google/gemini-2.5-flash").into()];
        cfg.models.reviewer =
            Some(vec![ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()]);

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
            roles.primary,
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
        cfg.models.coder = Some(vec![ModelRef::openrouter("model/coder").into()]);

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: None,
                    edit_protocol: None,
                    model: None,
                    selection: Some(ModelSelection::Role(ModelRole::Scribe)),
                    history_model: None,
                })
                .unwrap(),
            ),
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

    #[tokio::test]
    async fn test_handle_config_set_workflow_and_protocol() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), std::env::temp_dir(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: Some(WorkflowKind::Naive),
                    edit_protocol: Some(EditProtocolKind::Structured),
                    model: None,
                    selection: None,
                    history_model: None,
                })
                .unwrap(),
            ),
        };

        let resp = handle_config_set(req, &state).await;
        assert!(resp.error.is_none());

        let res_state: ConfigState = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(res_state.workflow, WorkflowKind::Naive);
        assert_eq!(res_state.edit_protocol, EditProtocolKind::Structured);
    }

    /// A failed reload must not erase the CLI overrides captured at daemon
    /// startup, or every later reload would silently stop honoring the explicit
    /// `--config` / `--credentials` paths.
    #[test]
    fn fallback_config_preserves_explicit_cli_paths() {
        let cfg_path = PathBuf::from("/tmp/tauqe/custom.toml");
        let creds_path = PathBuf::from("/tmp/tauqe/credentials.toml");

        let fallback =
            fallback_config_with_cli_paths(Some(cfg_path.clone()), Some(creds_path.clone()));

        assert_eq!(fallback.explicit_config_path, Some(cfg_path));
        assert_eq!(fallback.explicit_credentials_path, Some(creds_path));
    }

    #[tokio::test]
    async fn test_credentials_create_stub_rejected_while_operation_active() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), std::env::temp_dir(), history_sender, out);

        let _guard = state
            .try_begin_operation()
            .expect("operation slot must be free");

        let req = Request {
            id: tauqe_protocol::RequestId::Number(3),
            method: "credentials/create_stub".to_string(),
            params: None,
        };

        let resp = handle_credentials_create_stub(req, &state).await;
        let err = resp.error.expect("stub creation must be rejected");
        assert_eq!(err.code, errors::OPERATION_IN_PROGRESS);
    }
}
