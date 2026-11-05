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
    /// Wall-clock budget, in seconds, of every Git command the harness runs
    /// against this repository.
    ///
    /// A Git child is never only Git: a commit that runs the repository hooks
    /// executes arbitrary user code, and a `commit.gpgsign` configuration waits
    /// for a signing agent that can block on a passphrase prompt. The budget
    /// bounds such a child together with every descendant it spawned, so a
    /// wedged hook cannot occupy a turn indefinitely. Defaults to 300 seconds,
    /// the substrate budget of a Git invocation that names none of its own.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            upstream: None,
            bypass_hooks: default_bypass_hooks(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

fn default_bypass_hooks() -> bool {
    true
}

fn default_timeout_secs() -> u64 {
    tauqe_tools::git::exec::DEFAULT_GIT_TIMEOUT_SECS
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

    #[test]
    fn test_git_timeout_secs_defaults_to_the_substrate_budget_and_is_configurable() {
        let expected = tauqe_tools::git::exec::DEFAULT_GIT_TIMEOUT_SECS;
        let default_cfg: AppConfig = toml::from_str("[git]\n").expect("Failed to parse config");
        assert_eq!(default_cfg.git.timeout_secs, expected);
        assert_eq!(default_cfg.git.timeout_secs, 300);
        assert_eq!(GitConfig::default().timeout_secs, 300);

        let toml_str = r#"
[git]
timeout_secs = 45
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.git.timeout_secs, 45);
    }
}
