//! Deterministic project toolchain execution.
//!
//! The harness carries no knowledge of a language, a compiler, or a build
//! system: every technology-specific operation is declared in `tauqe.toml` and
//! runs as an ordinary project process in the repository root.
//!
//! - `[project.verify]` is the mandatory verification lifecycle. Its command
//!   line runs in its own process group with a wall-clock budget, and its output
//!   is parsed into typed diagnostics.
//! - `[project.format]` is the in-memory filter that rewrites staged content
//!   through standard input and output before any byte reaches disk.
//! - `[project.post_hook]` is the disk-side step that runs strictly after the
//!   change set is written and before verification.
//!
//! Legacy `[toolchain]` settings remain harness policy: budgets, the auto-verify
//! gate, and the deterministic resolution an unmigrated repository keeps.

mod commands;
mod diagnostics;
mod exec;
mod fmt;
mod report;
pub mod runner;

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauqe_protocol::{Diagnostic, ProjectConfig, ProjectHookOutcome, ProjectPostHookResponse};
use tokio::sync::watch;

use crate::edits::protocol::xml::VerifyTarget;

pub use commands::detect_toolchain;
pub use fmt::{format_staged_content, run_formatters, FormatterRunResult, InMemoryFormatOutcome};
pub use runner::merge_manifest;

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
    /// Findings parsed from the command output, empty when the command reported
    /// nothing addressable.
    pub diagnostics: Vec<Diagnostic>,
}

