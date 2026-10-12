use serde::{Deserialize, Serialize};

/// Development profile: workflow engine, edit protocol and retry policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevelopConfig {
    #[serde(default = "default_workflow")]
    pub workflow: String, // "git", "naive"
    #[serde(default = "default_protocol")]
    pub protocol: String, // "xml", "structured"
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

/// Backwards-compatible alias for the legacy `[edit]` configuration section.
pub type EditConfig = DevelopConfig;

fn default_workflow() -> String {
    "git".to_string()
}

fn default_protocol() -> String {
    "xml".to_string()
}

fn default_max_retries() -> usize {
    3
}

impl Default for DevelopConfig {
    fn default() -> Self {
        Self {
            workflow: default_workflow(),
            protocol: default_protocol(),
            max_retries: default_max_retries(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    #[test]
    fn test_structured_protocol_config() {
        let toml_str = r#"
[develop]
workflow = "git"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, "git");
        assert_eq!(config.develop.protocol, "structured");
        assert_eq!(config.develop.max_retries, 3);
    }

    #[test]
    fn test_edit_alias_compatibility() {
        let toml_str = r#"
[edit]
workflow = "naive"
protocol = "xml"
max_retries = 1
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, "naive");
        assert_eq!(config.develop.protocol, "xml");
        assert_eq!(config.develop.max_retries, 1);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.develop.max_retries, 0);
    }
}
