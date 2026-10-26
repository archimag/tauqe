use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauqe_protocol::{
    ModelCatalogEntry, ModelRef, ModelRole, ModelRolesSummary, ModelSelection, ProjectConfig,
    PromptLayoutKind, ReasoningEffort, TurnIntent,
};

mod context;
mod credentials;
mod develop;
mod git;
mod history;
mod models;
mod paths;
mod providers;

pub use context::{
    ContextConfig, DEFAULT_FILES_TOKEN_BUDGET, DEFAULT_INCLUDE_REPO_MAP,
    DEFAULT_MAX_DIR_LIST_ENTRIES, DEFAULT_MAX_DISCOVERY_ROUNDS, DEFAULT_MAX_FEEDBACK_CHARS,
    DEFAULT_MAX_MATCHED_FILES_PER_SEARCH, DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND,
    DEFAULT_MAX_SCRATCHPAD_CHARS, DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND,
};
pub use credentials::default_credentials_path;
pub use develop::{DevelopConfig, EditConfig};
pub use git::GitConfig;
pub use history::HistoryConfig;
pub use models::{
    serialize_model_ref_opt, ModelsConfig, BUILTIN_DEFAULT_ARCHITECT_MODEL,
    BUILTIN_DEFAULT_CODER_MODEL, BUILTIN_DEFAULT_MODEL, BUILTIN_DEFAULT_PRIMARY_MODEL,
    BUILTIN_DEFAULT_REVIEWER_MODEL, BUILTIN_DEFAULT_SCRIBE_MODEL,
};

use models::ModelsConfigRaw;
pub use paths::{
    cli_arg_value_from, config_candidates, default_config_path, find_config_file,
    is_repo_config_path, write_default_config, write_default_conventions, DEFAULT_CONFIG_TEMPLATE,
};
pub use providers::{
    credentials_candidates, find_credentials_file, has_openrouter_key, load_credentials,
    read_credentials_file, write_credentials_file, write_credentials_stub, Credentials,
    OpenRouterConfig, OpenRouterCredentials, ProvidersConfig,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(try_from = "AppConfigRaw")]
pub struct AppConfig {
    pub providers: ProvidersConfig,
    pub models: ModelsConfig,
    pub develop: DevelopConfig,
    /// The `[project]` section: the commands this repository declares.
    ///
    /// It is the single surface that tells the harness how to verify, format,
    /// and post-process the project, and which named commands the model may call
    /// as typed custom tools. An empty section declares nothing and is rejected
    /// by [`AppConfig::validate`], because a turn that cannot verify its own
    /// result must not run.
    pub project: ProjectConfig,
    pub context: ContextConfig,
    pub git: GitConfig,
    pub history: HistoryConfig,
    /// Explicit configuration file path supplied on the command line.
    ///
    /// Recorded so a running session re-applies the same override whenever it
    /// reloads configuration. The library never reads process arguments; the
    /// server binary parses them and passes them through the config load
    /// options. Never serialized into `tauqe.toml`.
    #[serde(skip)]
    pub explicit_config_path: Option<PathBuf>,
    /// Explicit credentials file path supplied on the command line.
    ///
    /// Recorded alongside `explicit_config_path` so credential discovery keeps
    /// honoring the `--credentials` override across reloads. Never serialized
    /// into `tauqe.toml`.
    #[serde(skip)]
    pub explicit_credentials_path: Option<PathBuf>,
    /// Diagnostic captured when a discovered or explicit configuration file
    /// existed but could not be read or parsed.
    ///
    /// The loader falls back to built-in defaults rather than aborting, but
    /// records the reason here so the UI can warn the user instead of silently
    /// switching to other models and budgets. Never serialized into
    /// `tauqe.toml`.
    #[serde(skip)]
    pub config_error: Option<String>,
}

/// Raw configuration shape decoded before the provider catalog is applied.
///
/// A role list may name a model by a catalog alias, and an alias is only
/// resolvable against a catalog that has already been read. The configuration
/// is therefore decoded into this proxy first and then resolved through
/// [`AppConfig::try_from`], so no role can resolve against a half-read catalog.
#[derive(Deserialize)]
struct AppConfigRaw {
    #[serde(default)]
    providers: ProvidersConfig,
    #[serde(default)]
    models: ModelsConfigRaw,
    #[serde(default, alias = "edit")]
    develop: DevelopConfig,
    #[serde(default)]
    project: ProjectConfig,
    #[serde(default)]
    context: ContextConfig,
    #[serde(default)]
    git: GitConfig,
    #[serde(default)]
    history: HistoryConfig,
}