/// Runs the project verification lifecycle for `target`, stopping on the first failure.
///
/// `[project.verify]` is authoritative, so it runs for every target; a
/// repository that declares no verification command keeps the legacy
/// deterministic resolution. The configuration is read off the async executor
/// thread, and the output of a failing command is parsed into typed diagnostics
/// so a healing round reasons about locations instead of a transcript.
/// Resolves the effective project configuration, merging any declared task runner.
///
/// If a task runner is declared (e.g. `runner = "just"`), its exported manifest
/// is loaded and merged into the configuration from `tauqe.toml`. Declared entries
/// in `tauqe.toml` always take precedence over runner recipes.
pub async fn effective_project_config(
    repo_root: &Path,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (ProjectConfig, crate::config::ToolchainConfig) {
    let root = repo_root.to_path_buf();
    let (mut project, config) = crate::blocking::run_blocking(|| {
        let project = crate::config::load_section::<ProjectConfig>(root.as_path(), "project");
        let config = crate::config::load_toolchain_config(root.as_path());
        (project, config)
    });

    if let Some(runner) = runner::task_runner(project.runner) {
        let timeout_secs = config.effective_timeout_secs();
        match runner::load_manifest(runner, repo_root, timeout_secs, cancel_rx).await {
            Ok(manifest) => {
                runner::merge_manifest(&mut project, manifest);
            }
            Err(err) => {
                tracing::warn!(
                    runner = %project.runner,
                    reason = %err,
                    "failed to load task runner manifest; using declared project configuration"
                );
            }
        }
    }

    (project, config)
}

pub async fn run_verification_pipeline(
    repo_root: &Path,
    target: &VerifyTarget,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
    let (project, config) = effective_project_config(repo_root, cancel_rx).await;
    let steps =
        commands::resolve_verification_plan_with_config(repo_root, target, &project, &config);
    let mut results = Vec::new();

    for step in steps {
        let mut result =
            exec::run_command(repo_root, &step.command, step.timeout_secs, cancel_rx).await;
        if !result.success && !result.is_cancelled {
            result.diagnostics = diagnostics::parse_diagnostics(&result.combined_output);
        }
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

/// Runs the disk-side post-hook declared by `[project.post_hook]`.
///
/// The hook executes strictly after the change set reached disk and before
/// verification, so generated sources and refreshed manifests are present when
/// the verification command runs. It receives no file list: the harness captures
/// whatever the hook produced through Git, so the command stays a plain project
/// process. A repository that declares no hook runs nothing and succeeds.
pub async fn run_post_hooks(
    repo_root: &Path,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ProjectPostHookResponse, String> {
    let (project, config) = crate::blocking::run_blocking(|| {
        (
            crate::config::load_section::<ProjectConfig>(repo_root, "project"),
            crate::config::load_toolchain_config(repo_root),
        )
    });

    let Some(spec) = project.post_hook.as_ref() else {
        return Ok(ProjectPostHookResponse {
            success: true,
            executed_hooks: Vec::new(),
            message: None,
        });
    };

    let resolved = project
        .resolve("post_hook", spec)
        .map_err(|err| err.to_string())?;
    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or_else(|| config.effective_timeout_secs());
    let command = exec::CommandLine::new(resolved.command, resolved.args);
    let outcome = exec::run_command(repo_root, &command, timeout_secs, cancel_rx).await;

    if outcome.is_cancelled {
        return Err("the project post-hook was cancelled".to_string());
    }

    let message = if outcome.success {
        None
    } else {
        Some(compact_failure(&outcome))
    };
    Ok(ProjectPostHookResponse {
        success: outcome.success,
        executed_hooks: vec![ProjectHookOutcome {
            name: "post_hook".to_string(),
            success: outcome.success,
            message: message.clone(),
        }],
        message,
    })
}

/// Runs one project command the model may address by name.
///
/// The `[project.commands]` declaration is the only authority on what may run,
/// so the name must be declared and its reference chain must resolve before a
/// process is spawned. A declaration is a closed command line: it names its own
/// literal arguments, so an invocation adds none and no shell ever interprets
/// them. The command runs in its own process group, so a timeout or a client
/// cancellation tears down the whole descendant tree.
pub async fn run_project_command(
    repo_root: &Path,
    name: &str,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ToolchainCheckResult, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the invocation names no project command".to_string());
    }

    // Reading `tauqe.toml` is blocking filesystem work, so it runs off the async
    // executor thread.
    let (project, config) = effective_project_config(repo_root, cancel_rx).await;

    let Some(spec) = project.commands.get(name) else {
        return Err(format!(
            "the project declares no command named `{}`; invoke one of: [{}]",
            name,
            project
                .commands
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };

    let resolved = project
        .resolve(&format!("commands.{}", name), spec)
        .map_err(|err| err.to_string())?;
    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or_else(|| config.effective_timeout_secs());

    let command = exec::CommandLine::new(resolved.command, resolved.args);
    Ok(exec::run_command(repo_root, &command, timeout_secs, cancel_rx).await)
}

/// Collapses a failed command into one compact report line.
fn compact_failure(outcome: &ToolchainCheckResult) -> String {
    let output = if outcome.combined_output.trim().is_empty() {
        outcome.stderr.trim()
    } else {
        outcome.combined_output.trim()
    };
    let collapsed = output.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.is_empty() {
        format!("`{}` exited with a failure status", outcome.command)
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a temporary repository with an optional `tauqe.toml`.
    fn repo(config: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn test_post_hook_runs_nothing_without_a_declaration() {
        let dir = repo(None);

        let response = run_post_hooks(dir.path(), None).await.unwrap();

        assert!(response.success);
        assert!(response.executed_hooks.is_empty());
        assert!(response.message.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_post_hook_reports_a_failed_command() {
        let dir = repo(Some(
            "[project.post_hook]\ncommand = \"sh\"\nargs = ['-c', 'printf \"missing input\" >&2; exit 2']\n",
        ));

        let response = run_post_hooks(dir.path(), None).await.unwrap();

        assert!(!response.success);
        assert_eq!(response.executed_hooks.len(), 1);
        assert!(!response.executed_hooks[0].success);
        assert!(response
            .message
            .unwrap_or_default()
            .contains("missing input"));
    }

    #[tokio::test]
    async fn test_post_hook_rejects_a_broken_declaration() {
        let dir = repo(Some("[project.post_hook]\ncommand = \"@missing\"\n"));

        let err = run_post_hooks(dir.path(), None).await.unwrap_err();

        assert!(err.contains("missing"), "unexpected reason: {err}");
    }
}
