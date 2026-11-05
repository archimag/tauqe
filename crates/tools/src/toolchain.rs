//! Deterministic project command execution.
//!
//! The harness carries no knowledge of a language, a compiler, or a build
//! system: every technology-specific operation is declared in `tauqe.toml` and
//! runs as an ordinary project process in the repository root. This module is
//! the substrate that executes those declarations. It works from the
//! `ProjectConfig` the wire contract defines and reads no configuration file of
//! its own, so resolving `tauqe.toml` into that value — and validating the file
//! itself — stays with the cognitive crate that owns the harness configuration.
//!
//! - `[project.verify]` is the mandatory verification lifecycle. Its command
//!   line runs in its own process group with a wall-clock budget, receives the
//!   turn's change set on standard input, and its output is parsed into typed
//!   diagnostics.
//! - `[project.format]` is the in-memory filter that rewrites staged content
//!   through standard input and output before any byte reaches disk.
//! - `[project.post_hook]` is the disk-side step that runs strictly after the
//!   change set is written and before verification. It receives the same change
//!   set on standard input as `[project.verify]`, so a generator rebuilds only
//!   the files the turn touched.
//!
//! Nothing is detected here: a repository either declares the commands it runs
//! or runs nothing at all.

mod commands;
mod diagnostics;
pub(crate) mod exec;
mod fmt;
mod report;
pub mod runner;

use std::collections::BTreeMap;
use std::path::Path;

use tauqe_protocol::{
    Diagnostic, ProjectConfig, ProjectHookOutcome, ProjectParameterSpec, ProjectPostHookResponse,
};
use tokio::sync::watch;

pub use fmt::{format_with_project_config, InMemoryFormatOutcome};
pub(crate) use fmt::{resolve_formatter, FormatterResolution};
pub use runner::merge_manifest;

/// Wall-clock budget of a single project command, in seconds.
///
/// Applied whenever the executed entry declares no `timeout_secs` of its own. A
/// real test suite routinely needs more than a minute, which is why the fallback
/// is generous.
pub const DEFAULT_PROJECT_TIMEOUT_SECS: u64 = 600;

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

/// Runs the declared verification lifecycle under an already resolved project
/// configuration, stopping at the first failure.
///
/// `[project.verify]` is the single authority on how the repository verifies, so
/// its command line is the whole pipeline: the harness runs it once with its own
/// wall-clock budget and takes the exit status as the verdict. A repository that
/// declares no verification command resolves no step at all and reports a clean
/// empty run instead of running an unintended program. A failing command's
/// output is parsed into typed diagnostics, so a healing round reasons about
/// locations instead of a transcript.
///
/// The turn's authoritative change set reaches the command on its standard
/// input, one path per line, so a project dispatcher selects the checks from the
/// files the turn actually touched. The command never derives that list itself:
/// while the transaction is open the working copy also holds post-hook artifacts
/// and the developer's uncommitted work. An empty change set sends no input at
/// all, so the command conservatively runs its full suite.
///
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so no verification round re-reads `tauqe.toml` or runs a declared task
/// runner's dump again. The integrity of that file is the harness's own concern
/// and is checked before this function runs, so only the commands a sound
/// declaration resolves ever reach a process here.
pub async fn run_verification_pipeline_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
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

/// Runs the disk-side post-hook declared by `[project.post_hook]` under an
/// already resolved project configuration.
///
/// The hook executes strictly after the change set reached disk and before
/// verification, so generated sources and refreshed manifests are present when
/// the verification command runs. The turn's change set reaches the command on
/// its standard input, one path per line, exactly as the verification lifecycle
/// receives it, so a generator rebuilds only the files the turn touched. An
/// empty change set sends no input at all, so the hook conservatively rebuilds
/// everything. The harness still captures whatever the hook produced through
/// Git, so the command stays a plain project process. A project that declares no
/// hook runs nothing and succeeds.
///
/// A bounded turn passes the configuration it froze on its first proposal round,
/// so the hook is never preceded by a second task runner dump.
pub async fn run_post_hooks_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    changed_files: &[String],
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
    let change_set = change_set_payload(changed_files);
    let outcome = exec::run_command(
        repo_root,
        &command,
        timeout_secs,
        change_set.as_deref(),
        cancel_rx,
    )
    .await;

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

/// Runs one project command the model may address by name, under an already
/// resolved project configuration.
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
///
/// A bounded turn passes the configuration it froze on its first proposal round,
/// so an invocation validates its declaration against one snapshot instead of
/// re-reading `tauqe.toml` and re-running a task runner dump.
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

    #[test]
    fn test_change_set_payload_renders_one_path_per_line() {
        assert_eq!(change_set_payload(&[]), None);
        assert_eq!(
            change_set_payload(&["src/lib.rs".to_string(), "generated.rs".to_string()]),
            Some("src/lib.rs\ngenerated.rs\n".to_string())
        );
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

    #[tokio::test]
    async fn test_a_configuration_that_declares_no_verification_runs_no_step() {
        let dir = tempfile::tempdir().unwrap();
        let project = ProjectConfig::default();

        let (ok, results) =
            run_verification_pipeline_with_config(dir.path(), &project, &[], None).await;

        assert!(ok, "a configuration that declares nothing verifies cleanly");
        assert!(results.is_empty(), "no command may be invented");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_declared_verification_runs_under_the_supplied_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let project: ProjectConfig =
            serde_json::from_str(r#"{"verify":{"command":"sh","args":["-c","exit 0"]}}"#).unwrap();

        let (ok, results) =
            run_verification_pipeline_with_config(dir.path(), &project, &[], None).await;

        assert!(ok);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    /// The post-hook receives the turn's change set on its standard input, so a
    /// generator selects the files the turn touched instead of scanning the
    /// whole repository.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_the_post_hook_receives_the_change_set_on_standard_input() {
        let dir = tempfile::tempdir().unwrap();
        let project: ProjectConfig = serde_json::from_str(
            r#"{"post_hook":{"command":"sh","args":["-c","cat > received.txt"]}}"#,
        )
        .unwrap();

        let changed = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        let response = run_post_hooks_with_config(dir.path(), &project, &changed, None)
            .await
            .expect("the declared hook runs");

        assert!(response.success, "hook failed: {:?}", response.message);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("received.txt")).unwrap(),
            "src/lib.rs\nsrc/main.rs\n",
            "the hook receives one relative path per line"
        );

        // An empty change set sends no input at all, so a generator
        // conservatively rebuilds everything.
        run_post_hooks_with_config(dir.path(), &project, &[], None)
            .await
            .expect("the declared hook runs");
        assert!(
            std::fs::read_to_string(dir.path().join("received.txt"))
                .unwrap()
                .is_empty(),
            "an empty change set sends no input"
        );
    }
}