impl TryFrom<AppConfigRaw> for AppConfig {
    type Error = String;

    fn try_from(raw: AppConfigRaw) -> Result<Self, Self::Error> {
        let AppConfigRaw {
            providers,
            models,
            develop,
            project,
            context,
            git,
            history,
        } = raw;

        Ok(AppConfig {
            models: ModelsConfig::resolve(models, providers.catalog())?,
            providers,
            develop,
            project,
            context,
            git,
            history,
            explicit_config_path: None,
            explicit_credentials_path: None,
            config_error: None,
        })
    }
}

impl AppConfig {
    /// Models registered in the provider catalog, in declaration order.
    pub fn model_catalog(&self) -> &[ModelCatalogEntry] {
        self.providers.catalog()
    }

    /// Resolves a model reference written in configuration.
    ///
    /// The reference is either a registered catalog alias or a provider model
    /// specification, so a role can name a model the way the project does
    /// without restating a display name.
    pub fn resolve_model_spec(&self, spec: &str) -> Result<ModelRef, String> {
        ModelsConfig::resolve_model_spec(self.model_catalog(), spec)
    }

    /// Returns the catalog alias registered for a model, if any.
    pub fn model_alias(&self, model: &ModelRef) -> Option<&str> {
        ModelsConfig::model_alias(self.model_catalog(), model)
    }

    /// Models registered in the provider catalog, de-duplicated.
    pub fn configured_models(&self) -> Vec<ModelRef> {
        let mut models = Vec::new();
        for entry in self.model_catalog() {
            if !models.contains(&entry.model) {
                models.push(entry.model.clone());
            }
        }
        models
    }

    /// Resolves a [`ModelRole`] to a concrete [`ModelRef`] using the
    /// deterministic fallback chain.
    pub fn resolve_role(&self, role: ModelRole) -> ModelRef {
        let fallback = self.configured_models().into_iter().next();
        self.models.resolve_role_with_fallback(role, fallback)
    }

    /// Resolves a [`ModelSelection`] to a concrete [`ModelRef`].
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
            ModelSelection::Tier { model, .. } => model.clone(),
        }
    }

    /// Resolves the reasoning effort for a model selection within the context of
    /// a specific role.
    pub fn resolve_selection_effort(
        &self,
        role: ModelRole,
        selection: &ModelSelection,
    ) -> Option<ReasoningEffort> {
        self.models.resolve_selection_effort(role, selection)
    }

    /// Summary of effective concrete models assigned to all semantic roles.
    pub fn roles_summary(&self) -> ModelRolesSummary {
        let fallback = self.configured_models().into_iter().next();
        self.models.roles_summary_with_fallback(fallback)
    }

    /// The default active model based on `Primary` role.
    pub fn active_model(&self) -> ModelRef {
        self.resolve_role(ModelRole::Primary)
    }

    /// The default model selection based on `Primary` role.
    pub fn default_selection(&self) -> ModelSelection {
        ModelSelection::Role(ModelRole::Primary)
    }

    /// The model used for history summarization and squash commit message generation.
    pub fn history_model(&self) -> ModelRef {
        self.resolve_role(ModelRole::Scribe)
    }

    /// Models the user may switch between.
    ///
    /// Includes every configured provider model and every entry of every role
    /// list, so a model that appears only as a non-primary rung of a role ladder
    /// stays selectable.
    pub fn available_models(&self) -> Vec<ModelRef> {
        let mut models = self.configured_models();
        for role in [
            ModelRole::Scribe,
            ModelRole::Coder,
            ModelRole::Architect,
            ModelRole::Primary,
            ModelRole::Reviewer,
        ] {
            for entry in self.models.role_entries(role) {
                if !models.contains(&entry.model) {
                    models.push(entry.model);
                }
            }
        }
        models
    }

    /// Validates application configuration invariants.
    ///
    /// Two layers are checked before a configuration may drive a turn: the
    /// semantic model roles must resolve, and the `[project]` declaration must
    /// name a verification command whose reference chain is sound. An invalid
    /// configuration is rejected as a whole instead of degrading silently,
    /// because a turn that cannot verify its own result must not run.
    pub fn validate(&self) -> Result<(), String> {
        self.models.validate()?;
        self.project
            .validate()
            .map_err(|err| format!("[project] {}", err))?;
        Ok(())
    }

    /// Resolves the prompt layout policy for a bounded turn.
    ///
    /// This is the single typed point that decides the layout: an explicitly
    /// configured [`PromptLayoutKind`] wins, otherwise the turn intent decides.
    ///
    /// - An interactive REPL turn and a plan discussion keep a growing history
    ///   and share one byte-stable head, so both build the chronological
    ///   transcript: its append-only growth keeps the provider prefix cache warm
    ///   across dialogue focus switches.
    /// - An isolated step carries no growing history to interleave, so it keeps
    ///   the semantic block order.
    pub fn resolve_prompt_layout(&self, intent: TurnIntent) -> PromptLayoutKind {
        if let Some(explicit) = self.develop.prompt_layout {
            return explicit;
        }

        match intent {
            TurnIntent::InteractiveRepl | TurnIntent::PlanDiscussion => {
                PromptLayoutKind::Chronological
            }
            TurnIntent::IsolatedStep => PromptLayoutKind::Semantic,
        }
    }
}

