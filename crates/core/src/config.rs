use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Active (default) model. Empty means "take first of `available`, or built-in default".
    #[serde(default)]
    pub default: String,
    /// Models the user may switch between. Always contains `default` after `normalize()`.
    #[serde(default)]
    pub available: Vec<String>,
}

pub const BUILTIN_DEFAULT_MODEL: &str = "anthropic/claude-3.5-sonnet";

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            default: BUILTIN_DEFAULT_MODEL.to_string(),
            available: vec![BUILTIN_DEFAULT_MODEL.to_string()],
        }
    }
}

impl ModelsConfig {
    /// Trims, de-duplicates and guarantees that `default` is non-empty and listed in `available`.
    pub fn normalize(&mut self) {
        let mut cleaned: Vec<String> = Vec::new();
        for m in self.available.drain(..) {
            let m = m.trim().to_string();
            if !m.is_empty() && !cleaned.contains(&m) {
                cleaned.push(m);
            }
        }

        let mut default = self.default.trim().to_string();
        if default.is_empty() {
            default = cleaned
                .first()
                .cloned()
                .unwrap_or_else(|| BUILTIN_DEFAULT_MODEL.to_string());
        }
        if !cleaned.contains(&default) {
            cleaned.insert(0, default.clone());
        }

        self.default = default;
        self.available = cleaned;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditConfig {
    #[serde(default = "default_workflow")]
    pub workflow: String, // "git", "naive", "toolchain"
    #[serde(default = "default_protocol")]
    pub protocol: String, // "xml", "structured"
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_workflow() -> String {
    "toolchain".to_string()
}

fn default_protocol() -> String {
    "xml".to_string()
}

fn default_max_retries() -> usize {
    3
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            workflow: default_workflow(),
            protocol: default_protocol(),
            max_retries: default_max_retries(),
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
        .and_then(|p| {
            match std::fs::read_to_string(&p) {
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
            }
        })
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

    config.models.normalize();

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
    fn test_structured_protocol_config() {
        let toml_str = r#"
[edit]
workflow = "toolchain"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.edit.protocol, "structured");
        assert_eq!(config.edit.max_retries, 3);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[edit]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.edit.max_retries, 0);
    }

    #[test]
    fn test_single_model_backward_compat() {
        let toml_str = r#"
[models]
default = "openai/gpt-4o"
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "openai/gpt-4o");
        assert_eq!(config.models.available, vec!["openai/gpt-4o".to_string()]);
    }

    #[test]
    fn test_available_models_include_default() {
        let toml_str = r#"
[models]
default = "a/b"
available = ["c/d", "e/f", "c/d"]
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "a/b");
        assert_eq!(
            config.models.available,
            vec!["a/b".to_string(), "c/d".to_string(), "e/f".to_string()]
        );
    }

    #[test]
    fn test_available_without_default_uses_first() {
        let toml_str = r#"
[models]
available = ["x/y", "z/w"]
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "x/y");
        assert_eq!(config.models.available.len(), 2);
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.models.default, "anthropic/claude-3.5-sonnet");
        assert_eq!(config.edit.workflow, "toolchain");
        assert_eq!(config.edit.protocol, "xml");
        assert_eq!(config.edit.max_retries, 3);
        assert!(config.providers.openrouter.is_none());
        assert!(config.toolchain.check_command.is_none());
    }
}
