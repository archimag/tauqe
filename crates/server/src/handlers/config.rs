use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauqe_core::config::{
    default_config_path, default_credentials_path, find_config_file, find_credentials_file,
    load_config_with_options, AppConfig, ConfigLoadOptions,
};
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    errors, events, ConfigSetParams, ConfigState, CredentialsSaveParams, Event, ModelCatalogEntry,
    ModelRole, ModelRolesEffort, ModelRolesEntries, ModelRolesSummary, ModelSelection, Request,
    Response, ReviewPresetSummary, SystemStatusResult,
};

use crate::state::AppState;

/// Returns the provider catalog as carried by a role summary.
///
/// A model's display alias is declared once, in the provider catalog, so the
/// client receives that declaration instead of deriving a display name from the
/// model string. An empty catalog is reported as absent, so a summary of a
/// project without a declared catalog adds no field to the wire.
fn provider_catalog(cfg: &AppConfig) -> Option<Vec<ModelCatalogEntry>> {
    let catalog = cfg.model_catalog();
    if catalog.is_empty() {
        None
    } else {
        Some(catalog.to_vec())
    }
}

/// Builds the client-facing role summary, enriching each role's effective
/// concrete model with the reasoning effort resolved from its role list and
/// with the provider catalog that declares the models' display aliases.
///
/// Every role-bearing response and event carries this summary so a client can
/// render each semantic role and its reasoning depth without re-reading the
/// configuration, and resolve a display name from the single catalog
/// declaration instead of stating an alias of its own.
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
        catalog: provider_catalog(cfg),
    });
    summary
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
        workflow: cfg.develop.workflow,
        edit_protocol: cfg.develop.protocol,
        model: effective,
        selection: Some(sel),
        roles: Some(roles),
        history_model: Some(cfg.history_model()),
        available_workflows: WorkflowFactory::available_workflows(),
        available_edit_protocols: EditProtocolFactory::available_protocols(),
        available_models: cfg.available_models(),
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

/// Filesystem-derived locations reported by [`SystemStatusResult`].
///
/// Discovery walks candidate directories and checks whether each candidate
/// exists, so the values are resolved once on the blocking pool and then
/// carried as plain in-memory data. A handler that has just written a file
/// replaces the discovered location with the written one, so the report never
/// names a stale path.
struct StatusPaths {
    /// The configuration file the status reports, absent when none was found.
    config: Option<String>,
    /// The credentials file the status reports, absent when none was found.
    credentials: Option<String>,
    /// The path a configuration file would take if it were created now.
    default_config: String,
    /// The path a credentials file would take if it were saved now.
    default_credentials: String,
}

impl StatusPaths {
    /// Runs every location probe against the filesystem and the environment.
    ///
    /// Discovery compares the metadata of every candidate location, and the
    /// default-path resolution reads the process environment, so this belongs
    /// on the blocking pool and never on an async worker thread.
    fn resolve(
        repo_root: Option<&Path>,
        explicit_config: Option<&Path>,
        explicit_credentials: Option<&Path>,
    ) -> Self {
        let config = find_config_file(repo_root, explicit_config);
        let credentials = find_credentials_file(repo_root, explicit_credentials);
        let default_config = default_config_path(repo_root, explicit_config);
        let default_credentials = default_credentials_path(repo_root, explicit_credentials);
        Self {
            config: config.map(|p| p.to_string_lossy().to_string()),
            credentials: credentials.map(|p| p.to_string_lossy().to_string()),
            default_config: default_config.to_string_lossy().to_string(),
            default_credentials: default_credentials.to_string_lossy().to_string(),
        }
    }

    /// The report used when the probes themselves cannot run.
    ///
    /// The blocking task fails only if the probe panicked, so the status still
    /// answers with an empty location report instead of failing the request.
    fn unknown() -> Self {
        Self {
            config: None,
            credentials: None,
            default_config: String::new(),
            default_credentials: String::new(),
        }
    }
}