/// Options for configuring how configuration and credentials files are resolved.
#[derive(Debug, Clone, Default)]
pub struct ConfigLoadOptions<'a> {
    pub repo_root: Option<&'a Path>,
    pub explicit_config: Option<&'a Path>,
    pub explicit_credentials: Option<&'a Path>,
}

pub fn load_config_with_options(opts: ConfigLoadOptions) -> AppConfig {
    // Discovery and reading of configuration files, plus credential
    // resolution, are blocking filesystem work. Route them off the async
    // executor thread.
    crate::blocking::run_blocking(|| {
        let config_path = opts
            .explicit_config
            .map(PathBuf::from)
            .or_else(|| find_config_file(opts.repo_root, opts.explicit_config));

        let mut config = AppConfig::default();
        if let Some(p) = config_path.as_deref().filter(|p| !p.as_os_str().is_empty()) {
            match std::fs::read_to_string(p) {
                Ok(content) => match toml::from_str::<AppConfig>(&content) {
                    Ok(parsed) => {
                        config = parsed;
                        if let Err(err) = config.validate() {
                            tracing::warn!(
                                "Configuration validation failed for {}: {}",
                                p.display(),
                                err
                            );
                            config.config_error =
                                Some(format!("Invalid configuration in {}: {}", p.display(), err));
                        }
                    }
                    Err(err) => {
                        tracing::warn!("Failed to parse config file {}: {}", p.display(), err);
                        config.config_error =
                            Some(format!("Failed to parse {}: {}", p.display(), err));
                    }
                },
                Err(err) => {
                    tracing::warn!("Failed to read config file {}: {}", p.display(), err);
                    config.config_error = Some(format!("Failed to read {}: {}", p.display(), err));
                }
            }
        }

        credentials::apply_credentials(&mut config, opts.explicit_credentials, opts.repo_root);

        config.explicit_config_path = opts
            .explicit_config
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from);
        config.explicit_credentials_path = opts
            .explicit_credentials
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from);

        config
    })
}

/// Loads configuration for `repo_root` without any command-line overrides.
///
/// Callers that must honor explicit `--config` / `--credentials` paths build a
/// [`ConfigLoadOptions`] themselves and call [`load_config_with_options`].
pub fn load_config(repo_root: Option<&Path>) -> AppConfig {
    load_config_with_options(ConfigLoadOptions {
        repo_root,
        explicit_config: None,
        explicit_credentials: None,
    })
}

/// Loads a specific configuration section from `tauqe.toml` without parsing the
/// whole configuration file or loading credentials.
///
/// Returns the default value if the file does not exist, cannot be read, or the
/// section cannot be parsed.
pub fn load_section<T: for<'de> Deserialize<'de> + Default>(
    repo_root: &Path,
    section_key: &str,
) -> T {
    // Locating and reading the configuration file is blocking filesystem work,
    // so it runs off the async executor thread.
    crate::blocking::run_blocking(|| {
        let Some(path) = find_config_file(Some(repo_root), None) else {
            return T::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return T::default();
        };
        let Ok(table) = toml::from_str::<toml::Table>(&content) else {
            return T::default();
        };
        table
            .get(section_key)
            .cloned()
            .and_then(|val| val.try_into::<T>().ok())
            .unwrap_or_default()
    })
}

