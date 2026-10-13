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
    ContextConfig, DiscoveryMode, DEFAULT_MAX_CONTEXT_FILES, DEFAULT_MAX_DISCOVERY_ROUNDS,
};
pub use credentials::default_credentials_path;
pub use develop::{DevelopConfig, EditConfig};
pub use git::GitConfig;
pub use history::HistoryConfig;
pub use models::{
    deserialize_model_ref_opt, parse_model_ref, serialize_model_ref_opt, CoderConfig, ModelsConfig,
    BUILTIN_DEFAULT_ADVISOR_MODEL, BUILTIN_DEFAULT_ARCHITECT_MODEL, BUILTIN_DEFAULT_CODER_MODEL,
    BUILTIN_DEFAULT_MODEL, BUILTIN_DEFAULT_REVIEWER_MODEL, BUILTIN_DEFAULT_SCRIBE_MODEL,
};
pub use paths::{
    cli_arg_value, cli_arg_value_from, config_candidates, default_config_path, find_config_file,
    write_default_config, write_default_conventions, DEFAULT_CONFIG_TEMPLATE,
};
pub use providers::{
    find_credentials_file, has_openrouter_key, load_credentials, read_credentials_file,
    write_credentials_file, write_credentials_stub, Credentials, OpenRouterConfig,
    OpenRouterCredentials, ProvidersConfig,
};
pub use toolchain::ToolchainConfig;

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

    /// The default active model based on `Advisor` role.
    pub fn active_model(&self) -> ModelRef {
        self.resolve_role(ModelRole::Advisor)
    }

    /// The default model selection based on `Advisor` role.
    pub fn default_selection(&self) -> ModelSelection {
        ModelSelection::Role(ModelRole::Advisor)
    }

    /// The model used for history summarization and squash commit message generation.
    pub fn history_model(&self) -> ModelRef {
        self.resolve_role(ModelRole::Scribe)
    }

    /// Models the user may switch between. Always contains all role models and configured models.
    pub fn available_models(&self) -> Vec<ModelRef> {
        let mut models = self.configured_models();
        let summary = self.roles_summary();
        for m in [
            summary.scribe,
            summary.coder,
            summary.architect,
            summary.advisor,
            summary.reviewer,
        ] {
            if !models.contains(&m) {
                models.push(m);
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
        .or_else(|| find_config_file(opts.repo_root));

    let mut config = config_path
        .and_then(|p| match std::fs::read_to_string(&p) {
            Ok(content) => match toml::from_str::<AppConfig>(&content) {
                Ok(cfg) => Some(cfg),
                Err(err) => {
                    tracing::warn!("Failed to parse config file {:?}: {}", p, err);
                    None
                }
            },
            Err(err) => {
                tracing::warn!("Failed to read config file {:?}: {}", p, err);
                None
            }
        })
        .unwrap_or_default();

    credentials::apply_credentials(&mut config, opts.explicit_credentials, opts.repo_root);

    config
}

pub fn load_config(repo_root: Option<&Path>) -> AppConfig {
    let cli_config = cli_arg_value("--config");
    let cli_credentials = cli_arg_value("--credentials");
    load_config_with_options(ConfigLoadOptions {
        repo_root,
        explicit_config: cli_config.as_deref(),
        explicit_credentials: cli_credentials.as_deref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_models_config_and_roles_resolution() {
        let toml_str = r#"
[models]
scribe = "openrouter:anthropic/claude-haiku-latest"
coder = "openrouter:google/gemini-flash-latest"
architect = "openrouter:anthropic/claude-sonnet-latest"
advisor = "openrouter:google/gemini-flash-latest"
reviewer = "openrouter:anthropic/claude-sonnet-latest"
auto_level_up = true
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(config.models.validate().is_ok());
        assert!(config.models.auto_level_up);
        assert_eq!(config.active_model(), ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(config.default_selection(), ModelSelection::Role(ModelRole::Advisor));
        assert_eq!(config.history_model(), ModelRef::openrouter("anthropic/claude-haiku-latest"));

        let roles = config.roles_summary();
        assert_eq!(roles.scribe, ModelRef::openrouter("anthropic/claude-haiku-latest"));
        assert_eq!(roles.coder, ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(roles.architect, ModelRef::openrouter("anthropic/claude-sonnet-latest"));
        assert_eq!(roles.advisor, ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(roles.reviewer, ModelRef::openrouter("anthropic/claude-sonnet-latest"));

        let models_summary = config.models.roles_summary();
        assert_eq!(models_summary.scribe, roles.scribe);
        assert_eq!(models_summary.coder, roles.coder);
        assert_eq!(models_summary.architect, roles.architect);
        assert_eq!(models_summary.advisor, roles.advisor);
        assert_eq!(models_summary.reviewer, roles.reviewer);

        let specific = ModelRef::openrouter("custom/model");
        assert_eq!(config.resolve_model(&ModelSelection::Specific(specific.clone())), specific);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Scribe)), roles.scribe);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Coder)), roles.coder);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Architect)), roles.architect);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Advisor)), roles.advisor);
        assert_eq!(config.resolve_model(&ModelSelection::Role(ModelRole::Reviewer)), roles.reviewer);
    }

    #[test]
    fn test_models_config_validation() {
        let empty_cfg = ModelsConfig::default();
        assert!(empty_cfg.validate().is_err());

        let one_role = ModelsConfig {
            coder: Some(ModelRef::openrouter("qwen/coder").into()),
            ..Default::default()
        };
        assert!(one_role.validate().is_ok());
    }

    #[test]
    fn test_single_model_fallback_closes_all_roles() {
        let toml_str = r#"
[models]
coder = "openrouter:anthropic/claude-3.7-sonnet"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(config.models.validate().is_ok());
        let expected = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(config.resolve_role(ModelRole::Scribe), expected);
        assert_eq!(config.resolve_role(ModelRole::Coder), expected);
        assert_eq!(config.resolve_role(ModelRole::Architect), expected);
        assert_eq!(config.resolve_role(ModelRole::Advisor), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_role_resolution_single_role_each() {
        // 1. Only Scribe
        let scribe_only = ModelsConfig {
            scribe: Some(ModelRef::openrouter("model/scribe")),
            ..Default::default()
        };
        let expected_scribe = ModelRef::openrouter("model/scribe");
        assert_eq!(scribe_only.resolve_role(ModelRole::Scribe), expected_scribe);
        assert_eq!(scribe_only.resolve_role(ModelRole::Coder), expected_scribe);
        assert_eq!(scribe_only.resolve_role(ModelRole::Architect), expected_scribe);
        assert_eq!(scribe_only.resolve_role(ModelRole::Advisor), expected_scribe);

        // 2. Only Architect
        let architect_only = ModelsConfig {
            architect: Some(ModelRef::openrouter("model/architect")),
            ..Default::default()
        };
        let expected_architect = ModelRef::openrouter("model/architect");
        assert_eq!(architect_only.resolve_role(ModelRole::Scribe), expected_architect);
        assert_eq!(architect_only.resolve_role(ModelRole::Coder), expected_architect);
        assert_eq!(architect_only.resolve_role(ModelRole::Architect), expected_architect);
        assert_eq!(architect_only.resolve_role(ModelRole::Advisor), expected_architect);

        // 3. Only Advisor
        let advisor_only = ModelsConfig {
            advisor: Some(ModelRef::openrouter("model/advisor")),
            ..Default::default()
        };
        let expected_advisor = ModelRef::openrouter("model/advisor");
        assert_eq!(advisor_only.resolve_role(ModelRole::Scribe), expected_advisor);
        assert_eq!(advisor_only.resolve_role(ModelRole::Coder), expected_advisor);
        assert_eq!(advisor_only.resolve_role(ModelRole::Architect), expected_advisor);
        assert_eq!(advisor_only.resolve_role(ModelRole::Advisor), expected_advisor);
        assert_eq!(advisor_only.resolve_role(ModelRole::Reviewer), expected_advisor);

        // 4. Only Reviewer
        let reviewer_only = ModelsConfig {
            reviewer: Some(ModelRef::openrouter("model/reviewer")),
            ..Default::default()
        };
        let expected_reviewer = ModelRef::openrouter("model/reviewer");
        assert_eq!(reviewer_only.resolve_role(ModelRole::Scribe), expected_reviewer);
        assert_eq!(reviewer_only.resolve_role(ModelRole::Coder), expected_reviewer);
        assert_eq!(reviewer_only.resolve_role(ModelRole::Architect), expected_reviewer);
        assert_eq!(reviewer_only.resolve_role(ModelRole::Advisor), expected_reviewer);
        assert_eq!(reviewer_only.resolve_role(ModelRole::Reviewer), expected_reviewer);
    }

    #[test]
    fn test_role_resolution_two_roles_proximity() {
        let m_scribe = ModelRef::openrouter("model/scribe");
        let m_coder = ModelRef::openrouter("model/coder");
        let m_architect = ModelRef::openrouter("model/architect");
        let m_advisor = ModelRef::openrouter("model/advisor");

        // Pair 1: Scribe + Coder
        // Architect -> Advisor(none) -> Coder -> Scribe => Coder
        // Advisor -> Architect(none) -> Coder -> Scribe => Coder
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            coder: Some(m_coder.clone().into()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_scribe);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_coder);

        // Pair 2: Scribe + Architect
        // Coder -> Architect -> Advisor(none) -> Scribe => Architect
        // Advisor -> Architect -> Coder(none) -> Scribe => Architect
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            architect: Some(m_architect.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_scribe);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_architect);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_architect);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_architect);

        // Pair 3: Scribe + Advisor
        // Coder -> Architect(none) -> Advisor -> Scribe => Advisor
        // Architect -> Advisor -> Coder(none) -> Scribe => Advisor
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_scribe);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_advisor);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_advisor);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_advisor);

        // Pair 4: Coder + Architect
        // Scribe -> Coder -> Advisor(none) -> Architect => Coder
        // Advisor -> Architect -> Coder -> Scribe(none) => Architect
        let cfg = ModelsConfig {
            coder: Some(m_coder.clone().into()),
            architect: Some(m_architect.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_architect);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_architect);

        // Pair 5: Coder + Advisor
        // Scribe -> Coder -> Advisor -> Architect(none) => Coder
        // Architect -> Advisor -> Coder -> Scribe(none) => Advisor
        let cfg = ModelsConfig {
            coder: Some(m_coder.clone().into()),
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_coder);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_advisor);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_advisor);

        // Pair 6: Architect + Advisor
        // Scribe -> Coder(none) -> Advisor -> Architect => Advisor
        // Coder -> Architect -> Advisor -> Scribe(none) => Architect
        let cfg = ModelsConfig {
            architect: Some(m_architect.clone()),
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_advisor);
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_architect);
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_architect);
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_advisor);
    }

    #[test]
    fn test_role_resolution_three_roles_proximity() {
        let m_scribe = ModelRef::openrouter("model/scribe");
        let m_coder = ModelRef::openrouter("model/coder");
        let m_architect = ModelRef::openrouter("model/architect");
        let m_advisor = ModelRef::openrouter("model/advisor");

        // Missing Advisor: Scribe + Coder + Architect
        // Advisor -> Architect -> Coder -> Scribe => Architect
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            coder: Some(m_coder.clone().into()),
            architect: Some(m_architect.clone()),
            advisor: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Advisor), m_architect);

        // Missing Architect: Scribe + Coder + Advisor
        // Architect -> Advisor -> Coder -> Scribe => Advisor
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            coder: Some(m_coder.clone().into()),
            architect: None,
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Architect), m_advisor);

        // Missing Coder: Scribe + Architect + Advisor
        // Coder -> Architect -> Advisor -> Scribe => Architect
        let cfg = ModelsConfig {
            scribe: Some(m_scribe.clone()),
            coder: None,
            architect: Some(m_architect.clone()),
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Coder), m_architect);

        // Missing Scribe: Coder + Architect + Advisor
        // Scribe -> Coder -> Advisor -> Architect => Coder
        let cfg = ModelsConfig {
            scribe: None,
            coder: Some(m_coder.clone().into()),
            architect: Some(m_architect.clone()),
            advisor: Some(m_advisor.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_role(ModelRole::Scribe), m_coder);
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
        assert_eq!(config.resolve_role(ModelRole::Advisor), expected);
        assert_eq!(config.resolve_role(ModelRole::Reviewer), expected);
        assert_eq!(config.active_model(), expected);
        assert_eq!(config.history_model(), expected);
    }

    #[test]
    fn test_available_models_include_all_roles() {
        let toml_str = r#"
[models]
scribe = "openrouter:a/scribe"
coder = "openrouter:b/coder"
architect = "openrouter:c/architect"
advisor = "openrouter:d/advisor"
reviewer = "openrouter:r/reviewer"

[providers.openrouter]
models = ["e/extra", "a/scribe"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("e/extra")));
        assert!(available.contains(&ModelRef::openrouter("a/scribe")));
        assert!(available.contains(&ModelRef::openrouter("b/coder")));
        assert!(available.contains(&ModelRef::openrouter("c/architect")));
        assert!(available.contains(&ModelRef::openrouter("d/advisor")));
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
max_files = 40
discovery_mode = "snapshot"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.max_discovery_rounds, 8);
        assert_eq!(config.context.max_files, 40);
        assert_eq!(config.context.discovery_mode, DiscoveryMode::Snapshot);

        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.max_discovery_rounds, 5);
        assert_eq!(default_cfg.max_files, 25);
        assert_eq!(default_cfg.discovery_mode, DiscoveryMode::Monotonic);

        assert_eq!(
            "monotonic".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
        assert_eq!(
            "cumulative".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
        assert_eq!(
            "append_only".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
    }

    #[test]
    fn test_typed_provider_models_config() {
        let toml_str = r#"
[models]
coder = "openrouter:openai/gpt-4o"

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
            ModelRef::openrouter(BUILTIN_DEFAULT_ADVISOR_MODEL)
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
            "[models]\ncoder = \"openrouter:anthropic/claude-3.5-sonnet\"\n",
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
            "[models]\ncoder = \"openrouter:deepseek/deepseek-chat\"\n",
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
}
