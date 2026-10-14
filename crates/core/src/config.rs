use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauqe_protocol::{ModelRef, ModelRole, ModelRolesSummary, ModelSelection};

mod context;
mod credentials;
mod develop;
mod git;
mod history;
mod models;
mod paths;
mod providers;
mod toolchain;

pub use context::{
    ContextConfig, DEFAULT_FILES_TOKEN_BUDGET, DEFAULT_MAX_DISCOVERY_ROUNDS,
};
pub use credentials::default_credentials_path;
pub use develop::{DevelopConfig, EditConfig};
pub use git::GitConfig;
pub use history::HistoryConfig;
pub use models::{
    deserialize_model_ref_opt, parse_model_ref, serialize_model_ref_opt, ModelsConfig,
    BUILTIN_DEFAULT_ARCHITECT_MODEL, BUILTIN_DEFAULT_CODER_MODEL, BUILTIN_DEFAULT_MODEL,
    BUILTIN_DEFAULT_PRIMARY_MODEL, BUILTIN_DEFAULT_REVIEWER_MODEL, BUILTIN_DEFAULT_SCRIBE_MODEL,
};
pub use paths::{
    cli_arg_value_from, config_candidates, default_config_path, find_config_file,
    write_default_config, write_default_conventions, DEFAULT_CONFIG_TEMPLATE,
};
pub use providers::{
    find_credentials_file, has_openrouter_key, load_credentials, read_credentials_file,
    write_credentials_file, write_credentials_stub, Credentials, OpenRouterConfig,
    OpenRouterCredentials, ProvidersConfig,
};
pub use toolchain::{ToolchainConfig, DEFAULT_TOOLCHAIN_TIMEOUT_SECS};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default, alias = "edit")]
    pub develop: DevelopConfig,
    #[serde(default)]
    pub toolchain: ToolchainConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
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