/// Resolves repository-local `[git]` integration settings for a repository root.
///
/// Only the `[git]` table is parsed without paying for credential resolution or
/// unrelated configuration sections.
pub fn load_git_config(repo_root: &Path) -> GitConfig {
    load_section(repo_root, "git")
}

/// Resolves repository-local `[context]` settings for a repository root.
///
/// Mirrors [`load_git_config`]: only the `[context]` table is parsed.
pub fn load_context_config(repo_root: &Path) -> ContextConfig {
    load_section(repo_root, "context")
}

/// Validates the project configuration file that governs `repo_root`.
///
/// The check is delegated to the engine rather than to a project tool, because
/// `tauqe.toml` defines the environment the turn runs in: the model roles, the
/// budgets, and the command lines the harness may execute. Two layers are
/// checked — the TOML syntax of the file and the semantic invariants enforced by
/// [`AppConfig::validate`] (resolvable model roles and a sound `[project]`
/// command chain).
///
/// A repository without a configuration file is valid: built-in defaults govern
/// it, exactly as they do for [`load_config`]. Reading and parsing the file is
/// blocking filesystem work, so it runs off the async executor thread.
pub fn validate_project_config_on_disk(repo_root: &Path) -> Result<(), String> {
    crate::blocking::run_blocking(|| -> Result<(), String> {
        let Some(path) = find_config_file(Some(repo_root), None) else {
            return Ok(());
        };
        let content = std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read {}: {}", path.display(), err))?;
        let config: AppConfig = toml::from_str(&content)
            .map_err(|err| format!("Failed to parse {}: {}", path.display(), err))?;
        config
            .validate()
            .map_err(|err| format!("Invalid configuration in {}: {}", path.display(), err))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{EditProtocolKind, PromptLayoutKind, TurnIntent, WorkflowKind};

    #[test]
    fn test_models_config_and_roles_resolution() {
        let toml_str = r#"
[models]
primary = ["openrouter:google/gemini-flash-latest"]
scribe = "openrouter:anthropic/claude-haiku-latest"
coder = ["openrouter:google/gemini-flash-latest"]
architect = ["openrouter:anthropic/claude-sonnet-latest"]
reviewer = ["openrouter:anthropic/claude-sonnet-latest"]
auto_level_up = true
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(config.models.validate().is_ok());
        assert!(config.models.auto_level_up);
        assert_eq!(
            config.active_model(),
            ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert_eq!(
            config.default_selection(),
            ModelSelection::Role(ModelRole::Primary)
        );
        assert_eq!(
            config.history_model(),
            ModelRef::openrouter("anthropic/claude-haiku-latest")
        );

        let roles = config.roles_summary();
        assert_eq!(
            roles.scribe,
            ModelRef::openrouter("anthropic/claude-haiku-latest")
        );
        assert_eq!(
            roles.coder,
            ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert_eq!(
            roles.architect,
            ModelRef::openrouter("anthropic/claude-sonnet-latest")
        );
        assert_eq!(
            roles.primary,
            ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert_eq!(
            roles.reviewer,
            ModelRef::openrouter("anthropic/claude-sonnet-latest")
        );

        let models_summary = config.models.roles_summary();
        assert_eq!(models_summary.scribe, roles.scribe);
        assert_eq!(models_summary.coder, roles.coder);
        assert_eq!(models_summary.architect, roles.architect);
        assert_eq!(models_summary.primary, roles.primary);
        assert_eq!(models_summary.reviewer, roles.reviewer);

        let specific = ModelRef::openrouter("custom/model");
        assert_eq!(
            config.resolve_model(&ModelSelection::Specific(specific.clone())),
            specific
        );
        assert_eq!(
            config.resolve_model(&ModelSelection::Role(ModelRole::Scribe)),
            roles.scribe
        );
        assert_eq!(
            config.resolve_model(&ModelSelection::Role(ModelRole::Coder)),
            roles.coder
        );
        assert_eq!(
            config.resolve_model(&ModelSelection::Role(ModelRole::Architect)),
            roles.architect
        );
        assert_eq!(
            config.resolve_model(&ModelSelection::Role(ModelRole::Primary)),
            roles.primary
        );
        assert_eq!(
            config.resolve_model(&ModelSelection::Role(ModelRole::Reviewer)),
            roles.reviewer
        );
    }

    #[test]
    fn test_app_config_models_effort_resolution() {
        let toml_str = r#"
[models]
primary = ["openrouter:google/gemini-flash-latest"]
coder = [
    { model = "openrouter:deepseek/deepseek-chat", effort = "medium" },
    { model = "openrouter:google/gemini-flash-latest", effort = "high" },
]
architect = [{ model = "openrouter:anthropic/claude-sonnet-latest", effort = "high" }]
reviewer = ["openrouter:google/gemini-flash-latest"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("parse config");
        assert_eq!(
            config.models.resolve_effort(ModelRole::Architect),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            config.models.resolve_effort(ModelRole::Coder),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(config.models.resolve_effort(ModelRole::Reviewer), None);
        assert_eq!(config.models.resolve_effort(ModelRole::Primary), None);
    }

    #[test]
    fn test_models_config_validation() {
        let empty_cfg = ModelsConfig::default();
        assert!(empty_cfg.validate().is_err());

        let primary_only = ModelsConfig {
            primary: vec![ModelRef::openrouter("qwen/coder").into()],
            ..Default::default()
        };
        assert!(primary_only.validate().is_ok());

        let empty_coder = ModelsConfig {
            primary: vec![ModelRef::openrouter("qwen/coder").into()],
            coder: Some(Vec::new()),
            ..Default::default()
        };
        assert!(empty_coder.validate().is_err());
    }

    #[test]
    fn test_single_model_fallback_closes_all_roles() {
        let toml_str = r#"
[models]
primary = ["openrouter:anthropic/claude-3.7-sonnet"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(config.models.validate().is_ok());
        let expected = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::Primary), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_specialized_roles_inherit_primary_list() {
        let config: AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:alpha/model", "openrouter:beta/model"]
"#,
        )
        .expect("parse config");

        // A specialized role without its own list inherits the primary list.
        let expected = ModelRef::openrouter("alpha/model");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::Primary), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);

        // The coder escalation ladder inherits the full primary list.
        let ladder = config.models.coder_ladder();
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].model, ModelRef::openrouter("alpha/model"));
        assert_eq!(ladder[1].model, ModelRef::openrouter("beta/model"));
    }

    #[test]
    fn test_role_resolution_external_provider_fallback() {
        let toml_str = r#"
[providers.openrouter]
models = ["custom/external-fallback"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let expected = ModelRef::openrouter("custom/external-fallback");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::Primary), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_available_models_include_all_roles() {
        let toml_str = r#"
[models]
primary = ["openrouter:d/primary"]
scribe = "openrouter:a/scribe"
coder = ["openrouter:b/coder"]
architect = ["openrouter:c/architect"]
reviewer = ["openrouter:r/reviewer"]

[providers.openrouter]
models = ["e/extra", "a/scribe"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("e/extra")));
        assert!(available.contains(&ModelRef::openrouter("a/scribe")));
        assert!(available.contains(&ModelRef::openrouter("b/coder")));
        assert!(available.contains(&ModelRef::openrouter("c/architect")));
        assert!(available.contains(&ModelRef::openrouter("d/primary")));
        assert!(available.contains(&ModelRef::openrouter("r/reviewer")));
    }

    #[test]
    fn test_context_pinned_config_list() {
        let toml_str = r#"
[context]
pinned = ["docs/Core.md", "README.md"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(
            config.context.pinned,
            vec!["docs/Core.md".to_string(), "README.md".to_string()]
        );
    }

    #[test]
    fn test_context_pinned_config_table() {
        let toml_str = r#"
[context.pinned]
files = ["docs/TUI.md"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.pinned, vec!["docs/TUI.md".to_string()]);
    }

    #[test]
    fn test_history_config_defaults_and_override() {
        let default_cfg = HistoryConfig::default();
        assert_eq!(default_cfg.budget_tokens, 20_000);
        assert_eq!(default_cfg.tail_turns, 10);

        let toml_str = r#"
[history]
budget_tokens = 45000
tail_turns = 7
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.history.budget_tokens, 45_000);
        assert_eq!(config.history.tail_turns, 7);
    }

    #[test]
    fn test_repomap_budget_config() {
        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.repomap_token_budget, 4000);

        let toml_str = r#"
[context]
repomap_token_budget = 6000
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.repomap_token_budget, 6000);
    }

    #[test]
    fn test_include_repo_map_defaults_and_override() {
        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.include_repo_map, DEFAULT_INCLUDE_REPO_MAP);
        assert!(default_cfg.include_repo_map);

        let absent: AppConfig =
            toml::from_str("[context]\nrepomap_token_budget = 1000\n").expect("parse config");
        assert!(
            absent.context.include_repo_map,
            "an omitted flag must keep the repository map enabled"
        );

        let disabled: AppConfig =
            toml::from_str("[context]\ninclude_repo_map = false\n").expect("parse config");
        assert!(!disabled.context.include_repo_map);

        let enabled: AppConfig =
            toml::from_str("[context]\ninclude_repo_map = true\n").expect("parse config");
        assert!(enabled.context.include_repo_map);
    }

    #[test]
    fn test_context_discovery_config() {
        let toml_str = r#"
[context]
max_discovery_rounds = 8
files_token_budget = 150000
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.max_discovery_rounds, 8);
        assert_eq!(config.context.files_token_budget, 150000);

        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.max_discovery_rounds, 5);
        assert_eq!(default_cfg.files_token_budget, 300_000);
    }

    #[test]
    fn test_discovery_and_feedback_limits_defaults_and_overrides() {
        // Every operational cap that shapes model context carries a documented
        // default, so an omitted key never silently disables the bound.
        let defaults = ContextConfig::default();
        assert_eq!(
            defaults.max_search_requests_per_round,
            DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND
        );
        assert_eq!(
            defaults.max_matched_files_per_search,
            DEFAULT_MAX_MATCHED_FILES_PER_SEARCH
        );
        assert_eq!(defaults.max_dir_list_entries, DEFAULT_MAX_DIR_LIST_ENTRIES);
        assert_eq!(
            defaults.max_repo_map_requests_per_round,
            DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND
        );
        assert_eq!(defaults.max_feedback_chars, DEFAULT_MAX_FEEDBACK_CHARS);
        assert_eq!(defaults.max_scratchpad_chars, DEFAULT_MAX_SCRATCHPAD_CHARS);
        assert_eq!(defaults.max_search_requests_per_round, 5);
        assert_eq!(defaults.max_matched_files_per_search, 50);
        assert_eq!(defaults.max_dir_list_entries, 100);
        assert_eq!(defaults.max_repo_map_requests_per_round, 3);
        assert_eq!(defaults.max_feedback_chars, 4_000);
        assert_eq!(defaults.max_scratchpad_chars, 4_000);

        // A project that routinely needs wider searches, larger listings, or
        // roomier tool-output reporting raises the ceilings in `tauqe.toml`.
        let overridden: AppConfig = toml::from_str(
            r#"
[context]
max_search_requests_per_round = 9
max_matched_files_per_search = 12
max_dir_list_entries = 40
max_repo_map_requests_per_round = 1
max_feedback_chars = 800
max_scratchpad_chars = 1500
"#,
        )
        .expect("parse config");
        assert_eq!(overridden.context.max_search_requests_per_round, 9);
        assert_eq!(overridden.context.max_matched_files_per_search, 12);
        assert_eq!(overridden.context.max_dir_list_entries, 40);
        assert_eq!(overridden.context.max_repo_map_requests_per_round, 1);
        assert_eq!(overridden.context.max_feedback_chars, 800);
        assert_eq!(overridden.context.max_scratchpad_chars, 1500);

        // A partially declared section keeps the defaults of omitted keys.
        let partial: AppConfig =
            toml::from_str("[context]\nmax_dir_list_entries = 7\n").expect("parse config");
        assert_eq!(partial.context.max_dir_list_entries, 7);
        assert_eq!(
            partial.context.max_search_requests_per_round,
            DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND
        );
    }

    #[test]
    fn test_typed_provider_models_config() {
        let toml_str = r#"
[models]
primary = ["openrouter:openai/gpt-4o"]

[providers.openrouter]
models = ["deepseek/deepseek-chat", "openai/gpt-4o"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.active_model(), ModelRef::openrouter("openai/gpt-4o"));
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("deepseek/deepseek-chat")));
        assert!(available.contains(&ModelRef::openrouter("openai/gpt-4o")));

        // A catalog entry must name a real model: a blank declaration is a
        // project error, not an entry to silently skip.
        let blank = toml_str.replace("\"deepseek/deepseek-chat\", ", "\" \", ");
        assert!(toml::from_str::<AppConfig>(&blank).is_err());
    }

    #[test]
    fn test_catalog_alias_resolves_roles_and_display_name() {
        let toml_str = r#"
[providers.openrouter]
models = [
    "google/gemini-flash-latest",
    { model = "deepseek/deepseek-chat", alias = "Deep" },
]

[models]
primary = ["Deep"]
scribe = "Deep"
coder = [{ model = "Deep", effort = "high" }]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let deep = ModelRef::openrouter("deepseek/deepseek-chat");

        // A role names a model by its catalog alias instead of restating the
        // provider identifier, and the catalog resolves it deterministically.
        assert_eq!(config.active_model(), deep);
        assert_eq!(config.history_model(), deep);
        let coder = config.models.coder_ladder();
        assert_eq!(coder[0].model, deep);
        assert_eq!(coder[0].effort, Some(ReasoningEffort::High));

        // The display name lives only in the catalog and is looked up on demand.
        assert_eq!(config.model_alias(&deep), Some("Deep"));
        assert_eq!(
            config.model_alias(&ModelRef::openrouter("google/gemini-flash-latest")),
            None
        );
        assert_eq!(config.resolve_model_spec("Deep").unwrap(), deep);
        assert_eq!(
            config
                .resolve_model_spec("openrouter:google/gemini-flash-latest")
                .unwrap(),
            ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert!(config.resolve_model_spec("anthropc:claude").is_err());
    }

    #[test]
    fn test_unresolvable_role_reference_surfaces_diagnostic() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            r#"
[providers.openrouter]
models = ["deepseek/deepseek-chat"]

[models]
primary = ["anthropc:claude"]
"#,
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        let err = config
            .config_error
            .expect("an unresolvable role reference must be reported");
        assert!(err.contains("anthropc:claude"), "got: {err}");
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(
            config.active_model(),
            ModelRef::openrouter(BUILTIN_DEFAULT_PRIMARY_MODEL)
        );
        assert_eq!(config.develop.workflow, WorkflowKind::Git);
        assert_eq!(config.develop.protocol, EditProtocolKind::Structured);
        assert_eq!(config.develop.max_retries, 3);
        assert!(config.providers.openrouter.is_none());
    }

    #[test]
    fn test_load_config_with_project_credentials() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:anthropic/claude-3.5-sonnet\"]\n",
        )
        .unwrap();

        let dot_wb = temp_dir.path().join(".tauqe");
        std::fs::create_dir_all(&dot_wb).unwrap();
        std::fs::write(
            dot_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-credentials-key\"\n",
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            Some("sk-credentials-key")
        );
    }

    #[test]
    fn test_load_config_with_explicit_options() {
        let temp_dir = tempfile::tempdir().unwrap();
        let custom_cfg = temp_dir.path().join("my-custom.toml");
        std::fs::write(
            &custom_cfg,
            "[models]\nprimary = [\"openrouter:deepseek/deepseek-chat\"]\n",
        )
        .unwrap();

        let custom_creds = temp_dir.path().join("my-creds.toml");
        std::fs::write(
            &custom_creds,
            "[openrouter]\napi_key = \"sk-explicit-creds\"\n",
        )
        .unwrap();

        let config = load_config_with_options(ConfigLoadOptions {
            repo_root: None,
            explicit_config: Some(&custom_cfg),
            explicit_credentials: Some(&custom_creds),
        });

        assert_eq!(
            config.active_model(),
            ModelRef::openrouter("deepseek/deepseek-chat")
        );
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            Some("sk-explicit-creds")
        );
    }

    #[test]
    fn test_malformed_config_surfaces_diagnostic() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\ncoder = = \"broken\"\n",
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        assert!(
            config.config_error.is_some(),
            "a malformed config must record a diagnostic instead of silently defaulting"
        );
    }

    #[test]
    fn test_invalid_models_config_surfaces_diagnostic() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:test/model\"]\ncoder = []\n",
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        assert!(
            config.config_error.is_some(),
            "an invalid models config must record a diagnostic"
        );
        assert!(config
            .config_error
            .unwrap()
            .contains("Invalid configuration"));
    }

    #[test]
    fn test_load_context_config_reads_pinned() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[context]\npinned = [\"Conventions.md\", \"README.md\"]\nmax_discovery_rounds = 9\n",
        )
        .unwrap();

        let cfg = load_context_config(temp_dir.path());
        assert_eq!(
            cfg.pinned,
            vec!["Conventions.md".to_string(), "README.md".to_string()]
        );
        assert_eq!(cfg.max_discovery_rounds, 9);
    }

    #[test]
    fn test_validate_project_config_on_disk_accepts_valid_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            r#"
