//! Deterministic project toolchain verification and best-effort formatting.
//!
//! Verification commands are resolved from the typed `[toolchain.check]`,
//! `[toolchain.lint]`, and `[toolchain.test]` sections; a phase that names no
//! command falls back to deterministic project auto-detection, and the legacy
//! `check_command` remains the last resort. Formatters registered under
//! `[toolchain.fmt.<name>]` run best-effort over the changed files their
//! extension filter claims and never abort a verified turn.

mod commands;
mod exec;
mod fmt;
mod report;

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::edits::protocol::xml::VerifyTarget;

pub use commands::detect_toolchain;
pub use fmt::{run_formatters, FormatterRunResult};

/// The build and validation toolchain detected from the project files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolchainKind {
    Cargo,
    Npm,
    Go,
    Custom(String),
    None,
}

/// Outcome of a single toolchain command.
#[derive(Debug, Clone)]
pub struct ToolchainCheckResult {
    pub success: bool,
    pub is_cancelled: bool,
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub combined_output: String,
}

/// Runs the verification pipeline for `target`, stopping on the first failure.
///
/// The pipeline is resolved from the repository configuration: the typed phase
/// sections (`check`, `lint`, `test`) win, each phase keeps its own wall-clock
/// budget, and a phase that names no command falls back to auto-detection. The
/// configuration and the `package.json` manifest are read off the async executor
/// thread.
pub async fn run_verification_pipeline(
    repo_root: &Path,
    target: &VerifyTarget,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
    let steps =
        crate::blocking::run_blocking(|| commands::resolve_verification_plan(repo_root, target));
    let mut results = Vec::new();

    for step in steps {
        let result =
            exec::run_command(repo_root, &step.command, step.timeout_secs, cancel_rx).await;
        let success = result.success;
        results.push(result);
        if !success {
            return (false, results);
        }
    }

    (true, results)
}

/// Executes a toolchain verification command with the default wall-clock timeout.
pub async fn run_toolchain_check(repo_root: &Path, command_str: &str) -> ToolchainCheckResult {
    run_toolchain_check_with_timeout(
        repo_root,
        command_str,
        crate::config::DEFAULT_TOOLCHAIN_TIMEOUT_SECS,
        None,
    )
    .await
}

/// Executes a toolchain verification command in repo root with a wall-clock timeout and cancellation.
///
/// The command string is tokenized with shell-style quoting so arguments that
/// contain spaces survive intact. On Unix platforms, the child process is launched
/// in an isolated process group (`process_group(0)`) so that timeouts or client
/// cancellations terminate the entire process hierarchy (e.g. `rustc` and test binaries).
///
/// The wall-clock duration and outcome are recorded as a single structured log
/// record so verification timing is observable without duplicating orchestration.
pub async fn run_toolchain_check_with_timeout(
    repo_root: &Path,
    command_str: &str,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> ToolchainCheckResult {
    let command = exec::CommandLine::tokenized(command_str);
    exec::run_command(repo_root, &command, timeout_secs, cancel_rx).await
}
