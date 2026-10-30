//! Deterministic project command execution.
//!
//! The harness carries no knowledge of a language, a compiler, or a build
//! system: every technology-specific operation is declared in `tauqe.toml` and
//! runs as an ordinary project process in the repository root.
//!
//! - `[project.verify]` is the mandatory verification lifecycle. Its command
//!   line runs in its own process group with a wall-clock budget, receives the
//!   turn's change set on standard input, and its output is parsed into typed
//!   diagnostics.
//! - `[project.format]` is the in-memory filter that rewrites staged content
//!   through standard input and output before any byte reaches disk. When the
//!   entry is omitted, the `format` recipe the declared task runner exports
//!   fills the role; a declaration always keeps precedence over that export.
//! - `[project.post_hook]` is the disk-side step that runs strictly after the
//!   change set is written and before verification.
//!
//! Nothing is detected here: a repository either declares the commands it runs
//! or runs nothing at all.

mod commands;
mod diagnostics;
mod exec;
mod fmt;
mod report;
pub mod runner;

use std::collections::BTreeMap;
use std::path::Path;

use tauqe_protocol::{
    Diagnostic, ProjectConfig, ProjectHookOutcome, ProjectParameterSpec, ProjectPostHookResponse,
};
use tokio::sync::watch;

pub(crate) use fmt::format_with_project_config;
pub use fmt::{format_staged_content, InMemoryFormatOutcome};
pub use runner::merge_manifest;

/// Wall-clock budget of a single project command, in seconds.
///
/// Applied whenever the executed entry declares no `timeout_secs` of its own. A
/// real test suite routinely needs more than a minute, which is why the fallback
/// is generous.
pub const DEFAULT_PROJECT_TIMEOUT_SECS: u64 = 600;

/// Name of the configuration file whose integrity the verification pipeline
/// checks before any declared project command runs.
const PROJECT_CONFIG_FILE: &str = "tauqe.toml";

/// Outcome of a single project command.
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

