//! Abstraction over an external project task runner.
//!
//! A project may delegate its command surface to an external task runner
//! instead of declaring every command by hand. The harness never detects a
//! runner on its own and never parses a task-runner file format: the choice is
//! declared explicitly in `[project]`, and the runner's own declarative dump is
//! the single source of what it exports.
//!
//! [`TaskRunner`] isolates the two technology-specific halves of that
//! integration — which process prints the dump and how the dump is read — so
//! supporting another runner means adding one implementation. The configuration
//! surface and the bounded turn stay unchanged.
//!
//! The dump is a plain project process: [`load_manifest`] runs it through the
//! toolchain executor, which means literal arguments, its own process group, the
//! caller's wall-clock budget, and the turn cancellation signal. A runner that
//! is missing, fails, or answers with an unreadable dump yields a typed error
//! and never a partially trusted command list.

use std::fmt;
use std::path::Path;

use tauqe_protocol::{ProjectCommandSpec, TaskRunnerKind};
use tokio::sync::watch;

mod just;

use just::JUST;

/// One command a task runner exports to the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerCommand {
    /// Name the runner addresses the command by.
    pub name: String,
    /// Semantic description declared by the runner, when it declares one.
    pub description: Option<String>,
    /// Concrete command line the harness executes.
    pub spec: ProjectCommandSpec,
}

/// Commands a task runner exports, split by the role they fill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerManifest {
    /// Command bound to the verification lifecycle, when the runner exports one.
    pub verify: Option<RunnerCommand>,
    /// Remaining commands, in name order, published as typed custom tools.
    pub commands: Vec<RunnerCommand>,
}

impl RunnerManifest {
    /// Reports whether the runner exported no command at all.
    pub fn is_empty(&self) -> bool {
        self.verify.is_none() && self.commands.is_empty()
    }
}

/// Merges the exported runner manifest into the project configuration.
///
/// The declared configuration takes precedence over every runner export:
/// a declared `verify` command or a declared named command always wins over
/// a recipe exported with the same name.
pub fn merge_manifest(project: &mut tauqe_protocol::ProjectConfig, manifest: RunnerManifest) {
    if project.verify.is_none() {
        if let Some(verify) = manifest.verify {
            project.verify = Some(verify.spec);
        }
    }
    for command in manifest.commands {
        project.commands.entry(command.name).or_insert(command.spec);
    }
}

/// Failure to obtain the manifest a task runner exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerManifestError {
    /// The runner could not be executed or exited with a failure status.
    Unavailable {
        /// Command line that ran, so the diagnostic names what failed.
        command: String,
        /// Compact failure reason.
        reason: String,
    },
    /// The dump is not the document the runner declares.
    InvalidDump {
        /// What the dump violated.
        reason: String,
    },
    /// The turn was cancelled while the dump ran.
    Cancelled,
}

impl fmt::Display for RunnerManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { command, reason } => write!(
                f,
                "the task runner could not be queried through `{command}`: {reason}"
            ),
            Self::InvalidDump { reason } => {
                write!(f, "the task runner dump is unusable: {reason}")
            }
            Self::Cancelled => f.write_str("the task runner dump was cancelled"),
        }
    }
}

impl std::error::Error for RunnerManifestError {}

/// External task runner whose exported commands the harness may execute.
///
/// The trait isolates exactly the two technology-specific halves of the
/// integration: the process that prints the runner's declarative dump, and the
/// reading of that dump. Adding another runner therefore means adding one
/// implementation, never widening the configuration surface or the bounded turn.
pub trait TaskRunner: Send + Sync {
    /// Executable that prints the runner's declarative dump.
    fn program(&self) -> &str;

    /// Arguments that make the runner print the dump on standard output.
    fn dump_args(&self) -> Vec<String>;

    /// Reads a dump into the commands the harness may run.
    ///
    /// Only commands the runner deliberately exports for the harness may
    /// survive parsing; everything else stays invisible to the model.
    fn parse_manifest(&self, dump: &str) -> Result<RunnerManifest, RunnerManifestError>;
}

/// Returns the runner implementation a project declares, if it declares one.
///
/// The declaration is the only input: a project that declares no runner gets no
/// implementation, so no stray binary on the machine can change what the
/// harness executes.
pub fn task_runner(kind: TaskRunnerKind) -> Option<&'static dyn TaskRunner> {
    match kind {
        TaskRunnerKind::None => None,
        TaskRunnerKind::Just => Some(&JUST),
    }
}

