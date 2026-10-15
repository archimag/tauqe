use serde::{Deserialize, Serialize};
use tauqe_protocol::{EditProtocolKind, WorkflowKind};

/// Development profile: workflow engine, edit protocol and retry policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevelopConfig {
    #[serde(default)]
    pub workflow: WorkflowKind,
    #[serde(default)]
    pub protocol: EditProtocolKind,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_replans")]
    pub max_replans: usize,
}

/// Backwards-compatible alias for the legacy `[edit]` configuration section.
pub type EditConfig = DevelopConfig;

fn default_max_retries() -> usize {
    3
}

fn default_max_replans() -> usize {
    2
}

impl Default for DevelopConfig {
    fn default() -> Self {
        Self {
            workflow: WorkflowKind::default(),
            protocol: EditProtocolKind::default(),
            max_retries: default_max_retries(),
            max_replans: default_max_replans(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn test_structured_protocol_config() {
        let toml_str = r#"
[develop]
workflow = "git"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, WorkflowKind::Git);
        assert_eq!(config.develop.protocol, EditProtocolKind::Structured);
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
        assert_eq!(config.develop.workflow, WorkflowKind::Naive);
        assert_eq!(config.develop.protocol, EditProtocolKind::Xml);
        assert_eq!(config.develop.max_retries, 1);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.develop.max_retries, 0);
    }

    #[test]
    fn test_develop_max_replans_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_replans = 4\n").unwrap();
        assert_eq!(config.develop.max_replans, 4);
    }
}