/// Resolves the effective project configuration, merging any declared task runner.
///
/// If a task runner is declared in `[project]`, its exported manifest is loaded
/// and merged into the declared configuration. Declared entries in `tauqe.toml`
/// always take precedence over runner recipes, and a runner that is missing,
/// fails, or answers with an unreadable dump leaves the declared configuration
/// untouched instead of contributing a partially trusted command list.
pub async fn effective_project_config(
    repo_root: &Path,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> ProjectConfig {
    let root = repo_root.to_path_buf();
    // Reading `tauqe.toml` is blocking filesystem work, so it runs off the async
    // executor thread.
    let mut project = crate::blocking::run_blocking(|| {
        crate::config::load_section::<ProjectConfig>(root.as_path(), "project")
    });

    let Some(runner) = runner::task_runner(project.runner) else {
        return project;
    };

    match runner::load_manifest(runner, repo_root, DEFAULT_PROJECT_TIMEOUT_SECS, cancel_rx).await {
        Ok(manifest) => runner::merge_manifest(&mut project, manifest),
        Err(err) => {
            tracing::warn!(
                runner = %project.runner,
                reason = %err,
                "failed to load task runner manifest; using declared project configuration"
            );
        }
    }

    project
}

/// Resolves the effective project configuration and runs the declared
/// verification lifecycle, stopping at the first failure.
///
/// This is the entry point for callers outside a bounded turn. A turn holds the
/// configuration it froze on its first proposal round and calls
/// [`run_verification_pipeline_with_config`], so a declared task runner's dump
/// runs once for the whole turn instead of once per verification round.
pub async fn run_verification_pipeline(
    repo_root: &Path,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
    let project = effective_project_config(repo_root, cancel_rx).await;
    run_verification_pipeline_with_config(repo_root, &project, changed_files, cancel_rx).await
}

/// Runs the declared verification lifecycle under an already resolved project
/// configuration, stopping at the first failure.
///
/// The pipeline opens with phase zero: `tauqe.toml` itself is validated on
/// disk, because it is the declaration every later step obeys. A change set or
/// a post-hook that leaves the file malformed or semantically unsound fails the
/// verification immediately, and no declared command is spawned against an
/// environment the harness can no longer trust.
///
/// `[project.verify]` is the single authority on how the repository verifies, so
/// its command line is the whole pipeline: the harness runs it once with its own
/// wall-clock budget and takes the exit status as the verdict. A repository that
/// declares no verification command resolves no step at all and reports a clean
/// empty run instead of running an unintended program. A failing command's
/// output is parsed into typed diagnostics, so a healing round reasons about
/// locations instead of a transcript.
/// The turn's authoritative change set reaches the command on its standard
/// input, one path per line, so a project dispatcher selects the checks from the
/// files the turn actually touched. The command never derives that list itself:
/// while the transaction is open the working copy also holds post-hook artifacts
/// and the developer's uncommitted work. An empty change set sends no input at
/// all, so the command conservatively runs its full suite.
///
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so no verification round re-reads `tauqe.toml` or runs a declared task
/// runner's dump again.
pub async fn run_verification_pipeline_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
    let config_check = check_project_config(repo_root);
    if !config_check.success {
        return (false, vec![config_check]);
    }

    let steps = commands::resolve_verification_plan(project);
    let mut results = Vec::new();
    let change_set = change_set_payload(changed_files);

    for step in steps {
        let mut result = exec::run_command(
            repo_root,
            &step.command,
            step.timeout_secs,
            change_set.as_deref(),
            cancel_rx,
        )
        .await;
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

/// Renders the turn's authoritative change set as a command's standard input.
///
/// One repository-relative path per line, so a project dispatcher reads the list
/// with a plain line loop. An empty change set produces no input at all: the
/// command then runs its full suite instead of an empty selection.
fn change_set_payload(changed_files: &[String]) -> Option<String> {
    if changed_files.is_empty() {
        return None;
    }
    let mut payload = changed_files.join("\n");
    payload.push('\n');
    Some(payload)
}

/// Validates the project configuration file as the zeroth verification step.
///
/// `tauqe.toml` governs the environment the turn runs in: the model roles, the
/// budgets, and the command lines the harness may execute. A model edit or a
/// post-hook that corrupts the file leaves the project without a trustworthy
/// environment, so the pipeline reports the parses and semantic verdict as the
/// change set's verification result and never spawns a declared command. A
/// repository without a configuration file is governed by built-in defaults and
/// therefore passes this check.
fn check_project_config(repo_root: &Path) -> ToolchainCheckResult {
    let verdict = crate::config::validate_project_config_on_disk(repo_root);
    ToolchainCheckResult {
        success: verdict.is_ok(),
        is_cancelled: false,
        command: PROJECT_CONFIG_FILE.to_string(),
        stdout: String::new(),
        stderr: String::new(),
        combined_output: verdict.err().unwrap_or_default(),
        diagnostics: Vec::new(),
    }
}

/// Executes a project command with the default wall-clock timeout.
pub async fn run_toolchain_check(repo_root: &Path, command_str: &str) -> ToolchainCheckResult {
    run_toolchain_check_with_timeout(repo_root, command_str, DEFAULT_PROJECT_TIMEOUT_SECS, None)
        .await
}

/// Executes a project command in repo root with a wall-clock timeout and cancellation.
///
/// The command string is tokenized with shell-style quoting so arguments that
/// contain spaces survive intact. On Unix platforms, the child process is launched
/// in an isolated process group (`process_group(0)`) so that timeouts or client
/// cancellations terminate the entire process hierarchy (e.g. `rustc` and test binaries).
///
/// The wall-clock duration and outcome are recorded as a single structured log
/// record so execution timing is observable without duplicating orchestration.
pub async fn run_toolchain_check_with_timeout(
    repo_root: &Path,
    command_str: &str,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> ToolchainCheckResult {
    let command = exec::CommandLine::tokenized(command_str);
    exec::run_command(repo_root, &command, timeout_secs, None, cancel_rx).await
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
    let project = effective_project_config(repo_root, cancel_rx).await;
    run_post_hooks_with_config(repo_root, &project, cancel_rx).await
}

/// Runs the disk-side post-hook declared by `[project.post_hook]` under an
/// already resolved project configuration.
///
/// A bounded turn passes the configuration it froze on its first proposal
/// round, so the hook is never preceded by a second task runner dump.
pub async fn run_post_hooks_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ProjectPostHookResponse, String> {
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
        .unwrap_or(DEFAULT_PROJECT_TIMEOUT_SECS);
    let command = exec::CommandLine::new(resolved.command, resolved.args);
    let outcome = exec::run_command(repo_root, &command, timeout_secs, None, cancel_rx).await;

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
/// process is spawned. The declaration also owns the parameter contract: an
/// invocation supplies named values, which are checked against the declared
/// parameters, filled from their defaults, and appended to the command line in
/// declaration order. No value is ever split, expanded, or interpreted, so the
/// process still receives literal arguments and no shell ever sees them. The
/// command runs in its own process group, so a timeout or a client cancellation
/// tears down the whole descendant tree.
pub async fn run_project_command(
    repo_root: &Path,
    name: &str,
    arguments: &BTreeMap<String, String>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ToolchainCheckResult, String> {
    let project = effective_project_config(repo_root, cancel_rx).await;
    run_project_command_with_config(repo_root, &project, name, arguments, cancel_rx).await
}

/// Runs one project command the model may address by name, under an already
/// resolved project configuration.
///
/// A bounded turn passes the configuration it froze on its first proposal
/// round, so an invocation validates its declaration against one snapshot
/// instead of re-reading `tauqe.toml` and re-running a task runner dump.
pub async fn run_project_command_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    name: &str,
    arguments: &BTreeMap<String, String>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ToolchainCheckResult, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the invocation names no project command".to_string());
    }

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
        .unwrap_or(DEFAULT_PROJECT_TIMEOUT_SECS);

    let mut args = resolved.args;
    args.extend(bind_parameters(name, &resolved.parameters, arguments)?);
    let command = exec::CommandLine::new(resolved.command, args);
    Ok(exec::run_command(repo_root, &command, timeout_secs, None, cancel_rx).await)
}

/// Binds one invocation onto the declared parameter contract.
///
/// The declaration is the whole contract: a name it does not carry is refused
/// instead of being forwarded to a process, and a declared parameter that the
/// caller left out takes its default. A parameter with neither a supplied value
/// nor a default cannot be bound, so it is reported instead of silently shifting
/// the positional arguments that follow it. The result keeps declaration order,
/// which is exactly how a task runner binds positional arguments, and every value
/// stays one literal argument: it is never split, expanded, or interpreted.
fn bind_parameters(
    name: &str,
    parameters: &[ProjectParameterSpec],
    arguments: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    for supplied in arguments.keys() {
        if !parameters
            .iter()
            .any(|parameter| parameter.name == *supplied)
        {
            let declared = parameters
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "the command `{name}` declares no parameter `{supplied}`; declared parameters: [{declared}]"
            ));
        }
    }

    let mut bound = Vec::with_capacity(parameters.len());
    for parameter in parameters {
        let value = match arguments.get(&parameter.name) {
            Some(value) => value.clone(),
            None => match parameter.default.as_deref() {
                Some(default) => default.to_string(),
                None => {
                    return Err(format!(
                        "the command `{name}` requires the parameter `{}`; supply it as a named argument",
                        parameter.name
                    ));
                }
            },
        };
        if value.contains('\0') {
            return Err(format!(
                "the parameter `{}` carries a value that cannot reach a process",
                parameter.name
            ));
        }
        bound.push(value);
    }
    Ok(bound)
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
    async fn test_a_corrupted_project_configuration_fails_phase_zero() {
        // The file parses as TOML but declares no verification command, so the
        // environment itself is unsound: the pipeline must report that verdict
        // instead of resolving a project step from it.
        let dir = repo(Some("[models]\nprimary = [\"openrouter:a/b\"]\n"));

        let (ok, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert!(!ok, "a corrupted configuration fails verification");
        assert_eq!(results.len(), 1, "only the phase-zero verdict is reported");
        assert!(!results[0].success);
        assert_eq!(results[0].command, PROJECT_CONFIG_FILE);
        assert!(
            results[0].combined_output.contains("verify"),
            "the verdict names the broken invariant: {}",
            results[0].combined_output
        );
    }

    #[tokio::test]
    async fn test_a_valid_project_configuration_keeps_the_project_pipeline() {
        let dir = repo(Some(
            "[models]\nprimary = [\"openrouter:a/b\"]\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'exit 0']\n",
        ));

        let (ok, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert!(ok);
        assert_eq!(
            results.len(),
            1,
            "phase zero contributes no verdict on success"
        );
        assert!(results[0].success);
        assert_ne!(
            results[0].command, PROJECT_CONFIG_FILE,
            "the declared project command ran instead of the config check"
        );
    }

    #[tokio::test]
    async fn test_a_project_without_a_verification_declaration_runs_no_step() {
        let dir = repo(None);

        let (ok, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert!(ok, "a repository that declares nothing verifies cleanly");
        assert!(results.is_empty(), "no command may be invented");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_project_command_binds_declared_parameters_and_defaults() {
        let dir = repo(Some(
            "[project.commands.greet]\ncommand = \"printf\"\nargs = [\"[%s] [%s]\"]\n\
             parameters = [{ name = \"who\" }, { name = \"count\", default = \"1\" }]\n",
        ));
        let arguments = BTreeMap::from([("who".to_string(), "tauqe".to_string())]);

        let result = run_project_command(dir.path(), "greet", &arguments, None)
            .await
            .expect("the declared parameters bind");

        assert!(result.success, "command failed: {}", result.combined_output);
        // Declaration order decides the positional arguments, and the omitted
        // parameter arrives as its declared default.
        assert_eq!(result.stdout, "[tauqe] [1]");
    }

    #[tokio::test]
    async fn test_project_command_rejects_an_undeclared_parameter() {
        let dir = repo(Some(
            "[project.commands.greet]\ncommand = \"printf\"\nargs = [\"hi\"]\n\
             parameters = [{ name = \"who\" }]\n",
        ));
        let arguments = BTreeMap::from([("path".to_string(), "../escape".to_string())]);

        let err = run_project_command(dir.path(), "greet", &arguments, None)
            .await
            .expect_err("an undeclared parameter never reaches a process");

        assert!(err.contains("declares no parameter `path`"), "error: {err}");
        assert!(err.contains("who"), "error names the declaration: {err}");
    }

    #[tokio::test]
    async fn test_project_command_requires_a_parameter_without_a_default() {
        let dir = repo(Some(
            "[project.commands.greet]\ncommand = \"printf\"\nparameters = [{ name = \"who\" }]\n",
        ));

        let err = run_project_command(dir.path(), "greet", &BTreeMap::new(), None)
            .await
            .expect_err("a required parameter cannot be invented");

        assert!(err.contains("requires the parameter `who`"), "error: {err}");
    }

    #[test]
    fn test_bind_parameters_rejects_a_value_that_cannot_reach_a_process() {
        let parameters = [ProjectParameterSpec {
            name: "n".to_string(),
            default: None,
            doc: None,
        }];
        let arguments = BTreeMap::from([("n".to_string(), "a\u{0}b".to_string())]);

        let err = bind_parameters("log-tail", &parameters, &arguments)
            .expect_err("a NUL byte never reaches a process");

        assert!(err.contains("cannot reach a process"), "error: {err}");
    }

    #[test]
    fn test_change_set_payload_renders_one_path_per_line() {
        assert_eq!(change_set_payload(&[]), None);
        assert_eq!(
            change_set_payload(&["src/lib.rs".to_string(), "generated.rs".to_string()]),
            Some("src/lib.rs\ngenerated.rs\n".to_string())
        );
    }
}