/// Runs the declared runner and reads the manifest it exports.
///
/// The dump is a project process like any other: it runs in the repository root
/// with literal arguments, in its own process group, under the caller's
/// wall-clock budget, and a cancelled turn tears it down instead of leaving it
/// behind. The manifest is parsed only from a successful dump, so a failing
/// runner is never mistaken for one that exports no command.
pub async fn load_manifest(
    runner: &dyn TaskRunner,
    repo_root: &Path,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Result<RunnerManifest, RunnerManifestError> {
    let command = super::exec::CommandLine::new(runner.program(), runner.dump_args());
    let outcome = super::exec::run_command(repo_root, &command, timeout_secs, cancel_rx).await;

    if outcome.is_cancelled {
        return Err(RunnerManifestError::Cancelled);
    }
    if !outcome.success {
        return Err(RunnerManifestError::Unavailable {
            reason: super::compact_failure(&outcome),
            command: outcome.command,
        });
    }

    runner.parse_manifest(&outcome.stdout)
}

#[cfg(test)]
mod tests {
    use tauqe_protocol::TaskRunnerKind;
    use tokio::sync::watch;

    use super::{
        load_manifest, merge_manifest, task_runner, RunnerCommand, RunnerManifest,
        RunnerManifestError, TaskRunner,
    };

    /// A runner whose executable exists on no machine.
    struct MissingRunner;

    impl TaskRunner for MissingRunner {
        fn program(&self) -> &str {
            "tauqe-runner-that-does-not-exist"
        }

        fn dump_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn parse_manifest(&self, _dump: &str) -> Result<RunnerManifest, RunnerManifestError> {
            Err(RunnerManifestError::InvalidDump {
                reason: "the missing runner never prints a dump".to_string(),
            })
        }
    }

    /// A runner whose dump is produced by a command every machine carries.
    struct PrintfRunner;

    impl TaskRunner for PrintfRunner {
        fn program(&self) -> &str {
            "printf"
        }

        fn dump_args(&self) -> Vec<String> {
            vec![r#"{"recipes":{"check":{"attributes":[{"group":"tauqe"}]}}}"#.to_string()]
        }

        fn parse_manifest(&self, dump: &str) -> Result<RunnerManifest, RunnerManifestError> {
            super::just::JUST.parse_manifest(dump)
        }
    }

    /// Creates a temporary repository the dump runs in.
    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary repository")
    }

    #[test]
    fn test_the_factory_follows_the_declared_kind() {
        assert!(task_runner(TaskRunnerKind::None).is_none());

        let runner = task_runner(TaskRunnerKind::Just).expect("just is supported");
        assert_eq!(runner.program(), "just");
        assert_eq!(runner.dump_args(), ["--dump", "--dump-format", "json"]);
    }

    #[tokio::test]
    async fn test_a_missing_runner_is_reported_as_unavailable() {
        let dir = repo();
        let error = load_manifest(&MissingRunner, dir.path(), 10, None)
            .await
            .expect_err("a missing runner cannot be queried");

        match error {
            RunnerManifestError::Unavailable { command, reason } => {
                assert!(
                    command.contains("tauqe-runner-that-does-not-exist"),
                    "unexpected command: {command}"
                );
                assert!(!reason.is_empty());
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn test_a_dump_is_parsed_through_the_runner() {
        let dir = repo();
        let manifest = load_manifest(&PrintfRunner, dir.path(), 10, None)
            .await
            .expect("the dump parses");

        assert!(manifest.verify.is_none());
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].name, "check");
        assert_eq!(manifest.commands[0].spec.command, "just");
        assert_eq!(manifest.commands[0].spec.args, ["check"]);
    }

    #[test]
    fn test_merge_manifest_preserves_declarations_over_exports() {
        use tauqe_protocol::ProjectConfig;

        let mut project = ProjectConfig {
            verify: Some(tauqe_protocol::ProjectCommandSpec {
                command: "cargo".to_string(),
                args: vec!["test".to_string()],
                timeout_secs: Some(300),
            }),
            commands: [(
                "check".to_string(),
                tauqe_protocol::ProjectCommandSpec {
                    command: "cargo".to_string(),
                    args: vec!["check".to_string()],
                    timeout_secs: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let manifest = RunnerManifest {
            verify: Some(RunnerCommand {
                name: "verify".to_string(),
                description: None,
                spec: tauqe_protocol::ProjectCommandSpec {
                    command: "just".to_string(),
                    args: vec!["verify".to_string()],
                    timeout_secs: Some(600),
                },
            }),
            commands: vec![
                RunnerCommand {
                    name: "check".to_string(),
                    description: None,
                    spec: tauqe_protocol::ProjectCommandSpec {
                        command: "just".to_string(),
                        args: vec!["check".to_string()],
                        timeout_secs: None,
                    },
                },
                RunnerCommand {
                    name: "lint".to_string(),
                    description: None,
                    spec: tauqe_protocol::ProjectCommandSpec {
                        command: "just".to_string(),
                        args: vec!["lint".to_string()],
                        timeout_secs: None,
                    },
                },
            ],
        };

        merge_manifest(&mut project, manifest);

        assert_eq!(project.verify.unwrap().command, "cargo");
        assert_eq!(project.commands.get("check").unwrap().command, "cargo");
        assert_eq!(project.commands.get("lint").unwrap().command, "just");
    }

    #[tokio::test]
    async fn test_a_cancelled_dump_is_not_a_failure_report() {
        let dir = repo();
        let (_cancel_tx, cancel_rx) = watch::channel(true);
        let error = load_manifest(&PrintfRunner, dir.path(), 10, Some(&cancel_rx))
            .await
            .expect_err("a cancelled dump yields no manifest");

        assert_eq!(error, RunnerManifestError::Cancelled);
    }
}