[models]
primary = ["openrouter:a/b"]

[project.verify]
command = "cargo"
args = ["test"]
"#,
        )
        .unwrap();

        assert!(validate_project_config_on_disk(temp_dir.path()).is_ok());
    }

    #[test]
    fn test_validate_project_config_on_disk_accepts_absent_file() {
        // A repository without a configuration file is governed by built-in
        // defaults, so the on-disk check has nothing to reject.
        let temp_dir = tempfile::tempdir().unwrap();
        assert!(validate_project_config_on_disk(temp_dir.path()).is_ok());
    }

    #[test]
    fn test_validate_project_config_on_disk_rejects_malformed_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\ncoder = = \"broken\"\n",
        )
        .unwrap();

        let err = validate_project_config_on_disk(temp_dir.path())
            .expect_err("malformed TOML must be rejected");
        assert!(err.contains("Failed to parse"), "got: {err}");
    }

    #[test]
    fn test_validate_project_config_on_disk_rejects_semantically_invalid_config() {
        // The file parses as TOML but declares no verification command, so the
        // semantic layer must reject it.
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n",
        )
        .unwrap();

        let err = validate_project_config_on_disk(temp_dir.path())
            .expect_err("a configuration without verification must be rejected");
        assert!(err.contains("Invalid configuration"), "got: {err}");
        assert!(err.contains("verify"), "got: {err}");
    }

    #[test]
    fn test_project_section_verification_is_mandatory() {
        let missing: AppConfig =
            toml::from_str("[models]\nprimary = [\"openrouter:a/b\"]\n").expect("parse config");
        let err = missing
            .validate()
            .expect_err("a configuration without verification must be rejected");
        assert!(
            err.contains("verify"),
            "the diagnostic must name the missing command: {err}"
        );

        let declared: AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:a/b"]

[project.verify]
command = "cargo"
args = ["test"]
"#,
        )
        .expect("parse config");
        assert!(declared.validate().is_ok());
        assert_eq!(
            declared
                .project
                .verify_command()
                .expect("verify resolves")
                .args,
            ["test"]
        );
    }

    #[test]
    fn test_project_section_validation_rejects_broken_commands() {
        let cyclic: AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:a/b"]

[project.verify]
command = "@first"

[project.commands.first]
command = "@second"

[project.commands.second]
command = "@first"
"#,
        )
        .expect("parse config");
        assert!(cyclic.validate().is_err());

        let unknown: AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:a/b"]

