use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauqe_protocol::{
    ModelCatalogEntry, ModelRef, ModelRole, ModelRolesSummary, ModelSelection, ProjectConfig,
    PromptLayoutKind, ReasoningEffort,
};

mod context;
mod credentials;
mod git;
mod history;
mod models;
mod modules;
mod outer_docs;
pub(crate) mod paths;
mod providers;
mod repl;
mod review;

pub use context::{
    ContextConfig, DEFAULT_INCLUDE_REPO_MAP, DEFAULT_MAX_DIR_LIST_ENTRIES,
    DEFAULT_MAX_FEEDBACK_CHARS, DEFAULT_MAX_MATCHED_FILES_PER_SEARCH,
    DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND, DEFAULT_MAX_RESEARCH_ROUNDS,
    DEFAULT_MAX_SCRATCHPAD_CHARS, DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND, DEFAULT_TOKEN_BUDGET,
};
pub use credentials::default_credentials_path;
pub use git::GitConfig;
pub use history::HistoryConfig;
pub use models::{
    serialize_model_ref_opt, ModelsConfig, BUILTIN_DEFAULT_ARCHITECT_MODEL,
    BUILTIN_DEFAULT_CODER_MODEL, BUILTIN_DEFAULT_MODEL, BUILTIN_DEFAULT_REPL_MODEL,
    BUILTIN_DEFAULT_REVIEWER_MODEL, BUILTIN_DEFAULT_SCRIBE_MODEL,
};
pub use modules::{ensure_no_module_conflict, module_owners, ModuleOwners};
pub use outer_docs::{OuterDocEntry, OuterDocSource};
pub use repl::{EditConfig, ReplConfig};

use models::ModelsConfigRaw;
pub use paths::{
    cli_arg_value_from, config_candidates, default_config_path, find_config_file,
    is_repo_config_path, is_repo_module_path, write_default_config, write_default_conventions,
    DEFAULT_CONFIG_TEMPLATE,
};
pub use providers::{
    credentials_candidates, find_credentials_file, has_openrouter_key, load_credentials,
    read_credentials_file, write_credentials_file, write_credentials_stub, Credentials,
    OpenRouterConfig, OpenRouterCredentials, ProvidersConfig,
};
pub use review::{ReviewConfig, ReviewGroupConfig, ReviewPreset};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(try_from = "AppConfigRaw")]
pub struct AppConfig {
    pub providers: ProvidersConfig,
    pub models: ModelsConfig,
    /// The `[repl]` section: the profile every bounded turn of this project runs
    /// under.
    ///
    /// The section declares the workflow engine, the edit protocol, the patch
    /// strategy, the prompt layout override, and the retry and replanning
    /// budgets, so an isolated step and a discussion read exactly what an
    /// interactive REPL turn reads. The legacy `[develop]` and `[edit]`
    /// spellings stay permanent deserialization aliases of the section, while
    /// serialization emits only the canonical `[repl]` name.
    pub repl: ReplConfig,
    /// The `[project]` section: the commands this repository declares.
    ///
    /// It is the single surface that tells the harness how to verify, format,
    /// and post-process the project, and which named commands the model may
    /// call as typed custom tools. Verification is declarative: a project that
    /// declares no `verify` command is valid and simply runs no verification
    /// step, while every command the section does declare must resolve to a
    /// runnable command line.
    pub project: ProjectConfig,
    pub context: ContextConfig,
    /// The `[[outer_docs]]` declarations: external documentation the project
    /// mounts for the model.
    ///
    /// Each entry names one mount point (`id`), one `title`, an optional
    /// `description`, and exactly one source key — `url`, `path`, or `mdbook`.
    /// A `mdbook` source mounts a composite document whose index — `SUMMARY.md`,
    /// `index.md`, or `llms.txt`, tried in that order — names its parts, so one
    /// declaration covers a whole published corpus.
    /// A declared document is materialized lazily into the isolated cache on its
    /// first access, so an entry costs nothing until a turn asks for it. An
    /// absent section declares no external documentation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outer_docs: Vec<OuterDocEntry>,
    pub git: GitConfig,
    pub history: HistoryConfig,
    /// The `[review]` section: named presets for a multi-level review run.
    ///
    /// A preset decomposes one audit into isolated groups with their own paths
    /// and local prompts, and declares when the post-analysis layer is
    /// produced. An absent section declares no preset, so a single-audit
    /// session stays available without any migration.
    pub review: ReviewConfig,
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
    #[serde(default, alias = "develop", alias = "edit")]
    repl: ReplConfig,
    #[serde(default)]
    project: ProjectConfig,
    #[serde(default)]
    context: ContextConfig,
    #[serde(default)]
    outer_docs: Vec<OuterDocEntry>,
    #[serde(default)]
    git: GitConfig,
    #[serde(default)]
    history: HistoryConfig,
    #[serde(default)]
    review: ReviewConfig,
}

