use serde::{Deserialize, Serialize};

/// Repository-level Git integration settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default)]
    pub upstream: Option<String>,
    /// Whether Tauqe commits pass `--no-verify` to `git commit`, skipping the
    /// repository's `pre-commit` and `commit-msg` hooks.
    ///
    /// Defaults to `true`: an autonomous turn commits its own verified state
    /// and must not stall on user hooks. Set to `false` to let project hooks
    /// run on every Tauqe commit.
    #[serde(default = "default_bypass_hooks")]
    pub bypass_hooks: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            upstream: None,
            bypass_hooks: default_bypass_hooks(),
        }
    }
}

fn default_bypass_hooks() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::GitConfig;
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

    #[test]
    fn test_git_bypass_hooks_defaults_to_true_and_is_configurable() {
        let default_cfg: AppConfig = toml::from_str("[git]\n").expect("Failed to parse config");
        assert!(default_cfg.git.bypass_hooks);
        assert!(GitConfig::default().bypass_hooks);

        let toml_str = r#"
[git]
bypass_hooks = false
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert!(!config.git.bypass_hooks);
    }
}
