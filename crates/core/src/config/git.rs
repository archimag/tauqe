use serde::{Deserialize, Serialize};

/// Repository-level Git integration settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default)]
    pub upstream: Option<String>,
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    #[test]
    fn test_git_upstream_config() {
        let toml_str = r#"
[git]
upstream = "origin/master"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.git.upstream.as_deref(), Some("origin/master"));
    }
}