impl TryFrom<AppConfigRaw> for AppConfig {
    type Error = String;

    fn try_from(raw: AppConfigRaw) -> Result<Self, Self::Error> {
        let AppConfigRaw {
            providers,
            models,
            repl,
            project,
            context,
            outer_docs,
            git,
            history,
            review,
        } = raw;

        Ok(AppConfig {
            models: ModelsConfig::resolve(models, providers.catalog())?,
            providers,
            repl,
            project,
            context,
            outer_docs,
            git,
            history,
            review,
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
        self.resolve_role(ModelRole::REPL)
    }

    /// The default model selection based on `Primary` role.
    pub fn default_selection(&self) -> ModelSelection {
        ModelSelection::Role(ModelRole::REPL)
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
            ModelRole::REPL,
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
    /// Each section is validated before a configuration may drive a turn: the
    /// semantic model roles must resolve and every command the `[project]`
    /// declaration names must resolve through a sound reference chain.
    /// Verification is optional — a project that declares no `verify` command
    /// is valid and runs no verification step — so what remains rejected is a
    /// declaration the harness could not run predictably. An invalid
    /// configuration is rejected as a whole instead of degrading silently.
    pub fn validate(&self) -> Result<(), String> {
        self.models.validate()?;
        self.project
            .validate()
            .map_err(|err| format!("[project] {}", err))?;
        outer_docs::validate(&self.outer_docs).map_err(|err| format!("[outer_docs] {}", err))?;
        self.review
            .validate()
            .map_err(|err| format!("[review] {}", err))?;
        Ok(())
    }

    /// Resolves the prompt layout policy for a bounded turn.
    ///
    /// This is the single typed point that decides the layout: an explicitly
    /// configured [`PromptLayoutKind`] wins, otherwise the caller's typed
    /// argument decides.
    ///
    /// - A contour that carries a growing session history — an interactive REPL
    ///   turn, a plan discussion, and a review discussion — shares one
    ///   byte-stable head and builds the chronological transcript: its
    ///   append-only growth keeps the provider prefix cache warm across
    ///   dialogue focus switches.
    /// - A contour that carries no growing dialogue history to interleave — an
    ///   isolated step and an Architect synthesis — keeps the semantic block
    ///   order.
    pub fn resolve_prompt_layout(&self, carries_session_history: bool) -> PromptLayoutKind {
        if let Some(explicit) = self.repl.prompt_layout {
            return explicit;
        }

        if carries_session_history {
            PromptLayoutKind::Chronological
        } else {
            PromptLayoutKind::Semantic
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
        let base = config_path.as_deref().filter(|p| !p.as_os_str().is_empty());
        let label = modules::document_label(base, opts.repo_root);

        let mut config = AppConfig::default();
        match modules::read_document(base, opts.repo_root) {
            Ok(Some(document)) => match AppConfig::deserialize(toml::Value::Table(document)) {
                Ok(parsed) => {
                    config = parsed;
                    if let Err(err) = config.validate() {
                        tracing::warn!("Configuration validation failed for {}: {}", label, err);
                        config.config_error =
                            Some(format!("Invalid configuration in {}: {}", label, err));
                    }
                }
                Err(err) => {
                    tracing::warn!("Failed to parse config file {}: {}", label, err);
                    config.config_error = Some(format!("Failed to parse {}: {}", label, err));
                }
            },
            // A repository that declares no configuration at all is governed by
            // the built-in defaults, which is not a diagnostic.
            Ok(None) => {}
            Err(message) => {
                tracing::warn!("{}", message);
                config.config_error = Some(message);
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

/// Loads a specific configuration section without parsing the whole layered
/// document or loading credentials.
///
/// The section is read from the same merged document [`load_config_with_options`]
/// consumes, so a section declared by a `tauqe.d` module reaches this reader
/// exactly as it reaches the whole-configuration one.
///
/// Returns the default value if the repository declares no configuration, the
/// document cannot be read or merged, or the section cannot be parsed.
pub fn load_section<T: for<'de> Deserialize<'de> + Default>(
    repo_root: &Path,
    section_key: &str,
) -> T {
    // Locating and reading the configuration files is blocking filesystem
    // work, so it runs off the async executor thread.
    crate::blocking::run_blocking(|| {
        let base = find_config_file(Some(repo_root), None);
        let document = match modules::read_document(base.as_deref(), Some(repo_root)) {
            Ok(Some(document)) => document,
            Ok(None) => return T::default(),
            Err(message) => {
                // A section reader has no diagnostics channel of its own, so a
                // broken layered document degrades to the section defaults and
                // is reported once in the journal.
                tracing::warn!("{}", message);
                return T::default();
            }
        };
        document
            .get(section_key)
            .cloned()
            .and_then(|value| T::deserialize(value).ok())
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

/// Validates the project configuration document that governs `repo_root`.
///
/// The check is delegated to the engine rather than to a project tool, because
/// `tauqe.toml` defines the environment the turn runs in: the model roles, the
/// budgets, and the command lines the harness may execute. Two layers are
/// checked — the TOML syntax of every configuration layer and the semantic
/// invariants enforced by [`AppConfig::validate`] (resolvable model roles and a
/// sound `[project]` command chain). A file that declares no verification
/// command passes this check: verification is optional, and the harness never
/// invents a toolchain a project did not declare.
///
/// The layers are validated as the merged document a turn would load, so a
/// `tauqe.d` module is judged in the context it takes part in: a module that
/// collides with the base document fails here even though its own bytes parse.
///
/// A repository without a configuration file is valid: built-in defaults govern
/// it, exactly as they do for [`load_config_with_options`]. Reading and parsing
/// the files is blocking filesystem work, so it runs off the async executor
/// thread.
pub fn validate_project_config_on_disk(repo_root: &Path) -> Result<(), String> {
    crate::blocking::run_blocking(|| -> Result<(), String> {
        let base = find_config_file(Some(repo_root), None);
        let label = modules::document_label(base.as_deref(), Some(repo_root));
        let document = modules::read_document(base.as_deref(), Some(repo_root))?;
        let Some(document) = document else {
            return Ok(());
        };
        let config: AppConfig = AppConfig::deserialize(toml::Value::Table(document))
            .map_err(|err| format!("Failed to parse {}: {}", label, err))?;
        config
            .validate()
            .map_err(|err| format!("Invalid configuration in {}: {}", label, err))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::PostAnalysisMode;
    use tauqe_protocol::{PromptLayoutKind, WorkflowKind};
    #[test]
    fn test_models_config_and_roles_resolution() {
        let toml_str = r#"
[models]
repl = ["openrouter:google/gemini-flash-latest"]
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
            ModelSelection::Role(ModelRole::REPL)
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
            roles.repl,
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
        assert_eq!(models_summary.repl, roles.repl);
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
            config.resolve_model(&ModelSelection::Role(ModelRole::REPL)),
            roles.repl
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
repl = ["openrouter:google/gemini-flash-latest"]
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
        assert_eq!(config.models.resolve_effort(ModelRole::REPL), None);
    }

    #[test]
    fn test_models_config_validation() {
        let empty_cfg = ModelsConfig::default();
        assert!(empty_cfg.validate().is_err());

        let repl_only = ModelsConfig {
            repl: vec![ModelRef::openrouter("qwen/coder").into()],
            ..Default::default()
        };
        assert!(repl_only.validate().is_ok());

        let empty_coder = ModelsConfig {
            repl: vec![ModelRef::openrouter("qwen/coder").into()],
            coder: Some(Vec::new()),
            ..Default::default()
        };
        assert!(empty_coder.validate().is_err());
    }

    #[test]
    fn test_single_model_fallback_closes_all_roles() {
        let toml_str = r#"
[models]
repl = ["openrouter:anthropic/claude-3.7-sonnet"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(config.models.validate().is_ok());
        let expected = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::REPL), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_specialized_roles_inherit_repl_list() {
        let config: AppConfig = toml::from_str(
            r#"
[models]
repl = ["openrouter:alpha/model", "openrouter:beta/model"]
"#,
        )
        .expect("parse config");

        // A specialized role without its own list inherits the repl list.
        let expected = ModelRef::openrouter("alpha/model");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::REPL), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);

        // The coder escalation ladder inherits the full repl list.
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
        assert_eq!(config.resolve_role(ModelRole::REPL), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_available_models_include_all_roles() {
        let toml_str = r#"
[models]
repl = ["openrouter:d/repl"]
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
        assert!(available.contains(&ModelRef::openrouter("d/repl")));
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
    fn test_context_research_config() {
        let toml_str = r#"
[context]
max_research_rounds = 8
token_budget = 150000
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.max_research_rounds, 8);
        assert_eq!(config.context.token_budget, 150000);

        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.max_research_rounds, 5);
        assert_eq!(default_cfg.token_budget, 300_000);
    }

    #[test]
    fn test_research_and_feedback_limits_defaults_and_overrides() {
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
        assert_eq!(defaults.token_budget, DEFAULT_TOKEN_BUDGET);
        assert_eq!(defaults.max_search_requests_per_round, 5);
        assert_eq!(defaults.max_matched_files_per_search, 50);
        assert_eq!(defaults.max_dir_list_entries, 100);
        assert_eq!(defaults.max_repo_map_requests_per_round, 3);
        assert_eq!(defaults.max_feedback_chars, 4_000);
        assert_eq!(defaults.max_scratchpad_chars, 4_000);
        assert_eq!(defaults.token_budget, 300_000);

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
        assert_eq!(partial.context.token_budget, DEFAULT_TOKEN_BUDGET);
    }

    #[test]
    fn test_typed_provider_models_config() {
        let toml_str = r#"
[models]
repl = ["openrouter:openai/gpt-4o"]

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
repl = ["Deep"]
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
repl = ["anthropc:claude"]
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
            ModelRef::openrouter(BUILTIN_DEFAULT_REPL_MODEL)
        );
        assert_eq!(config.repl.workflow, WorkflowKind::Git);
        assert_eq!(config.repl.max_retries, 3);
        assert!(config.providers.openrouter.is_none());
    }

    #[test]
    fn test_load_config_with_project_credentials() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:anthropic/claude-3.5-sonnet\"]\n",
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
            "[models]\nrepl = [\"openrouter:deepseek/deepseek-chat\"]\n",
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
            "[models]\nrepl = [\"openrouter:test/model\"]\ncoder = []\n",
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
            "[context]\npinned = [\"Conventions.md\", \"README.md\"]\nmax_research_rounds = 9\n",
        )
        .unwrap();

        let cfg = load_context_config(temp_dir.path());
        assert_eq!(
            cfg.pinned,
            vec!["Conventions.md".to_string(), "README.md".to_string()]
        );
        assert_eq!(cfg.max_research_rounds, 9);
    }

    #[test]
    fn test_validate_project_config_on_disk_accepts_valid_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            r#"
[models]
repl = ["openrouter:a/b"]

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
    fn test_validate_project_config_on_disk_accepts_models_only_config() {
        // Verification is declarative, so a file that only assigns model roles
        // is a complete declaration.
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
        )
        .unwrap();

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
        // The file parses as TOML but routes its verification through a command
        // nobody declares, so the semantic layer must reject it.
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            concat!(
                "[models]\nrepl = [\"openrouter:a/b\"]\n",
                "\n[project.verify]\ncommand = \"@missing\"\n",
            ),
        )
        .unwrap();

        let err = validate_project_config_on_disk(temp_dir.path())
            .expect_err("a broken command chain must be rejected");
        assert!(err.contains("Invalid configuration"), "got: {err}");
        assert!(err.contains("missing"), "got: {err}");
    }

    #[test]
    fn test_project_section_verification_is_optional() {
        // A configuration that only assigns model roles is complete:
        // verification is a declared capability, not a mandatory barrier.
        let models_only: AppConfig =
            toml::from_str("[models]\nrepl = [\"openrouter:a/b\"]\n").expect("parse config");
        assert!(
            models_only.validate().is_ok(),
            "a configuration without a verification command must be valid"
        );
        assert!(
            models_only.project.verify.is_none(),
            "the project section declares no verification"
        );

        // A declared verification command still resolves through its chain.
        let declared: AppConfig = toml::from_str(
            r#"
[models]
repl = ["openrouter:a/b"]

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
repl = ["openrouter:a/b"]

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
repl = ["openrouter:a/b"]

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
repl = ["openrouter:a/b"]

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
    fn test_resolve_prompt_layout_follows_the_session_history_and_configuration() {
        // Without an explicit override the caller's typed argument decides.
        let default_cfg = AppConfig::default();
        assert_eq!(
            default_cfg.resolve_prompt_layout(true),
            PromptLayoutKind::Chronological
        );
        assert_eq!(
            default_cfg.resolve_prompt_layout(false),
            PromptLayoutKind::Semantic
        );

        // A declared value overrides the resolution for every contour.
        for (declared, expected) in [
            ("semantic", PromptLayoutKind::Semantic),
            ("chronological", PromptLayoutKind::Chronological),
        ] {
            let toml_str = format!("[repl]\nprompt_layout = \"{declared}\"\n");
            let config: AppConfig = toml::from_str(&toml_str).expect("parse config");
            for carries_session_history in [true, false] {
                assert_eq!(
                    config.resolve_prompt_layout(carries_session_history),
                    expected,
                    "declared {declared} must apply to a turn with history = {carries_session_history}"
                );
            }
        }
    }

    #[test]
    fn test_review_section_defaults_to_no_presets() {
        // The section is optional: an omitted declaration leaves the project
        // without a preset, so a single audit stays available.
        let default_cfg = AppConfig::default();
        assert!(default_cfg.review.presets.is_empty());
        assert!(default_cfg.review.validate().is_ok());

        let absent: AppConfig =
            toml::from_str("[models]\nrepl = [\"openrouter:a/b\"]\n").expect("parse config");
        assert!(absent.review.presets.is_empty());
    }

    #[test]
    fn test_review_section_parses_named_presets() {
        let toml_str = r#"
[models]
repl = ["openrouter:a/b"]

[project.verify]
command = "cargo"
args = ["test"]

[[review.presets]]
name = "backend"
title = "Backend audit"
post_analysis = "manual"

[[review.presets.groups]]
name = "core"
paths = ["crates/core/src"]
prompt = "Audit the harness invariants."

[[review.presets.groups]]
name = "server"
paths = ["crates/server/src", "crates/protocol/src"]

[[review.presets]]
name = "frontend"
post_analysis = "automatic"

[[review.presets.groups]]
name = "tui"
paths = ["crates/tui/src"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("parse config");
        assert_eq!(config.review.presets.len(), 2);

        let backend = &config.review.presets[0];
        assert_eq!(backend.name, "backend");
        assert_eq!(backend.title.as_deref(), Some("Backend audit"));
        assert_eq!(backend.post_analysis, PostAnalysisMode::Manual);
        assert_eq!(backend.groups.len(), 2);
        assert_eq!(backend.groups[0].name, "core");
        assert_eq!(backend.groups[0].paths, vec!["crates/core/src".to_string()]);
        assert_eq!(
            backend.groups[0].prompt.as_deref(),
            Some("Audit the harness invariants.")
        );
        assert_eq!(backend.groups[1].prompt, None);

        let frontend = &config.review.presets[1];
        assert_eq!(frontend.name, "frontend");
        assert_eq!(frontend.title, None);
        assert_eq!(frontend.post_analysis, PostAnalysisMode::Automatic);

        // A valid section keeps the whole configuration valid.
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_review_preset_materializes_launch_groups() {
        let config: AppConfig = toml::from_str(
            r#"
[[review.presets]]
name = "backend"

[[review.presets.groups]]
name = "core"
paths = ["crates/core/src", "crates/protocol/src"]
prompt = "Audit the wire contract."

[[review.presets.groups]]
name = "server"
paths = ["crates/server/src"]
"#,
        )
        .expect("parse config");

        let groups = config.review.presets[0].to_launch_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "core");
        assert_eq!(
            groups[0].paths,
            vec![
                "crates/core/src".to_string(),
                "crates/protocol/src".to_string()
            ]
        );
        assert_eq!(
            groups[0].prompt.as_deref(),
            Some("Audit the wire contract.")
        );
        assert_eq!(groups[1].title, "server");
        assert_eq!(groups[1].prompt, None);

        // Every launch mints its own identifiers, so two sessions started from
        // one preset never collide.
        assert!(crate::review::uuid::is_valid_uuid_v7(&groups[0].id));
        assert!(crate::review::uuid::is_valid_uuid_v7(&groups[1].id));
        assert_ne!(groups[0].id, groups[1].id);
        assert_ne!(
            groups[0].id,
            config.review.presets[0].to_launch_groups()[0].id
        );
    }

    #[test]
    fn test_review_section_rejects_empty_and_duplicate_preset_names() {
        let unnamed: AppConfig = toml::from_str(
            r#"
[[review.presets]]
name = "   "

[[review.presets.groups]]
name = "core"
paths = ["crates/core/src"]
"#,
        )
        .expect("parse config");
        let err = unnamed
            .review
            .validate()
            .expect_err("an empty preset name must be rejected");
        assert!(err.contains("empty name"), "got: {err}");

        // Names are compared after trimming, so surrounding whitespace cannot
        // smuggle a second preset under the same name.
        let duplicate: AppConfig = toml::from_str(
            r#"
[[review.presets]]
name = "backend"

[[review.presets.groups]]
name = "core"
paths = ["crates/core/src"]

[[review.presets]]
name = " backend "

[[review.presets.groups]]
name = "tui"
paths = ["crates/tui/src"]
"#,
        )
        .expect("parse config");
        let err = duplicate
            .review
            .validate()
            .expect_err("a duplicate preset name must be rejected");
        assert!(err.contains("backend"), "got: {err}");
    }

    #[test]
    fn test_review_section_rejects_a_preset_without_groups_or_paths() {
        let no_groups: AppConfig = toml::from_str(
            r#"
[[review.presets]]
name = "backend"
"#,
        )
        .expect("parse config");
        let err = no_groups
            .review
            .validate()
            .expect_err("a preset without groups must be rejected");
        assert!(err.contains("backend"), "got: {err}");

        let no_paths: AppConfig = toml::from_str(
            r#"
[models]
repl = ["openrouter:a/b"]

[project.verify]
command = "cargo"
args = ["test"]

[[review.presets]]
name = "backend"

[[review.presets.groups]]
name = "core"
paths = []
"#,
        )
        .expect("parse config");
        let err = no_paths
            .review
            .validate()
            .expect_err("a group without paths must be rejected");
        assert!(err.contains("core"), "got: {err}");

        // The rejection reaches the whole configuration through the section
        // hook and names the offending section.
        let whole = no_paths
            .validate()
            .expect_err("an invalid review section must invalidate the configuration");
        assert!(whole.contains("[review]"), "got: {whole}");
    }

    #[test]
    fn test_review_post_analysis_modes_are_typed() {
        let absent: AppConfig = toml::from_str(
            r#"
[[review.presets]]
name = "modes"

[[review.presets.groups]]
name = "core"
paths = ["crates/core/src"]
"#,
        )
        .expect("parse config");
        assert_eq!(
            absent.review.presets[0].post_analysis,
            PostAnalysisMode::None
        );

        for (declared, expected) in [
            ("automatic", PostAnalysisMode::Automatic),
            ("manual", PostAnalysisMode::Manual),
            ("none", PostAnalysisMode::None),
        ] {
            let toml_str = format!(
                "[[review.presets]]\nname = \"modes\"\npost_analysis = \"{declared}\"\n\n\
                 [[review.presets.groups]]\nname = \"core\"\npaths = [\"crates/core/src\"]\n"
            );
            let config: AppConfig = toml::from_str(&toml_str).expect("parse config");
            assert_eq!(
                config.review.presets[0].post_analysis, expected,
                "declared {declared} must resolve"
            );
        }

        // A spelling outside the three modes is rejected instead of silently
        // defaulting to `none`.
        let unknown = toml::from_str::<AppConfig>(
            "[[review.presets]]\nname = \"modes\"\npost_analysis = \"later\"\n\n\
             [[review.presets.groups]]\nname = \"core\"\npaths = [\"crates/core/src\"]\n",
        );
        assert!(
            unknown.is_err(),
            "an unknown post-analysis mode must be rejected"
        );
    }

    /// The canonical section is `[repl]`; the legacy `[develop]` and `[edit]`
    /// spellings stay permanent deserialization aliases, so a configuration
    /// written before the rename keeps loading unchanged while the serialized
    /// form always carries the canonical name.
    #[test]
    fn test_legacy_turn_profile_sections_decode_into_the_repl_profile() {
        let canonical: AppConfig = toml::from_str("[repl]\nworkflow = \"naive\"\n").expect("parse");
        assert_eq!(canonical.repl.workflow, WorkflowKind::Naive);

        for legacy in ["develop", "edit"] {
            let declaration = format!("[{legacy}]\nworkflow = \"naive\"\n");
            let config: AppConfig = toml::from_str(&declaration).expect("parse config");
            assert_eq!(
                config.repl.workflow,
                WorkflowKind::Naive,
                "the legacy [{legacy}] spelling of the profile section must keep decoding"
            );
        }

        #[derive(Serialize)]
        struct Profile<'a> {
            repl: &'a ReplConfig,
        }

        let serialized = toml::to_string(&Profile {
            repl: &canonical.repl,
        })
        .expect("serialize");
        assert!(serialized.starts_with("[repl]"), "got: {serialized}");
        for legacy in ["develop", "edit"] {
            assert!(
                !serialized.contains(legacy),
                "the serialized profile must carry the canonical section only: {serialized}"
            );
        }
    }
    /// Creates a repository with the given base configuration and module layers.
    fn layered_repo(base: &str, modules: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tauqe.toml"), base).unwrap();
        if !modules.is_empty() {
            let modules_dir = dir.path().join("tauqe.d");
            std::fs::create_dir_all(&modules_dir).unwrap();
            for (name, content) in modules {
                std::fs::write(modules_dir.join(name), content).unwrap();
            }
        }
        dir
    }

    #[test]
    fn test_module_layers_merge_sections_with_the_base_document() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n\n[context]\nrepomap_token_budget = 6000\n",
            &[("10-research.toml", "[context]\nmax_research_rounds = 9\n")],
        );

        let config = load_config(Some(dir.path()));

        assert!(
            config.config_error.is_none(),
            "got: {:?}",
            config.config_error
        );
        // The base document keeps its own key ...
        assert_eq!(config.context.repomap_token_budget, 6000);
        // ... and the module contributes the key it owns.
        assert_eq!(config.context.max_research_rounds, 9);
    }

    #[test]
    fn test_module_layers_concatenate_arrays_of_tables_in_lexical_order() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n\n\
             [[review.presets]]\nname = \"base\"\n\n\
             [[review.presets.groups]]\nname = \"core\"\npaths = [\"crates/core/src\"]\n",
            &[
                (
                    "10-first.toml",
                    "[[review.presets]]\nname = \"first\"\n\n\
                     [[review.presets.groups]]\nname = \"one\"\npaths = [\"a\"]\n",
                ),
                (
                    "20-second.toml",
                    "[[review.presets]]\nname = \"second\"\n\n\
                     [[review.presets.groups]]\nname = \"two\"\npaths = [\"b\"]\n",
                ),
            ],
        );

        let config = load_config(Some(dir.path()));
        assert!(
            config.config_error.is_none(),
            "got: {:?}",
            config.config_error
        );

        let names: Vec<&str> = config
            .review
            .presets
            .iter()
            .map(|preset| preset.name.as_str())
            .collect();
        assert_eq!(names, vec!["base", "first", "second"]);
    }

    #[test]
    fn test_module_layer_scalar_collision_names_both_files_and_the_key() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("10-models.toml", "[models]\nrepl = [\"openrouter:c/d\"]\n")],
        );

        let config = load_config(Some(dir.path()));

        let err = config
            .config_error
            .expect("a scalar collision must be reported");
        assert!(
            err.contains("tauqe.toml"),
            "the base file must be named: {err}"
        );
        assert!(
            err.contains("10-models.toml"),
            "the module must be named: {err}"
        );
        assert!(
            err.contains("models.repl"),
            "the colliding key must be named: {err}"
        );
    }

    #[test]
    fn test_module_layer_syntax_error_names_the_file_and_the_line() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("20-broken.toml", "[context]\nrepomap_token_budget = = 1\n")],
        );

        let config = load_config(Some(dir.path()));

        let err = config
            .config_error
            .expect("a module syntax error must be reported");
        assert!(
            err.contains("20-broken.toml"),
            "the module must be named: {err}"
        );
        assert!(err.contains("line 2"), "the line must be named: {err}");
    }

