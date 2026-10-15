use serde::{Deserialize, Serialize};

/// Default wall-clock timeout for a single toolchain verification command.
///
/// Real projects routinely exceed one minute for `cargo test` or a full lint
/// pass, so the default budget is generous and overridable through
/// `[toolchain] timeout_secs`.
pub const DEFAULT_TOOLCHAIN_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolchainConfig {
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub max_retries: Option<usize>,
    #[serde(default)]
    pub auto_heal: Option<bool>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub auto_verify: Option<bool>,
    #[serde(default, alias = "default_target")]
    pub target: Option<String>,
}

impl ToolchainConfig {
    /// Effective per-command timeout in seconds.
    ///
    /// Falls back to [`DEFAULT_TOOLCHAIN_TIMEOUT_SECS`] when unset or set to a
    /// non-positive value, so malformed configuration degrades to a sane budget
    /// instead of aborting or blocking indefinitely.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_TOOLCHAIN_TIMEOUT_SECS)
    }

    /// Whether toolchain verification is automatically run after edits.
    ///
    /// Defaults to `true` so invariant checks and compiler verifications execute
    /// deterministically even if the model omitted `<verify>`.
    pub fn effective_auto_verify(&self) -> bool {
        self.auto_verify.unwrap_or(true)
    }

    /// Default verification target to run when the model does not specify one.
    ///
    /// Defaults to [`tauqe_protocol::VerifyTarget::All`].
    pub fn effective_target(&self) -> tauqe_protocol::VerifyTarget {
        match self
            .target
            .as_deref()
            .map(str::trim)
            .map(str::to_lowercase)
            .as_deref()
        {
            Some("check") => tauqe_protocol::VerifyTarget::Check,
            Some("clippy") => tauqe_protocol::VerifyTarget::Clippy,
            Some("test") => tauqe_protocol::VerifyTarget::Test,
            _ => tauqe_protocol::VerifyTarget::All,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toolchain_config_defaults() {
        let config = ToolchainConfig::default();
        assert!(config.effective_auto_verify());
        assert_eq!(config.effective_target(), tauqe_protocol::VerifyTarget::All);
        assert_eq!(config.effective_timeout_secs(), DEFAULT_TOOLCHAIN_TIMEOUT_SECS);
    }

    #[test]
    fn test_toolchain_config_custom_values() {
        let config: ToolchainConfig = toml::from_str(
            r#"
            auto_verify = false
            target = "check"
            timeout_secs = 30
        "#,
        )
        .unwrap();
        assert!(!config.effective_auto_verify());
        assert_eq!(config.effective_target(), tauqe_protocol::VerifyTarget::Check);
        assert_eq!(config.effective_timeout_secs(), 30);
    }

    #[test]
    fn test_toolchain_config_target_aliases_and_case() {
        let config: ToolchainConfig = toml::from_str(
            r#"
            default_target = "Clippy"
        "#,
        )
        .unwrap();
        assert_eq!(config.effective_target(), tauqe_protocol::VerifyTarget::Clippy);
    }
}
