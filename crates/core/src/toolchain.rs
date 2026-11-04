//! Project environment resolution for the deterministic command substrate.
//!
//! Command execution is deterministic substrate and lives in
//! `tauqe_tools::toolchain`: it runs from the `ProjectConfig` of the wire
//! contract, reads no configuration file of its own, and never learns about the
//! harness configuration.
//!
//! This module owns the one step that does need the harness. The `[project]`
//! section is declared inside `tauqe.toml`, the file that also carries the model
//! roles and the budgets, so resolving the declared commands into a
//! `ProjectConfig`, and validating the file itself before any declared command
//! runs, belongs here. Every entry point that starts from the repository is a
//! thin wrapper that resolves the configuration once and delegates; a bounded
//! turn that already froze its configuration calls the `*_with_config` variants
//! directly.

use std::collections::BTreeMap;
use std::path::Path;

use tauqe_protocol::{ProjectConfig, ProjectPostHookResponse, VerificationVerdict};
use tokio::sync::watch;

pub use tauqe_tools::toolchain::{
    format_with_project_config, merge_manifest, run_post_hooks_with_config,
    run_project_command_with_config, run_toolchain_check, run_toolchain_check_with_timeout, runner,
    InMemoryFormatOutcome, ToolchainCheckResult, DEFAULT_PROJECT_TIMEOUT_SECS,
};

/// Name of the configuration file whose integrity the verification pipeline
/// checks before any declared project command runs.
const PROJECT_CONFIG_FILE: &str = "tauqe.toml";

/// Explicit verdict of a project verification run.
///
/// The verdict is a status rather than a boolean, because a passed gate and an
/// absent declaration are different facts about a change set: a repository that
/// declares no verification lifecycle produces
/// [`VerificationStatus::NotConfigured`], and its change set may still be
/// committed while nothing claims that it was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    /// The project declares a verification lifecycle and its command passed.
    Passed,
    /// The project declares a verification lifecycle and its command failed.
    Failed,
    /// The project declares no verification lifecycle, so nothing ran.
    NotConfigured,
    /// The run was interrupted by the turn's cancellation signal.
    Cancelled,
}

impl VerificationStatus {
    /// Whether the change set may be committed under this verdict.
    ///
    /// A project that declares no verification lifecycle states its own choice,
    /// so an unverified change set is not blocked by a gate the project never
    /// declared.
    pub fn allows_commit(self) -> bool {
        matches!(self, Self::Passed | Self::NotConfigured)
    }

    /// Whether the project declared no verification lifecycle at all.
    pub fn is_not_configured(self) -> bool {
        self == Self::NotConfigured
    }
}

impl std::fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::NotConfigured => "not_configured",
            Self::Cancelled => "cancelled",
        };
        f.write_str(name)
    }
}

/// Maps the engine verdict onto the wire verdict of a change set.
///
/// Both vocabularies describe the same four outcomes, so the conversion is
/// declared once beside the engine status instead of being restated by every
/// caller that reports a change set.
impl From<VerificationStatus> for VerificationVerdict {
    fn from(status: VerificationStatus) -> Self {
        match status {
            VerificationStatus::Passed => Self::Passed,
            VerificationStatus::Failed => Self::Failed,
            VerificationStatus::NotConfigured => Self::NotConfigured,
            VerificationStatus::Cancelled => Self::Cancelled,
        }
    }
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
) -> (VerificationStatus, Vec<ToolchainCheckResult>) {
    let project = effective_project_config(repo_root, cancel_rx).await;
    run_verification_pipeline_with_config(repo_root, &project, changed_files, cancel_rx).await
}

