use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default)]
    pub edit: EditConfig,
    #[serde(default)]
    pub toolchain: ToolchainConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolchainConfig {
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub max_retries: Option<usize>,
    #[serde(default)]
    pub auto_heal: Option<bool>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditConfig {
    #[serde(default = "default_workflow")]
    pub workflow: String, // "git", "naive", "toolchain"
    #[serde(default = "default_protocol")]
    pub protocol: String, // "xml", "whole_file", "tool_call"
}

fn default_workflow() -> String {
    "toolchain".to_string()
}

fn default_protocol() -> String {
    "xml".to_string()
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            workflow: default_workflow(),
            protocol: default_protocol(),
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
    fn test_tool_call_protocol_config() {
        let toml_str = r#"
[edit]
workflow = "toolchain"
protocol = "tool_call"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.edit.protocol, "tool_call");
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.models.default, "anthropic/claude-3.5-sonnet");
        assert_eq!(config.edit.workflow, "toolchain");
        assert_eq!(config.edit.protocol, "xml");
        assert!(config.providers.openrouter.is_none());
        assert!(config.toolchain.check_command.is_none());
    }
}