impl AppConfig {
    /// Models from provider sections, trimmed and de-duplicated.
    pub fn configured_models(&self) -> Vec<ModelRef> {
        let names = self
            .providers
            .openrouter
            .iter()
            .flat_map(|c| c.models.iter());

        let mut models = Vec::new();
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let model = ModelRef::openrouter(name);
            if !models.contains(&model) {
                models.push(model);
            }
        }
        models
    }

    /// Resolves a ModelRole to a concrete ModelRef using the deterministic fallback chain.
    pub fn resolve_role(&self, role: ModelRole) -> ModelRef {
        let fallback = self.configured_models().into_iter().next();
        self.models.resolve_role_with_fallback(role, fallback)
    }

    /// Resolves a ModelSelection to a concrete ModelRef.
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Role(role) => self.resolve_role(*role),
            ModelSelection::Specific(model) => model.clone(),
        }
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
    pub fn validate(&self) -> Result<(), String> {
        self.models.validate()
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
    let config_path = opts
        .explicit_config
        .map(PathBuf::from)
        .or_else(|| find_config_file(opts.repo_root, opts.explicit_config));

    let mut config = AppConfig::default();
    if let Some(p) = config_path.as_deref().filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::read_to_string(p) {
            Ok(content) => match toml::from_str::<AppConfig>(&content) {
                Ok(parsed) => config = parsed,
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

/// Resolves repository-local `[git]` integration settings for a repository root.
///
/// Only the `[git]` table is parsed: commit-path helpers need a single
/// deterministic flag and must not pay for credential resolution or unrelated
/// configuration sections on every commit. A missing, unreadable, or malformed
/// configuration resolves to the built-in defaults.
pub fn load_git_config(repo_root: &Path) -> GitConfig {
    let Some(path) = find_config_file(Some(repo_root), None) else {
        return GitConfig::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return GitConfig::default();
    };

    #[derive(Deserialize)]
    struct GitSection {
        #[serde(default)]
        git: GitConfig,
    }

    toml::from_str::<GitSection>(&content)
        .map(|section| section.git)
        .unwrap_or_default()
}

/// Resolves repository-local `[toolchain]` settings for a repository root.
///
/// Mirrors [`load_git_config`]: only the `[toolchain]` table is parsed so the
/// verification pipeline can honor a custom `check_command` and the configured
/// per-command timeout without paying for the whole application configuration.
/// A missing, unreadable, or malformed configuration resolves to the built-in
/// defaults.
pub fn load_toolchain_config(repo_root: &Path) -> ToolchainConfig {
    let Some(path) = find_config_file(Some(repo_root), None) else {
        return ToolchainConfig::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return ToolchainConfig::default();
    };

    #[derive(Deserialize)]
    struct ToolchainSection {
        #[serde(default)]
        toolchain: ToolchainConfig,
    }

    toml::from_str::<ToolchainSection>(&content)
        .map(|section| section.toolchain)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(config.active_model(), ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(config.default_selection(), ModelSelection::Role(ModelRole::Primary));
        assert_eq!(config.history_model(), ModelRef::openrouter("anthropic/claude-haiku-latest"));

        let roles = config.roles_summary();
        assert_eq!(roles.scribe, ModelRef::openrouter("anthropic/claude-haiku-latest"));
        assert_eq!(roles.coder, ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(roles.architect, ModelRef::openrouter("anthropic/claude-sonnet-latest"));
        assert_eq!(roles.primary, ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(roles.reviewer, ModelRef::openrouter("anthropic/claude-sonnet-latest"));

        let models_summary = config.models.roles_summary();
        assert_eq!(models_summary.scribe, roles.scribe);
        assert_eq!(models_summary.coder, roles.coder);
        assert_eq!(models_summary.architect, roles.architect);
        assert_eq!(models_summary.primary, roles.primary);
        assert_eq!(models_summary.reviewer, roles.reviewer);

        let specific = ModelRef::openrouter("custom/model");
        assert_eq!(config.resolve_model(&ModelSelection::Specific(specific.clone())), specific);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Scribe)), roles.scribe);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Coder)), roles.coder);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Architect)), roles.architect);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Primary)), roles.primary);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Reviewer)), roles.reviewer);
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
            Some(tauqe_protocol::ReasoningEffort::High)
        );
        assert_eq!(
            config.models.resolve_effort(ModelRole::Coder),
            Some(tauqe_protocol::ReasoningEffort::Medium)
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
    fn test_typed_provider_models_config() {
        let toml_str = r#"
[models]
primary = ["openrouter:openai/gpt-4o"]

[providers.openrouter]
models = ["deepseek/deepseek-chat", " ", "openai/gpt-4o"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.active_model(), ModelRef::openrouter("openai/gpt-4o"));
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("deepseek/deepseek-chat")));
        assert!(available.contains(&ModelRef::openrouter("openai/gpt-4o")));
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(
            config.active_model(),
            ModelRef::openrouter(BUILTIN_DEFAULT_PRIMARY_MODEL)
        );
        assert_eq!(config.develop.workflow, "git");
        assert_eq!(config.develop.protocol, "xml");
        assert_eq!(config.develop.max_retries, 3);
        assert!(config.providers.openrouter.is_none());
        assert!(config.toolchain.check_command.is_none());
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
    fn test_load_toolchain_config_reads_custom_command_and_timeout() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[toolchain]\ncheck_command = \"make verify\"\nauto_heal = false\ntimeout_secs = 42\n",
        )
        .unwrap();

        let cfg = load_toolchain_config(temp_dir.path());
        assert_eq!(cfg.check_command.as_deref(), Some("make verify"));
        assert_eq!(cfg.auto_heal, Some(false));
        assert_eq!(cfg.effective_timeout_secs(), 42);
    }

    #[test]
    fn test_load_toolchain_config_defaults_when_absent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cfg = load_toolchain_config(temp_dir.path());
        assert!(cfg.check_command.is_none());
        assert_eq!(cfg.effective_timeout_secs(), DEFAULT_TOOLCHAIN_TIMEOUT_SECS);
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
}