/// Runs the declared verification lifecycle under an already resolved project
/// configuration, stopping at the first failure.
///
/// The pipeline opens with phase zero: `tauqe.toml` itself is validated on disk,
/// because it is the declaration every later step obeys. A change set or a
/// post-hook that leaves the file malformed or semantically unsound fails the
/// verification immediately, and no declared command is spawned against an
/// environment the harness can no longer trust. The verdict belongs here rather
/// than in the substrate, because the file also carries the model roles and the
/// budgets the cognitive crate owns.
///
/// The declared command lines themselves run in `tauqe_tools::toolchain`, and
/// their outcome is folded into an explicit [`VerificationStatus`]: a project
/// that resolves no step reports [`VerificationStatus::NotConfigured`] instead
/// of a fabricated success.
pub async fn run_verification_pipeline_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (VerificationStatus, Vec<ToolchainCheckResult>) {
    let config_check = check_project_config(repo_root);
    if !config_check.success {
        return (VerificationStatus::Failed, vec![config_check]);
    }

    let (passed, results) = tauqe_tools::toolchain::run_verification_pipeline_with_config(
        repo_root,
        project,
        changed_files,
        cancel_rx,
    )
    .await;
    let status = classify_status(passed, &results);
    (status, results)
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

/// Folds a substrate run into the explicit verdict of the pipeline.
///
/// Cancellation wins over every other outcome, because an interrupted run is a
/// verdict of its own rather than a failed gate. A successful run that resolved
/// no command means the project declared no lifecycle, which is exactly
/// [`VerificationStatus::NotConfigured`].
fn classify_status(passed: bool, results: &[ToolchainCheckResult]) -> VerificationStatus {
    if results.iter().any(|result| result.is_cancelled) {
        return VerificationStatus::Cancelled;
    }
    if !passed {
        return VerificationStatus::Failed;
    }
    if results.is_empty() {
        VerificationStatus::NotConfigured
    } else {
        VerificationStatus::Passed
    }
}

/// Runs the disk-side post-hook declared by `[project.post_hook]`.
///
/// The hook executes strictly after the change set reached disk and before
/// verification, so generated sources and refreshed manifests are present when
/// the verification command runs. The turn's change set reaches the command on
/// its standard input, one path per line, exactly as the verification lifecycle
/// receives it. The caller supplies that change set, so the wrapper reaches the
/// hook with exactly the paths the current round wrote; an empty slice sends no
/// input at all, so the hook conservatively rebuilds everything. The harness
/// still captures whatever the hook produced through Git, so the command stays a
/// plain project process. A repository that declares no hook runs nothing and
/// succeeds.
pub async fn run_post_hooks(
    repo_root: &Path,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<ProjectPostHookResponse, String> {
    let project = effective_project_config(repo_root, cancel_rx).await;
    run_post_hooks_with_config(repo_root, &project, changed_files, cancel_rx).await
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

/// Streams `content` through the in-memory formatter of the project.
///
/// The filter comes from the effective project configuration: a declared
/// `[project.format]` entry, or the `format` recipe the declared task runner
/// exports when the entry is omitted. A declaration always keeps precedence over
/// the export of the same role, so a project can pin the filter in `tauqe.toml`
/// or delegate it to its justfile without changing the bounded turn.
pub async fn format_staged_content(
    repo_root: &Path,
    content: &str,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Option<InMemoryFormatOutcome> {
    let project = effective_project_config(repo_root, cancel_rx).await;
    format_with_project_config(repo_root, &project, content, cancel_rx).await
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
    async fn test_a_configuration_without_verification_stays_valid() {
        // The file declares only model roles, so the environment is sound:
        // verification is declarative, and a project that declares none runs no
        // verification step instead of failing validation.
        let dir = repo(Some("[models]\nprimary = [\"openrouter:a/b\"]\n"));

        let (status, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert_eq!(
            status,
            VerificationStatus::NotConfigured,
            "a configuration without a verification command must stay valid"
        );
        assert!(
            results.is_empty(),
            "no command may be invented for a project that declares none"
        );
    }

    #[tokio::test]
    async fn test_a_corrupted_project_configuration_fails_phase_zero() {
        // The file is malformed, so the environment itself is unsound: the
        // pipeline reports that verdict instead of resolving a project step.
        let dir = repo(Some("[models]\ncoder = = \"broken\"\n"));

        let (status, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert_eq!(
            status,
            VerificationStatus::Failed,
            "a corrupted configuration fails verification"
        );
        assert_eq!(results.len(), 1, "only the phase-zero verdict is reported");
        assert!(!results[0].success);
        assert_eq!(results[0].command, PROJECT_CONFIG_FILE);
        assert!(
            results[0].combined_output.contains("Failed to parse"),
            "the verdict names the broken invariant: {}",
            results[0].combined_output
        );
    }

    #[tokio::test]
    async fn test_a_valid_project_configuration_keeps_the_project_pipeline() {
        let dir = repo(Some(
            "[models]\nprimary = [\"openrouter:a/b\"]\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'exit 0']\n",
        ));

        let (status, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert_eq!(status, VerificationStatus::Passed);
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

        let (status, results) = run_verification_pipeline(dir.path(), &[], None).await;

        assert_eq!(
            status,
            VerificationStatus::NotConfigured,
            "a repository that declares nothing verifies cleanly"
        );
        assert!(results.is_empty(), "no command may be invented");
    }

    #[tokio::test]
    async fn test_a_project_without_a_declaration_has_no_in_memory_formatter() {
        let dir = repo(None);

        assert!(
            format_staged_content(dir.path(), "fn main() {}\n", None)
                .await
                .is_none(),
            "a repository that declares nothing has no in-memory formatter"
        );

        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[project.verify]\ncommand = \"cargo\"\nargs = [\"test\"]\n",
        )
        .unwrap();
        assert!(
            format_staged_content(dir.path(), "fn main() {}\n", None)
                .await
                .is_none(),
            "a project that declares no in-memory formatter has none"
        );
    }

    #[tokio::test]
    async fn test_post_hook_runs_nothing_without_a_declaration() {
        let dir = repo(None);

        let response = run_post_hooks(dir.path(), &[], None).await.unwrap();

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

        let response = run_post_hooks(dir.path(), &[], None).await.unwrap();

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

        let err = run_post_hooks(dir.path(), &[], None).await.unwrap_err();

        assert!(err.contains("missing"), "unexpected reason: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_post_hook_receives_the_change_set_on_standard_input() {
        // The wrapper forwards the change set of the current round, one relative
        // path per line, exactly as the verification lifecycle receives it.
        let dir = repo(Some(
            "[project.post_hook]\ncommand = \"sh\"\nargs = ['-c', 'cat > received.txt']\n",
        ));

        let changed = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        let response = run_post_hooks(dir.path(), &changed, None)
            .await
            .expect("the declared hook runs");

        assert!(response.success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("received.txt")).unwrap(),
            "src/lib.rs\nsrc/main.rs\n",
            "the hook receives one relative path per line"
        );
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
}
