use crate::contracts::Project;
use std::path::PathBuf;
use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::models::ModelSelection;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    errors, events, ConfigSetParams, ConfigState, CredentialsSaveParams, EditProtocolKind, Event,
    Request, Response, ReviewPresetSummary, SystemStatusResult,
};

use crate::state::AppState;

/// Projects the core's resolved model-role configuration into the RPC contract.
pub(crate) fn effective_roles_summary(cfg: &AppConfig) -> tauqe_protocol::ModelRolesSummary {
    cfg.effective_roles_summary().project()
}

/// Projects the declared review presets into the compact snapshot a client
/// renders its review launcher from.
///
/// Groups, paths, and local prompts stay on the server, because they are applied
/// when a session is launched. A project that declares no preset adds no field
/// to the wire, so the client reads the snapshot as absent.
fn review_preset_summaries(cfg: &AppConfig) -> Option<Vec<ReviewPresetSummary>> {
    let summaries: Vec<ReviewPresetSummary> = cfg
        .review
        .presets
        .iter()
        .map(|preset| ReviewPresetSummary {
            name: preset.name.clone(),
            title: preset.title.clone(),
        })
        .collect();
    if summaries.is_empty() {
        None
    } else {
        Some(summaries)
    }
}

pub fn config_state(cfg: &AppConfig, selection: Option<&ModelSelection>) -> ConfigState {
    let sel = selection
        .cloned()
        .unwrap_or_else(|| cfg.default_selection());
    let effective = cfg.resolve_model(&sel);
    let roles = effective_roles_summary(cfg);
    ConfigState {
        workflow: cfg.repl.workflow.project(),
        edit_protocol: EditProtocolKind::Structured,
        model: effective.project(),
        selection: Some(sel.project()),
        roles: Some(roles),
        history_model: Some(cfg.history_model().project()),
        available_workflows: WorkflowFactory::available_workflows()
            .into_iter()
            .map(Project::project)
            .collect(),
        available_edit_protocols: Vec::new(),
        available_models: cfg
            .available_models()
            .into_iter()
            .map(crate::contracts::Project::project)
            .collect(),
        review_presets: review_preset_summaries(cfg),
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

/// Converts a completed application status to the existing RPC shape.
fn system_status_dto(status: tauqe_core::api::SystemStatus) -> SystemStatusResult {
    let cfg = &status.configuration.config;
    let selection = status.configuration.selection;
    SystemStatusResult {
        has_git: status.has_git,
        has_config: status.locations.config.is_some(),
        config_path: status
            .locations
            .config
            .map(|path| path.to_string_lossy().to_string()),
        default_config_path: status
            .locations
            .default_config
            .to_string_lossy()
            .to_string(),
        has_api_key: status.has_api_key,
        credentials_path: status
            .locations
            .credentials
            .map(|path| path.to_string_lossy().to_string()),
        default_credentials_path: status
            .locations
            .default_credentials
            .to_string_lossy()
            .to_string(),
        ready: status.ready,
        model: cfg.resolve_model(&selection).project(),
        selection: Some(selection.project()),
        roles: Some(effective_roles_summary(cfg)),
        history_model: Some(cfg.history_model().project()),
        available_models: cfg
            .available_models()
            .into_iter()
            .map(crate::contracts::Project::project)
            .collect(),
        config_error: cfg.config_error.clone(),
    }
}

pub async fn handle_config_get(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.application.configuration().await;
    let result = config_state(&guard.config, Some(&guard.selection));
    Response::ok_typed(req.id, &result)
}

pub async fn handle_config_set(req: Request, state: &Arc<AppState>) -> Response {
    // The mutation replaces the shared configuration and model selection, so
    // the slot is claimed: a turn registering right after a plain check would
    // observe a configuration swapped underneath its running batch.
    let operation = match super::begin_configuration_change(
        &req.id,
        state,
        "Cannot change workflow or model while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let params: ConfigSetParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid parameters",
            );
        }
    };

    let root = resolved_repo_root(state).await;
    let change = match operation
        .update_project_configuration(
            root.as_deref(),
            tauqe_core::api::ConfigurationPatch {
                selection: params.selection.map(crate::contracts::Project::project),
                model: params.model.map(crate::contracts::Project::project),
                history_model: params.history_model.map(crate::contracts::Project::project),
                workflow: params.workflow.map(Project::project),
            },
        )
        .await
    {
        Ok(change) => change,
        Err(error) => {
            let code = match &error {
                tauqe_core::api::ConfigurationError::UnknownModel { .. }
                | tauqe_core::api::ConfigurationError::UnknownHistoryModel { .. } => {
                    errors::INVALID_MODEL
                }
                tauqe_core::api::ConfigurationError::ModuleRead(_)
                | tauqe_core::api::ConfigurationError::ModuleConflict { .. } => {
                    errors::MODULE_CONFLICT
                }
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    let updated_state = config_state(&change.snapshot.config, Some(&change.snapshot.selection));

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    Response::ok_typed(req.id, &updated_state)
}

pub async fn handle_system_status(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let status = system_status_dto(state.application.system_status(repo_opt).await);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_reload(req: Request, state: &Arc<AppState>) -> Response {
    // The reload replaces the shared configuration for every later turn, so the
    // slot is claimed for the whole reload instead of sampled once.
    let operation = match super::begin_configuration_change(
        &req.id,
        state,
        "Cannot reload configuration while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let snapshot = operation.reload_configuration(repo_opt).await;
    let updated_state = config_state(&snapshot.config, Some(&snapshot.selection));

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let status = system_status_dto(state.application.system_status(repo_opt).await);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_create(req: Request, state: &Arc<AppState>) -> Response {
    // Creating the file is followed by the same full rebind a reload performs,
    // so the slot is claimed for both halves of the operation.
    let operation = match super::begin_configuration_change(
        &req.id,
        state,
        "Cannot create configuration while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let result = match operation.create_configuration(repo_opt).await {
        Ok(result) => result,
        Err(error) => {
            let code = match &error {
                tauqe_core::api::ConfigurationFileError::ModuleConflict(_) => {
                    errors::MODULE_CONFLICT
                }
                _ => errors::CONFIG_CREATE_FAILED,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    let updated_state = config_state(&result.snapshot.config, Some(&result.snapshot.selection));

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let status = system_status_dto(
        state
            .application
            .system_status_after_write(repo_opt, &result)
            .await,
    );

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_save(req: Request, state: &Arc<AppState>) -> Response {
    // Saving credentials reloads the configuration that every later turn reads,
    // so the slot is claimed for the write and the reload together.
    let operation = match super::begin_configuration_change(
        &req.id,
        state,
        "Cannot save credentials while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

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

    let result = match operation.save_credentials(repo_opt, params.api_key).await {
        Ok(result) => result,
        Err(error) => {
            let code = match &error {
                tauqe_core::api::ConfigurationFileError::InvalidKey => errors::INVALID_KEY,
                _ => errors::CREDENTIALS_SAVE_FAILED,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    let updated_state = config_state(&result.snapshot.config, Some(&result.snapshot.selection));

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let status = system_status_dto(
        state
            .application
            .system_status_after_write(repo_opt, &result)
            .await,
    );

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_create_stub(req: Request, state: &Arc<AppState>) -> Response {
    // The stub replaces a file the configuration loader reads, so the slot is
    // claimed for the write instead of being observed once before it.
    let operation = match super::begin_configuration_change(
        &req.id,
        state,
        "Cannot create a credentials stub while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let result = match operation.create_credentials_stub(repo_opt).await {
        Ok(result) => result,
        Err(error) => {
            return Response::err(req.id, errors::CREDENTIALS_STUB_FAILED, error.to_string())
        }
    };

    let status = system_status_dto(
        state
            .application
            .system_status_after_write(repo_opt, &result)
            .await,
    );

    Response::ok_typed(req.id, &status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use tauqe_core::models::ModelRef;
    use tauqe_core::models::{ModelCatalogEntry, ModelRole};
    use tauqe_protocol::WorkflowKind;

    #[tokio::test]
    async fn test_config_state_roles_and_scribe_history() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("anthropic/claude-3.5-haiku"));
        cfg.models.coder = Some(vec![
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()
        ]);
        cfg.models.architect = Some(vec![
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()
        ]);
        cfg.models.repl = vec![ModelRef::openrouter("google/gemini-2.5-flash").into()];
        cfg.models.reviewer = Some(vec![
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()
        ]);

        let st = config_state(&cfg, None);
        assert_eq!(
            st.history_model,
            Some(ModelRef::openrouter("anthropic/claude-3.5-haiku").project())
        );

        let roles = st.roles.expect("roles summary must be present");
        assert_eq!(
            roles.scribe,
            ModelRef::openrouter("anthropic/claude-3.5-haiku").project()
        );
        assert_eq!(
            roles.coder,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").project()
        );
        assert_eq!(
            roles.architect,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").project()
        );
        assert_eq!(
            roles.repl,
            ModelRef::openrouter("google/gemini-2.5-flash").project()
        );
        assert_eq!(
            roles.reviewer,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").project()
        );
    }

    /// The role summary is the single carrier of the provider catalog toward the
    /// client, so a declared alias reaches every surface that prints a model
    /// name and an undeclared catalog adds nothing to the wire.
    #[test]
    fn test_effective_roles_summary_carries_the_provider_catalog() {
        let mut cfg = AppConfig::default();
        cfg.providers.openrouter = Some(tauqe_core::config::OpenRouterConfig {
            api_key: None,
            models: vec![ModelCatalogEntry {
                model: ModelRef::openrouter("deepseek/deepseek-chat"),
                alias: Some("Deep".to_string()),
            }],
        });

        let summary = effective_roles_summary(&cfg);
        let catalog = summary
            .entries
            .and_then(|entries| entries.catalog)
            .expect("a declared catalog must be carried by the summary");
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].model,
            ModelRef::openrouter("deepseek/deepseek-chat").project()
        );
        assert_eq!(catalog[0].alias.as_deref(), Some("Deep"));

        // A project without a declared catalog never ships an empty list.
        let empty = effective_roles_summary(&AppConfig::default());
        assert!(empty
            .entries
            .expect("entries are always present")
            .catalog
            .is_none());
    }

    /// The snapshot carries the compact identity of every declared review
    /// preset, because the client renders its review launcher from this payload
    /// instead of re-reading `tauqe.toml`.
    #[test]
    fn test_config_state_carries_review_preset_summaries() {
        let mut cfg = AppConfig::default();
        cfg.review.presets = vec![
            tauqe_core::config::ReviewPreset {
                name: "backend".to_string(),
                title: Some("Backend audit".to_string()),
                groups: Vec::new(),
                post_analysis: tauqe_core::review::PostAnalysisMode::None,
            },
            tauqe_core::config::ReviewPreset {
                name: "frontend".to_string(),
                title: None,
                groups: Vec::new(),
                post_analysis: tauqe_core::review::PostAnalysisMode::None,
            },
        ];

        let st = config_state(&cfg, None);
        let presets = st
            .review_presets
            .expect("declared presets must reach the snapshot");
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].name, "backend");
        assert_eq!(presets[0].title.as_deref(), Some("Backend audit"));
        assert_eq!(presets[1].name, "frontend");
        assert_eq!(presets[1].title, None);

        // A project that declares no preset ships no snapshot field at all, so
        // the client reads the launcher as absent rather than as an empty list.
        let empty = config_state(&AppConfig::default(), None);
        assert!(empty.review_presets.is_none());
    }

    #[tokio::test]
    async fn test_handle_config_set_model_role_selection() {
        let mut cfg = AppConfig::default();
        cfg.models.scribe = Some(ModelRef::openrouter("model/scribe"));
        cfg.models.coder = Some(vec![ModelRef::openrouter("model/coder").into()]);

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("config-set-model-role");
        let state = AppState::new(cfg, root.clone(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: None,
                    edit_protocol: None,
                    model: None,
                    selection: Some(ModelSelection::Role(ModelRole::Scribe).project()),
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
            Some(ModelSelection::Role(ModelRole::Scribe).project())
        );
        assert_eq!(
            res_state.model,
            ModelRef::openrouter("model/scribe").project()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_handle_config_set_workflow() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("config-set-workflow");
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: Some(WorkflowKind::Naive),
                    model: None,
                    selection: None,
                    edit_protocol: None,
                    history_model: None,
                })
                .unwrap(),
            ),
        };

        let resp = handle_config_set(req, &state).await;
        assert!(resp.error.is_none());

        let res_state: ConfigState = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(res_state.workflow, WorkflowKind::Naive);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_projection_preserves_paths_and_application_readiness() {
        let config = AppConfig::default();
        let selection = config.default_selection();
        let status = system_status_dto(tauqe_core::api::SystemStatus {
            configuration: tauqe_core::api::ConfigurationSnapshot { config, selection },
            locations: tauqe_core::api::ConfigurationLocations {
                config: Some(PathBuf::from("/resolved/tauqe.toml")),
                credentials: Some(PathBuf::from("/resolved/credentials.toml")),
                default_config: PathBuf::from("/default/tauqe.toml"),
                default_credentials: PathBuf::from("/default/credentials.toml"),
            },
            has_git: true,
            has_api_key: true,
            ready: true,
        });
        assert!(status.has_git && status.has_api_key && status.has_config && status.ready);
        assert_eq!(status.config_path.as_deref(), Some("/resolved/tauqe.toml"));
        assert_eq!(
            status.credentials_path.as_deref(),
            Some("/resolved/credentials.toml")
        );
        assert_eq!(status.default_config_path, "/default/tauqe.toml");
        assert_eq!(status.default_credentials_path, "/default/credentials.toml");
    }

    #[tokio::test]
    async fn test_credentials_create_stub_rejected_while_operation_active() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("credentials-stub");
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

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
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `config/set` replaces the shared configuration and model selection, so it
    /// must own the operation slot: while a turn is registered the request is
    /// refused and the selection is left exactly as the turn reads it.
    #[tokio::test]
    async fn test_config_set_is_refused_while_an_operation_is_registered() {
        let mut cfg = AppConfig::default();
        cfg.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("config-set-exclusive");
        let state = AppState::new(cfg, root.clone(), history_sender, out);

        let operation = state
            .try_begin_operation()
            .expect("operation slot must be free");
        let before = state.application.configuration().await.selection.clone();

        let resp = handle_config_set(
            config_set_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
            &state,
        )
        .await;
        let err = resp.error.expect("the switch must be refused");
        assert_eq!(err.code, errors::OPERATION_IN_PROGRESS);

        assert_eq!(
            state.application.configuration().await.selection,
            before,
            "a refused config/set must not change the selection"
        );
        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );

        drop(operation);
        assert!(!state.is_operation_active());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Builds a `config/set` request switching to `selection`.
    fn config_set_request(selection: ModelSelection) -> Request {
        Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: None,
                    edit_protocol: None,
                    model: None,
                    selection: Some(selection.project()),
                    history_model: None,
                })
                .unwrap(),
            ),
        }
    }

    /// Builds a `config/reload` request.
    fn config_reload_request() -> Request {
        Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: "config/reload".to_string(),
            params: None,
        }
    }

    /// A model switch through `config/set` invalidates the provider session,
    /// because a prefix cache belongs to one concrete model.
    #[tokio::test]
    async fn test_handle_config_set_rotates_the_session_on_a_model_change() {
        let mut cfg = AppConfig::default();
        cfg.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("config-rotate-session");
        let state = AppState::new(cfg, root.clone(), history_sender, out);

        let warm = state.session_uuid().await;
        let resp = handle_config_set(
            config_set_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
            &state,
        )
        .await;
        assert!(resp.error.is_none(), "the switch must succeed");

        assert_ne!(
            warm,
            state.session_uuid().await,
            "switching the effective model must rotate the session"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reselecting the model that is already effective keeps the warm session:
    /// the cached prefix is still valid.
    #[tokio::test]
    async fn test_handle_config_set_keeps_the_session_when_the_model_is_unchanged() {
        let mut cfg = AppConfig::default();
        cfg.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let root = scratch_root("config-keep-session");
        let state = AppState::new(cfg, root.clone(), history_sender, out);

        let warm = state.session_uuid().await;
        let resp = handle_config_set(
            config_set_request(ModelSelection::Specific(ModelRef::openrouter(
                "alpha/model",
            ))),
            &state,
        )
        .await;
        assert!(resp.error.is_none(), "the reselection must succeed");

        assert_eq!(
            warm,
            state.session_uuid().await,
            "an unchanged effective model must keep the session"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reloading a configuration that resolves another model invalidates the
    /// session once, and a repeated reload of the same file keeps it warm.
    #[tokio::test]
    async fn test_handle_config_reload_rotates_the_session_on_a_model_change() {
        let root = scratch_root("reload");
        std::fs::write(
            root.join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:deepseek/deepseek-chat\"]\n",
        )
        .unwrap();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let warm = state.session_uuid().await;
        let resp = handle_config_reload(config_reload_request(), &state).await;
        assert!(resp.error.is_none(), "the reload must succeed");

        let status: SystemStatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(
            status.model,
            ModelRef::openrouter("deepseek/deepseek-chat").project(),
            "the reload must adopt the configured model"
        );
        assert_ne!(
            warm,
            state.session_uuid().await,
            "a reloaded effective model must rotate the session"
        );

        let stable = state.session_uuid().await;
        let resp = handle_config_reload(config_reload_request(), &state).await;
        assert!(resp.error.is_none(), "the repeated reload must succeed");
        assert_eq!(
            stable,
            state.session_uuid().await,
            "an unchanged effective model must keep the session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
    /// A key a `tauqe.d` module declares belongs to that module: the in-memory
    /// change would be overwritten on the next load, so the request is refused
    /// with the typed conflict instead of mutating a value nobody can persist.
    #[tokio::test]
    async fn test_config_set_is_refused_for_a_key_a_module_declares() {
        let root = scratch_root("module-conflict");
        std::fs::write(
            root.join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:alpha/model\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("tauqe.d")).unwrap();
        std::fs::write(
            root.join("tauqe.d/10-models.toml"),
            "[models]\nscribe = \"openrouter:scribe/model\"\n",
        )
        .unwrap();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(4),
            method: "config/set".to_string(),
            params: Some(
                serde_json::to_value(ConfigSetParams {
                    workflow: None,
                    edit_protocol: None,
                    model: None,
                    selection: None,
                    history_model: Some(ModelRef::openrouter("scribe/model").project()),
                })
                .unwrap(),
            ),
        };

        let resp = handle_config_set(req, &state).await;
        let err = resp.error.expect("the module-owned key must be refused");
        assert_eq!(err.code, errors::MODULE_CONFLICT);

        let _ = std::fs::remove_dir_all(&root);
    }
}
