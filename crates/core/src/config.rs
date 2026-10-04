use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub models: ModelsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub openrouter: Option<OpenRouterConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenRouterConfig {
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub default: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            default: "anthropic/claude-3.5-sonnet".to_string(),
        }
    }
}

pub fn load_config(repo_root: Option<&Path>) -> AppConfig {
    let mut candidates = Vec::new();

    if let Some(root) = repo_root {
        candidates.push(root.join("workbench.toml"));
        candidates.push(root.join(".workbench.toml"));
    }

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("workbench.toml"));
        candidates.push(cwd.join(".workbench.toml"));
    }

    if let Some(home) = dirs_home() {
        candidates.push(home.join(".config/workbench/workbench.toml"));
        candidates.push(home.join(".workbench.toml"));
    }

    let mut config = candidates
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|content| toml::from_str::<AppConfig>(&content).ok())
        .unwrap_or_default();

    // Fallback to environment variables
    if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
        if !key.trim().is_empty() {
            let mut or_cfg = config.providers.openrouter.unwrap_or_default();
            if or_cfg.api_key.is_none() {
                or_cfg.api_key = Some(key.trim().to_string());
            }
            config.providers.openrouter = Some(or_cfg);
        }
    }

    if let Ok(model) = std::env::var("WORKBENCH_MODEL") {
        if !model.trim().is_empty() {
            config.models.default = model.trim().to_string();
        }
    }

    config
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.models.default, "anthropic/claude-3.5-sonnet");
        assert!(config.providers.openrouter.is_none());
    }

    #[test]
    fn test_parse_toml_config() {
        let toml_str = r#"
            [models]
            default = "openai/gpt-4o"

            [providers.openrouter]
            api_key = "sk-or-v1-12345"
        "#;

        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.models.default, "openai/gpt-4o");
        
        let openrouter = config.providers.openrouter.expect("openrouter config missing");
        assert_eq!(openrouter.api_key.unwrap(), "sk-or-v1-12345");
    }

    #[test]
    fn test_parse_partial_toml_config() {
        let toml_str = r#"
            [models]
            default = "google/gemini-pro"
        "#;

        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.models.default, "google/gemini-pro");
        assert!(config.providers.openrouter.is_none());
    }
}
