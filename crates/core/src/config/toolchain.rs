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
}