[project.verify]
command = "@missing"
"#,
        )
        .expect("parse config");
        assert!(unknown.validate().is_err());

        // A declared command may be reused from `verify`, and the resolved
        // command line keeps the arguments of the target entry.
        let reused: AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:a/b"]

[project.verify]
command = "@check"

[project.commands.check]
command = "cargo"
args = ["check", "--all-targets"]
"#,
        )
        .expect("parse config");
        assert!(reused.validate().is_ok());
        assert_eq!(
            reused.project.verify_command().expect("verify resolves"),
            tauqe_protocol::ResolvedProjectCommand {
                command: "cargo".to_string(),
                args: vec!["check".to_string(), "--all-targets".to_string()],
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_resolve_prompt_layout_follows_intent_and_configuration() {
        // Without an explicit override the turn intent decides.
        let default_cfg = AppConfig::default();
        assert_eq!(
            default_cfg.resolve_prompt_layout(TurnIntent::InteractiveRepl),
            PromptLayoutKind::Chronological
        );
        assert_eq!(
            default_cfg.resolve_prompt_layout(TurnIntent::PlanDiscussion),
            PromptLayoutKind::Chronological
        );
        assert_eq!(
            default_cfg.resolve_prompt_layout(TurnIntent::IsolatedStep),
            PromptLayoutKind::Semantic
        );

        // A declared value overrides the resolution for every intent.
        for (declared, expected) in [
            ("semantic", PromptLayoutKind::Semantic),
            ("chronological", PromptLayoutKind::Chronological),
        ] {
            let toml_str = format!("[develop]\nprompt_layout = \"{declared}\"\n");
            let config: AppConfig = toml::from_str(&toml_str).expect("parse config");
            for intent in [
                TurnIntent::InteractiveRepl,
                TurnIntent::PlanDiscussion,
                TurnIntent::IsolatedStep,
            ] {
                assert_eq!(
                    config.resolve_prompt_layout(intent),
                    expected,
                    "declared {declared} must apply to {intent:?}"
                );
            }
        }
    }
}