    #[test]
    fn test_load_section_observes_the_merged_module_layers() {
        let dir = layered_repo(
            "[context]\nrepomap_token_budget = 6000\n",
            &[("10-research.toml", "[context]\nmax_research_rounds = 9\n")],
        );

        let cfg = load_context_config(dir.path());

        assert_eq!(cfg.repomap_token_budget, 6000);
        assert_eq!(cfg.max_research_rounds, 9);
    }

    #[test]
    fn test_modules_resolve_from_the_repository_root_not_the_explicit_config() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("10-context.toml", "[context]\nmax_research_rounds = 11\n")],
        );
        let external = dir.path().join("external.toml");
        std::fs::write(&external, "[models]\nrepl = [\"openrouter:c/d\"]\n").unwrap();

        let config = load_config_with_options(ConfigLoadOptions {
            repo_root: Some(dir.path()),
            explicit_config: Some(&external),
            explicit_credentials: None,
        });

        assert!(
            config.config_error.is_none(),
            "got: {:?}",
            config.config_error
        );
        // The explicit document anchors the configuration ...
        assert_eq!(config.active_model(), ModelRef::openrouter("c/d"));
        // ... while the module directory stays the one of the repository.
        assert_eq!(config.context.max_research_rounds, 11);
    }

    #[test]
    fn test_validate_project_config_on_disk_rejects_a_module_collision() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("10-models.toml", "[models]\nrepl = [\"openrouter:c/d\"]\n")],
        );

        let err = validate_project_config_on_disk(dir.path())
            .expect_err("a module collision must fail the on-disk check");
        assert!(err.contains("models.repl"), "got: {err}");
    }

    #[test]
    fn test_module_owners_names_the_layer_declaring_each_key() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[
                (
                    "10-models.toml",
                    "[models]\nscribe = \"openrouter:s/scribe\"\n",
                ),
                ("20-repl.toml", "[repl]\nworkflow = \"naive\"\n"),
            ],
        );

        let owners = module_owners(dir.path()).expect("the modules are readable");

        let owner = |key: &str| {
            owners
                .get(key)
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        };
        assert_eq!(owner("models.scribe").as_deref(), Some("10-models.toml"));
        assert_eq!(owner("repl.workflow").as_deref(), Some("20-repl.toml"));
        assert!(
            !owners.contains_key("models.repl"),
            "the base document owns its own keys"
        );
    }

    #[test]
    fn test_ensure_no_module_conflict_reports_a_key_a_module_already_declares() {
        let dir = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("10-models.toml", "[models]\nrepl = [\"openrouter:c/d\"]\n")],
        );
        let target = dir.path().join("tauqe.toml");

        let err = ensure_no_module_conflict(DEFAULT_CONFIG_TEMPLATE, &target, Some(dir.path()))
            .expect_err("the template declares the same key");
        assert!(err.contains("models.repl"), "got: {err}");

        // A module that owns an independent key does not block the create.
        let free = layered_repo(
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
            &[("10-history.toml", "[history]\ntail_turns = 5\n")],
        );
        let free_target = free.path().join("tauqe.toml");
        assert!(
            ensure_no_module_conflict(DEFAULT_CONFIG_TEMPLATE, &free_target, Some(free.path()))
                .is_ok(),
            "an independent module key must not block the create"
        );
    }
}