/// Resolves the reported status locations on the blocking pool.
///
/// The probes behind [`StatusPaths::resolve`] walk candidate directories,
/// compare metadata, and read the process environment. They are offloaded for
/// the same reason [`repo_has_git`] offloads its git subprocess: the async
/// worker keeps serving other requests, and no caller holds the shared
/// configuration guard across directory I/O — the guard is taken only for the
/// in-memory assembly that follows.
async fn resolve_status_paths(
    repo_opt: Option<&Path>,
    explicit_config: Option<PathBuf>,
    explicit_credentials: Option<PathBuf>,
) -> StatusPaths {
    let root = repo_opt.map(Path::to_path_buf);
    super::offload(move || {
        StatusPaths::resolve(
            root.as_deref(),
            explicit_config.as_deref(),
            explicit_credentials.as_deref(),
        )
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(error = %err, "Failed to resolve the status paths");
        StatusPaths::unknown()
    })
}

/// Assembles the authoritative [`SystemStatusResult`] from a single source so
/// derived fields such as `ready` (which requires both a Git repository and an
/// API key) stay consistent across every status-producing handler.
///
/// Every filesystem-derived value arrives pre-resolved in `paths`, so the
/// assembly is pure in-memory work: a caller holds the configuration guard only
/// for these reads instead of across directory probes.
fn build_system_status(
    cfg: &AppConfig,
    selection: &ModelSelection,
    has_git: bool,
    paths: &StatusPaths,
) -> SystemStatusResult {
    let has_api_key = tauqe_core::config::has_openrouter_key(cfg);

    SystemStatusResult {
        has_git,
        has_config: paths.config.is_some(),
        config_path: paths.config.clone(),
        default_config_path: paths.default_config.clone(),
        has_api_key,
        credentials_path: paths.credentials.clone(),
        default_credentials_path: paths.default_credentials.clone(),
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
    // The mutation replaces the shared configuration and model selection, so
    // the slot is claimed: a turn registering right after a plain check would
    // observe a configuration swapped underneath its running batch.
    let _operation = match super::begin_exclusive_operation(
        &req.id,
        state,
        "Cannot change workflow, edit protocol or model while a model operation is in progress",
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

    let (updated_state, previous_model) = {
        let mut guard = state.config.lock().await;
        // The model that was effective before the change decides whether the
        // dialogue session stays warm: a provider's prompt prefix cache belongs
        // to one concrete model, so only a real model change invalidates it.
        let previous_model = guard.config.resolve_model(&guard.selection);

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

        (
            config_state(&guard.config, Some(&guard.selection)),
            previous_model,
        )
    };

    state
        .maybe_rotate_session_on_model_change(&previous_model, &updated_state.model)
        .await;

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
    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
    let paths = resolve_status_paths(repo_opt, explicit_config, explicit_credentials).await;
    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &guard.selection, has_git, &paths);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_reload(req: Request, state: &Arc<AppState>) -> Response {
    // The reload replaces the shared configuration for every later turn, so the
    // slot is claimed for the whole reload instead of sampled once.
    let _operation = match super::begin_exclusive_operation(
        &req.id,
        state,
        "Cannot reload configuration while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let new_cfg = reload_config_with_overrides(state, repo_opt).await;
    let (updated_state, previous_model, cur_sel_val) = {
        let mut guard = state.config.lock().await;
        let previous_model = guard.config.resolve_model(&guard.selection);
        guard.selection = new_cfg.default_selection();
        guard.config = new_cfg;
        let st = config_state(&guard.config, Some(&guard.selection));
        (st, previous_model, guard.selection.clone())
    };

    state
        .maybe_rotate_session_on_model_change(&previous_model, &updated_state.model)
        .await;

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let has_git = repo_has_git(repo_opt).await;
    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
    let paths = resolve_status_paths(repo_opt, explicit_config, explicit_credentials).await;
    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &cur_sel_val, has_git, &paths);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_config_create(req: Request, state: &Arc<AppState>) -> Response {
    // Creating the file is followed by the same full rebind a reload performs,
    // so the slot is claimed for both halves of the operation.
    let _operation = match super::begin_exclusive_operation(
        &req.id,
        state,
        "Cannot create configuration while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
    let target_path = tauqe_core::config::default_config_path(repo_opt, explicit_config.as_deref());
    let write_path = target_path.clone();
    let write_outcome =
        match super::offload(move || tauqe_core::config::write_default_config(&write_path)).await {
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
    let (updated_state, previous_model, cur_sel_val) = {
        let mut guard = state.config.lock().await;
        let previous_model = guard.config.resolve_model(&guard.selection);
        guard.selection = new_cfg.default_selection();
        guard.config = new_cfg;
        let st = config_state(&guard.config, Some(&guard.selection));
        (st, previous_model, guard.selection.clone())
    };

    state
        .maybe_rotate_session_on_model_change(&previous_model, &updated_state.model)
        .await;

    state.out.send_event(&Event {
        method: events::CONFIG_CHANGED.to_string(),
        params: serde_json::to_value(&updated_state).ok(),
    });

    let has_git = repo_has_git(repo_opt).await;
    let mut paths = resolve_status_paths(repo_opt, explicit_config, explicit_credentials).await;
    paths.config = Some(target_path.to_string_lossy().to_string());
    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &cur_sel_val, has_git, &paths);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_save(req: Request, state: &Arc<AppState>) -> Response {
    // Saving credentials reloads the configuration that every later turn reads,
    // so the slot is claimed for the write and the reload together.
    let _operation = match super::begin_exclusive_operation(
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

    let api_key = params.api_key.trim();
    if api_key.is_empty() {
        return Response::err(req.id, errors::INVALID_KEY, "API key cannot be empty");
    }

    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
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
    let mut paths = resolve_status_paths(repo_opt, explicit_config, explicit_credentials).await;
    paths.credentials = Some(target_path.to_string_lossy().to_string());
    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &cur_sel_val, has_git, &paths);

    Response::ok_typed(req.id, &status)
}

pub async fn handle_credentials_create_stub(req: Request, state: &Arc<AppState>) -> Response {
    // The stub replaces a file the configuration loader reads, so the slot is
    // claimed for the write instead of being observed once before it.
    let _operation = match super::begin_exclusive_operation(
        &req.id,
        state,
        "Cannot create a credentials stub while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let repo_root = resolved_repo_root(state).await;
    let repo_opt = repo_root.as_deref();

    let (explicit_config, explicit_credentials) = explicit_cli_paths(state).await;
    let target_path =
        tauqe_core::config::default_credentials_path(repo_opt, explicit_credentials.as_deref());
    let write_path = target_path.clone();
    let write_outcome =
        match super::offload(move || tauqe_core::config::write_credentials_stub(&write_path)).await
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
    let mut paths = resolve_status_paths(repo_opt, explicit_config, explicit_credentials).await;
    paths.credentials = Some(target_path.to_string_lossy().to_string());
    let guard = state.config.lock().await;
    let status = build_system_status(&guard.config, &guard.selection, has_git, &paths);

    Response::ok_typed(req.id, &status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{EditProtocolKind, ModelRef, WorkflowKind};

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
        cfg.models.primary = vec![ModelRef::openrouter("google/gemini-2.5-flash").into()];
        cfg.models.reviewer = Some(vec![
            ModelRef::openrouter("anthropic/claude-3.7-sonnet").into()
        ]);

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
            ModelRef::openrouter("deepseek/deepseek-chat")
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
                post_analysis: tauqe_protocol::review::PostAnalysisMode::None,
            },
            tauqe_core::config::ReviewPreset {
                name: "frontend".to_string(),
                title: None,
                groups: Vec::new(),
                post_analysis: tauqe_protocol::review::PostAnalysisMode::None,
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
        let state = AppState::new(
            AppConfig::default(),
            std::env::temp_dir(),
            history_sender,
            out,
        );

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
    /// The status locations are resolved off the async worker, so the
    /// repository root and the explicit CLI overrides must reach both the
    /// discovery probe and the default-path computation.
    #[tokio::test]
    async fn test_resolve_status_paths_reports_discovered_and_default_locations() {
        let root = unique_repo_dir("status-paths");
        let repo_config = root.join("tauqe.toml");
        // Discovery only checks that a candidate is a file, so the fixture
        // carries no configuration.
        std::fs::write(&repo_config, "").unwrap();

        let paths = resolve_status_paths(Some(&root), None, None).await;
        let expected = repo_config.to_string_lossy().to_string();
        assert_eq!(paths.config, Some(expected.clone()));
        assert_eq!(
            paths.default_config, expected,
            "the default location matches the file discovered in the repository"
        );

        // An explicit override wins over the repository candidates for both
        // the discovered file and the location a new file would take.
        let explicit = root.join("custom.toml");
        std::fs::write(&explicit, "").unwrap();
        let paths = resolve_status_paths(Some(&root), Some(explicit.clone()), None).await;
        let expected = explicit.to_string_lossy().to_string();
        assert_eq!(paths.config, Some(expected.clone()));
        assert_eq!(paths.default_config, expected);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `build_system_status` consumes the pre-resolved locations, so a caller
    /// holds the configuration guard only for in-memory reads while a freshly
    /// written file still replaces the discovered one.
    #[test]
    fn test_build_system_status_consumes_the_resolved_locations() {
        let cfg = AppConfig::default();
        let selection = cfg.default_selection();
        let paths = StatusPaths {
            config: Some("/resolved/tauqe.toml".to_string()),
            credentials: None,
            default_config: "/default/tauqe.toml".to_string(),
            default_credentials: "/default/creds.toml".to_string(),
        };

        let status = build_system_status(&cfg, &selection, true, &paths);
        assert_eq!(status.config_path, paths.config);
        assert!(status.has_config);
        assert!(status.credentials_path.is_none());
        assert_eq!(status.default_config_path, "/default/tauqe.toml");
        assert_eq!(status.default_credentials_path, "/default/creds.toml");
        assert!(status.has_git);

        // A create/save handler replaces the discovered location with the file
        // it has just written before the status is assembled.
        let written = StatusPaths {
            config: Some("/fresh/tauqe.toml".to_string()),
            credentials: Some("/fresh/creds.toml".to_string()),
            ..paths
        };
        let status = build_system_status(&cfg, &selection, false, &written);
        assert_eq!(status.config_path, written.config);
        assert_eq!(status.credentials_path, written.credentials);
        assert_eq!(status.default_config_path, "/default/tauqe.toml");
        assert!(!status.has_git);
    }
    #[tokio::test]
    async fn test_credentials_create_stub_rejected_while_operation_active() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            AppConfig::default(),
            std::env::temp_dir(),
            history_sender,
            out,
        );

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

    /// `config/set` replaces the shared configuration and model selection, so it
    /// must own the operation slot: while a turn is registered the request is
    /// refused and the selection is left exactly as the turn reads it.
    #[tokio::test]
    async fn test_config_set_is_refused_while_an_operation_is_registered() {
        let mut cfg = AppConfig::default();
        cfg.models.primary = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

        let operation = state
            .try_begin_operation()
            .expect("operation slot must be free");
        let before = state.config.lock().await.selection.clone();

        let resp = handle_config_set(
            config_set_request(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
            &state,
        )
        .await;
        let err = resp.error.expect("the switch must be refused");
        assert_eq!(err.code, errors::OPERATION_IN_PROGRESS);

        assert_eq!(
            state.config.lock().await.selection,
            before,
            "a refused config/set must not change the selection"
        );
        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );

        drop(operation);
        assert!(!state.is_operation_active());
    }

    /// Creates an isolated temporary repository directory for tests that need a
    /// `tauqe.toml` on disk.
    fn unique_repo_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "tauqe-config-{}-{}-{}",
            tag,
            std::process::id(),
            index
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
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
                    selection: Some(selection),
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
        cfg.models.primary = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

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
    }

    /// Reselecting the model that is already effective keeps the warm session:
    /// the cached prefix is still valid.
    #[tokio::test]
    async fn test_handle_config_set_keeps_the_session_when_the_model_is_unchanged() {
        let mut cfg = AppConfig::default();
        cfg.models.primary = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];

        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _hrx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(cfg, std::env::temp_dir(), history_sender, out);

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
    }

    /// Reloading a configuration that resolves another model invalidates the
    /// session once, and a repeated reload of the same file keeps it warm.
    #[tokio::test]
    async fn test_handle_config_reload_rotates_the_session_on_a_model_change() {
        let root = unique_repo_dir("reload");
        std::fs::write(
            root.join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:deepseek/deepseek-chat\"]\n",
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
            ModelRef::openrouter("deepseek/deepseek-chat"),
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
}
