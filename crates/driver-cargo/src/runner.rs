//! Autonomous execution of the three phases of the project protocol.
//!
//! The driver is the only place that knows how a repository is engineered: it
//! reads `tauqe-project.toml` from the repository root and executes the phases
//! of [`tauqe_protocol::ProjectRequest`] — in-memory formatting, disk-side
//! post-hooks, and named verification suites. The harness stays technology
//! agnostic, because every language-specific decision is confined here.
//!
//! Three rules shape the implementation:
//!
//! * A repository that declares no manifest gets a synthesized one, derived
//!   deterministically from the build files it does contain (`Cargo.toml`,
//!   `package.json`, `go.mod`). A plain Rust, Node, or Go repository is
//!   therefore verifiable without any configuration, and a declared manifest
//!   always wins over the synthesis.
//! * Every child process runs in its own process group, so a timeout tears the
//!   whole descendant tree down instead of orphaning the grandchildren a build
//!   system spawns.
//! * Child output is captured in temporary files rather than pipes, so a
//!   lingering background grandchild that inherited the descriptor cannot block
//!   the driver waiting for a pipe that never closes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tauqe_protocol::{
    DiagnosticParserKind, ProjectCapabilities, ProjectCheckRequest, ProjectCheckResponse,
    ProjectCheckStepSpec, ProjectCheckSuiteSpec, ProjectFormatRequest, ProjectFormatResponse,
    ProjectFormatterSpec, ProjectHookOutcome, ProjectManifest, ProjectPostHookRequest,
    ProjectPostHookResponse, ProjectPostHookSpec, ProjectRequest, ProjectResponse,
    ProjectStepResult,
};

use crate::diagnostics::{parse_diagnostics, MAX_DIAGNOSTICS};

/// Name of the project manifest the driver reads from the repository root.
pub const PROJECT_MANIFEST_FILE: &str = "tauqe-project.toml";

/// Timeout applied to one command when its manifest entry declares none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Poll interval used while waiting for a child process to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Executes the project protocol phases over one repository root.
#[derive(Debug, Clone)]
pub struct ProjectRunner {
    repo_root: PathBuf,
    manifest: ProjectManifest,
}

impl ProjectRunner {
    /// Loads the manifest declared by `repo_root`, synthesizing one when absent.
    ///
    /// A manifest that exists but cannot be read or parsed degrades to the
    /// synthesized manifest with a diagnostic on stderr, so one typo does not
    /// disable verification for the whole repository.
    pub fn load(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let manifest = load_manifest(&repo_root);
        Self {
            repo_root,
            manifest,
        }
    }

    /// Builds a runner over an already resolved manifest.
    pub fn with_manifest(repo_root: impl Into<PathBuf>, manifest: ProjectManifest) -> Self {
        Self {
            repo_root: repo_root.into(),
            manifest,
        }
    }

    /// Repository root every command runs in.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Manifest the runner executes.
    pub fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    /// Facets this runner implements for the loaded manifest.
    pub fn capabilities(&self) -> ProjectCapabilities {
        ProjectCapabilities {
            format: !self.manifest.formatters.is_empty(),
            post_hook: !self.manifest.post_hooks.is_empty(),
            suites: self.manifest.checks.keys().cloned().collect(),
            diagnostics: true,
            docs: false,
            dependencies: false,
        }
    }

    /// Answers one request with the matching response.
    pub fn handle(&self, request: &ProjectRequest) -> ProjectResponse {
        match request {
            ProjectRequest::Capabilities => ProjectResponse::Capabilities {
                capabilities: self.capabilities(),
            },
            ProjectRequest::Format(request) => ProjectResponse::Format(self.format(request)),
            ProjectRequest::PostHook(request) => ProjectResponse::PostHook(self.post_hook(request)),
            ProjectRequest::Check(request) => ProjectResponse::Check(self.check(request)),
        }
    }

    /// Formats one file in memory.
    ///
    /// The staged content travels to the formatter's stdin and the rewritten
    /// code comes back from its stdout; nothing touches the file system. When no
    /// formatter claims the path, or the formatter fails, or it produced no
    /// output for a non-empty input, the authored content is returned unchanged
    /// with `formatted: false`.
    pub fn format(&self, request: &ProjectFormatRequest) -> ProjectFormatResponse {
        let Some((name, spec)) = self.manifest.select_formatter_for_path(&request.path) else {
            return ProjectFormatResponse {
                formatted: false,
                content: request.content.clone(),
                formatter: None,
                error: None,
            };
        };

        let command = CommandSpec::formatter(spec);
        let outcome = run_command(&self.repo_root, &command, Some(request.content.as_bytes()));

        if !outcome.success {
            return ProjectFormatResponse {
                formatted: false,
                content: request.content.clone(),
                formatter: Some(name.to_string()),
                error: outcome.failure,
            };
        }

        // A formatter that emits nothing while the input was not empty did not
        // rewrite the code: keep the authored content instead of erasing it.
        if outcome.stdout.is_empty() && !request.content.trim().is_empty() {
            return ProjectFormatResponse {
                formatted: false,
                content: request.content.clone(),
                formatter: Some(name.to_string()),
                error: Some(format!(
                    "formatter `{}` produced no output",
                    spec.command.trim()
                )),
            };
        }

        ProjectFormatResponse {
            formatted: true,
            content: outcome.stdout,
            formatter: Some(name.to_string()),
            error: None,
        }
    }

    /// Runs the disk-side hooks responsible for the change set.
    ///
    /// Hooks execute in manifest name order, and the phase stops at the first
    /// failure: a broken code generator invalidates the change set, so running
    /// the remaining hooks would only produce work on top of a failed state.
    pub fn post_hook(&self, request: &ProjectPostHookRequest) -> ProjectPostHookResponse {
        let hooks = self.manifest.matching_post_hooks(&request.changed_files);

        let mut executed = Vec::new();
        let mut success = true;
        for (name, spec) in hooks {
            let command = CommandSpec::post_hook(spec, &request.changed_files);
            let outcome = run_command(&self.repo_root, &command, None);
            executed.push(ProjectHookOutcome {
                name: name.to_string(),
                success: outcome.success,
                message: outcome.failure,
            });
            if !outcome.success {
                success = false;
                break;
            }
        }

        let message = if executed.is_empty() {
            Some("no post-hook matched the change set".to_string())
        } else if success {
            Some(format!("{} post-hook(s) executed", executed.len()))
        } else {
            // The phase summary repeats the failing hook and its compact reason,
            // so a timeout or a signal reads as one line instead of a bare name.
            let failed = executed.last();
            let name = failed.map(|hook| hook.name.as_str()).unwrap_or_default();
            let reason = failed
                .and_then(|hook| hook.message.as_deref())
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            Some(format!("post-hook `{name}` failed{reason}"))
        };

        ProjectPostHookResponse {
            success,
            executed_hooks: executed,
            message,
        }
    }

    /// Runs one named verification suite.
    ///
    /// Steps execute in manifest name order and the suite stops at the first
    /// failing step, because later steps would run on top of a change set the
    /// compiler already rejected. Diagnostics are parsed only from the output of
    /// failing steps, so a successful run never contributes incidental findings.
    pub fn check(&self, request: &ProjectCheckRequest) -> ProjectCheckResponse {
        let steps = self
            .manifest
            .select_check_steps_for_suite(&request.suite, &request.changed_files);

        if steps.is_empty() {
            // A declared suite with nothing in scope for this change set passes
            // trivially; a suite nobody declared is a configuration error and
            // must never look like a green verification.
            if self.manifest.checks.contains_key(&request.suite) {
                return ProjectCheckResponse {
                    success: true,
                    steps: Vec::new(),
                    diagnostics: Vec::new(),
                };
            }
            return ProjectCheckResponse {
                success: false,
                steps: vec![ProjectStepResult {
                    name: request.suite.clone(),
                    command: String::new(),
                    success: false,
                    duration_ms: None,
                    message: Some(format!(
                        "unknown suite `{}`: the project manifest declares no such suite",
                        request.suite
                    )),
                }],
                diagnostics: Vec::new(),
            };
        }

        let mut results = Vec::new();
        let mut diagnostics = Vec::new();
        let mut success = true;

        for (name, spec) in steps {
            let command = CommandSpec::check_step(spec);
            let outcome = run_command(&self.repo_root, &command, None);
            if !outcome.success {
                diagnostics.extend(parse_diagnostics(spec.parser, &outcome.combined));
            }
            results.push(ProjectStepResult {
                name: name.to_string(),
                command: command.display(),
                success: outcome.success,
                duration_ms: Some(outcome.duration_ms),
                message: outcome.failure,
            });
            if !outcome.success {
                success = false;
                break;
            }
        }

        diagnostics.truncate(MAX_DIAGNOSTICS);
        ProjectCheckResponse {
            success,
            steps: results,
            diagnostics,
        }
    }
}

/// Reads the declared manifest, falling back to a synthesized one.
fn load_manifest(repo_root: &Path) -> ProjectManifest {
    let path = repo_root.join(PROJECT_MANIFEST_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<ProjectManifest>(&text) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!(
                    "tauqe-driver-cargo: cannot parse {}: {err}; using a synthesized manifest",
                    path.display()
                );
                synthesize_manifest(repo_root)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => synthesize_manifest(repo_root),
        Err(err) => {
            eprintln!(
                "tauqe-driver-cargo: cannot read {}: {err}; using a synthesized manifest",
                path.display()
            );
            synthesize_manifest(repo_root)
        }
    }
}

/// Derives a manifest from the build files the repository actually contains.
///
/// The synthesis is deterministic: only the presence of `Cargo.toml`,
/// `package.json`, and `go.mod` is inspected, and each detected ecosystem
/// contributes its formatter and its phase steps. Steps are scoped by the source
/// extensions of their ecosystem, so a Rust backend and a TypeScript frontend
/// share one repository without either toolchain scanning the other's files.
fn synthesize_manifest(repo_root: &Path) -> ProjectManifest {
    let mut manifest = ProjectManifest::default();
    let mut steps = PhaseSteps::default();

    if repo_root.join("Cargo.toml").is_file() {
        synthesize_rust(&mut manifest, &mut steps, repo_root);
    }
    if repo_root.join("package.json").is_file() {
        synthesize_node(&mut steps);
    }
    if repo_root.join("go.mod").is_file() {
        synthesize_go(&mut manifest, &mut steps);
    }

    steps.install(&mut manifest);
    manifest
}

/// Adds the Rust formatter and the Rust phase steps.
fn synthesize_rust(manifest: &mut ProjectManifest, steps: &mut PhaseSteps, repo_root: &Path) {
    manifest.formatters.insert(
        "rust".to_string(),
        ProjectFormatterSpec {
            command: "rustfmt".to_string(),
            args: rustfmt_args(repo_root),
            extensions: vec!["rs".to_string()],
            ..Default::default()
        },
    );

    let sources = ["**/*.rs"];
    steps.push(
        Phase::Check,
        "rust-check",
        scoped_step(
            "cargo",
            ["check", "--all-targets", "--message-format=json"],
            &sources,
            DiagnosticParserKind::CargoJson,
        ),
    );
    steps.push(
        Phase::Lint,
        "rust-lint",
        scoped_step(
            "cargo",
            ["clippy", "--all-targets", "--message-format=json"],
            &sources,
            DiagnosticParserKind::CargoJson,
        ),
    );
    steps.push(
        Phase::Test,
        "rust-test",
        scoped_step("cargo", ["test"], &sources, DiagnosticParserKind::Plain),
    );
}

/// Adds the Node phase steps.
///
/// No formatter is synthesized: `prettier` needs the file path on its command
/// line, and the manifest model declares only literal arguments, so formatting
/// Node sources is left to a manifest that can express it.
fn synthesize_node(steps: &mut PhaseSteps) {
    let sources = ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx", "package.json"];
    steps.push(
        Phase::Check,
        "node-check",
        scoped_step(
            "npx",
            ["tsc", "--noEmit", "--pretty", "false"],
            &sources,
            DiagnosticParserKind::TypeScript,
        ),
    );
    steps.push(
        Phase::Test,
        "node-test",
        scoped_step(
            "npm",
            ["test", "--silent"],
            &sources,
            DiagnosticParserKind::Plain,
        ),
    );
}

/// Adds the Go formatter and the Go phase steps.
fn synthesize_go(manifest: &mut ProjectManifest, steps: &mut PhaseSteps) {
    manifest.formatters.insert(
        "go".to_string(),
        ProjectFormatterSpec {
            command: "gofmt".to_string(),
            extensions: vec!["go".to_string()],
            ..Default::default()
        },
    );

    let sources = ["**/*.go"];
    steps.push(
        Phase::Check,
        "go-check",
        scoped_step("go", ["build", "./..."], &sources, DiagnosticParserKind::Go),
    );
    steps.push(
        Phase::Lint,
        "go-lint",
        scoped_step("go", ["vet", "./..."], &sources, DiagnosticParserKind::Go),
    );
    steps.push(
        Phase::Test,
        "go-test",
        scoped_step("go", ["test", "./..."], &sources, DiagnosticParserKind::Go),
    );
}

/// Builds the `rustfmt` arguments for the repository's declared edition.
fn rustfmt_args(repo_root: &Path) -> Vec<String> {
    let mut args = vec!["--emit".to_string(), "stdout".to_string()];
    if let Some(edition) = cargo_edition(repo_root) {
        args.push("--edition".to_string());
        args.push(edition);
    }
    args
}

/// Reads the package edition a `Cargo.toml` declares, if any.
fn cargo_edition(repo_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo_root.join("Cargo.toml")).ok()?;
    let manifest: toml::Value = toml::from_str(&text).ok()?;
    manifest
        .get("package")?
        .get("edition")?
        .as_str()
        .map(str::trim)
        .filter(|edition| !edition.is_empty())
        .map(str::to_string)
}

/// Builds a path-scoped verification step.
fn scoped_step<const N: usize>(
    program: &str,
    args: [&str; N],
    paths: &[&str],
    parser: DiagnosticParserKind,
) -> ProjectCheckStepSpec {
    ProjectCheckStepSpec {
        command: program.to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        paths: paths.iter().map(|path| path.to_string()).collect(),
        parser,
        ..Default::default()
    }
}

/// Assembles a suite from already named steps.
fn suite_from(steps: Vec<(String, ProjectCheckStepSpec)>) -> ProjectCheckSuiteSpec {
    ProjectCheckSuiteSpec {
        steps: steps.into_iter().collect(),
    }
}

/// Verification phase a synthesized step belongs to.
#[derive(Debug, Clone, Copy)]
enum Phase {
    Check,
    Lint,
    Test,
}

/// Step accumulator used while a manifest is synthesized.
#[derive(Debug, Default)]
struct PhaseSteps {
    check: Vec<(String, ProjectCheckStepSpec)>,
    lint: Vec<(String, ProjectCheckStepSpec)>,
    test: Vec<(String, ProjectCheckStepSpec)>,
}

impl PhaseSteps {
    /// Files a synthesized step under its phase.
    fn push(&mut self, phase: Phase, name: &str, spec: ProjectCheckStepSpec) {
        let target = match phase {
            Phase::Check => &mut self.check,
            Phase::Lint => &mut self.lint,
            Phase::Test => &mut self.test,
        };
        target.push((name.to_string(), spec));
    }

    /// Installs the accumulated steps as phase suites plus one combined suite.
    fn install(self, manifest: &mut ProjectManifest) {
        let mut all: Vec<(String, ProjectCheckStepSpec)> = Vec::new();
        let phases = [
            ("check", self.check),
            ("lint", self.lint),
            ("test", self.test),
        ];

        for (name, steps) in phases {
            if steps.is_empty() {
                continue;
            }
            all.extend(steps.iter().cloned());
            manifest.checks.insert(name.to_string(), suite_from(steps));
        }

        if !all.is_empty() {
            manifest.checks.insert("all".to_string(), suite_from(all));
        }
    }
}

/// A program together with its literal argument list, working directory, and
/// timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    workdir: Option<String>,
    timeout_secs: u64,
}

impl CommandSpec {
    /// Builds the command of an in-memory formatter entry.
    fn formatter(spec: &ProjectFormatterSpec) -> Self {
        Self {
            program: spec.command.clone(),
            args: spec.args.clone(),
            workdir: spec.workdir.clone(),
            timeout_secs: spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Builds the command of a post-hook entry, appending the change set when the
    /// entry asks for it.
    fn post_hook(spec: &ProjectPostHookSpec, changed_files: &[String]) -> Self {
        let mut args = spec.args.clone();
        if spec.pass_files {
            args.extend(changed_files.iter().cloned());
        }
        Self {
            program: spec.command.clone(),
            args,
            workdir: spec.workdir.clone(),
            timeout_secs: spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Builds the command of a verification step.
    fn check_step(spec: &ProjectCheckStepSpec) -> Self {
        Self {
            program: spec.command.clone(),
            args: spec.args.clone(),
            workdir: spec.workdir.clone(),
            timeout_secs: spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// How the command is reported in step results and hook outcomes.
    fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// Result of one executed command.
#[derive(Debug, Clone, Default)]
struct CommandOutcome {
    /// Whether the command exited successfully.
    success: bool,
    /// Captured standard output, which is the formatted code for a formatter.
    stdout: String,
    /// Standard output followed by standard error, the diagnostic input.
    combined: String,
    /// Wall-clock duration of the invocation.
    duration_ms: u64,
    /// Compact failure reason, never the raw transcript.
    failure: Option<String>,
}

/// Runs a command in the repository root and measures it.
fn run_command(repo_root: &Path, spec: &CommandSpec, stdin: Option<&[u8]>) -> CommandOutcome {
    let cwd = match spec
        .workdir
        .as_deref()
        .map(str::trim)
        .filter(|workdir| !workdir.is_empty())
    {
        Some(workdir) => repo_root.join(workdir),
        None => repo_root.to_path_buf(),
    };

    let started = Instant::now();
    let mut outcome = run_command_core(&cwd, spec, stdin);
    outcome.duration_ms = started.elapsed().as_millis() as u64;
    outcome
}

/// Spawns the command, feeds its stdin, and collects its output.
fn run_command_core(cwd: &Path, spec: &CommandSpec, stdin: Option<&[u8]>) -> CommandOutcome {
    let program = spec.program.trim();
    if program.is_empty() {
        return CommandOutcome {
            success: true,
            ..Default::default()
        };
    }

    let (capture, stdout_file, stderr_file) = match OutputCapture::create() {
        Ok(parts) => parts,
        Err(err) => return failure_outcome(format!("cannot capture command output: {err}")),
    };

    let mut command = Command::new(program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            capture.cleanup();
            return failure_outcome(format!("failed to run `{program}`: {err}"));
        }
    };

    // The write happens on a detached thread so a formatter that never consumes
    // its input cannot block the driver before the timeout is observed; the
    // thread unblocks with a broken pipe once the child is torn down.
    if let Some(data) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let data = data.to_vec();
            drop(std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
                let _ = pipe.flush();
            }));
        }
    }

    let exit = wait_for_exit(&mut child, spec.timeout_secs);
    let (stdout, stderr) = capture.read();
    capture.cleanup();
    let combined = combine_output(&stdout, &stderr);

    let (success, failure) = match exit {
        ExitOutcome::Exited(status) if status.success() => (true, None),
        ExitOutcome::Exited(status) => (false, Some(exit_description(status))),
        ExitOutcome::TimedOut => (
            false,
            Some(format!("timed out after {}s", spec.timeout_secs)),
        ),
        ExitOutcome::Failed(err) => (false, Some(format!("cannot observe `{program}`: {err}"))),
    };

    CommandOutcome {
        success,
        stdout,
        combined,
        duration_ms: 0,
        failure,
    }
}

/// Builds the outcome of a command that could not be observed at all.
fn failure_outcome(message: String) -> CommandOutcome {
    CommandOutcome {
        success: false,
        failure: Some(message),
        ..Default::default()
    }
}

/// Describes how a command terminated for a step result or hook outcome.
fn exit_description(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => "was terminated by a signal".to_string(),
    }
}

/// Joins the captured streams into the diagnostic input.
fn combine_output(stdout: &str, stderr: &str) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(stderr);
    }
    combined
}

/// How a watched child process terminated.
#[derive(Debug)]
enum ExitOutcome {
    /// The child exited on its own and was reaped.
    Exited(std::process::ExitStatus),
    /// The timeout expired; the process group was torn down and reaped.
    TimedOut,
    /// The child state could not be observed.
    Failed(String),
}

/// Waits for a child to exit, enforcing the timeout on the whole process group.
fn wait_for_exit(child: &mut Child, timeout_secs: u64) -> ExitOutcome {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ExitOutcome::Exited(status),
            Ok(None) => {}
            Err(err) => {
                kill_tree(child);
                return ExitOutcome::Failed(err.to_string());
            }
        }

        if Instant::now() >= deadline {
            kill_tree(child);
            return ExitOutcome::TimedOut;
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Tears down the whole process group of the child and reaps it.
///
/// The group is signalled first, while the direct child is still un-reaped, so
/// its PID cannot have been recycled and the entire descendant tree is killed.
fn kill_tree(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// Sends `SIGKILL` to the process group led by `pid`.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    if let Ok(pid) = i32::try_from(pid) {
        if pid > 1 {
            unsafe {
                let _ = kill(-pid, 9);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Temporary files that receive the standard streams of one child process.
struct OutputCapture {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl OutputCapture {
    /// Creates the capture files, returning their handles for the child.
    fn create() -> std::io::Result<(Self, std::fs::File, std::fs::File)> {
        let id = next_capture_id();
        let base = std::env::temp_dir();
        let stdout_path = base.join(format!("tauqe-driver-{}-{id}.stdout", std::process::id()));
        let stderr_path = base.join(format!("tauqe-driver-{}-{id}.stderr", std::process::id()));

        let stdout_file = std::fs::File::create(&stdout_path)?;
        let stderr_file = match std::fs::File::create(&stderr_path) {
            Ok(file) => file,
            Err(err) => {
                let _ = std::fs::remove_file(&stdout_path);
                return Err(err);
            }
        };

        Ok((
            Self {
                stdout_path,
                stderr_path,
            },
            stdout_file,
            stderr_file,
        ))
    }

    /// Reads the captured streams, decoding undecodable bytes lossily.
    fn read(&self) -> (String, String) {
        (read_lossy(&self.stdout_path), read_lossy(&self.stderr_path))
    }

    /// Removes the capture files; a failure is not actionable.
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

/// Reads a captured file as text, yielding an empty string when unreadable.
fn read_lossy(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => String::new(),
    }
}

/// Hands out unique capture file identifiers within this process.
fn next_capture_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary repository root removed when the test ends.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        /// Creates an empty directory with a unique name.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tauqe-driver-test-{}-{}",
                std::process::id(),
                next_capture_id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        /// Root of the temporary repository.
        fn path(&self) -> &Path {
            &self.path
        }

        /// Writes one file relative to the root.
        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.path.join(name), content).unwrap();
        }

        /// Reads one file relative to the root.
        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.path.join(name)).unwrap()
        }

        /// Reports whether a file exists relative to the root.
        fn exists(&self, name: &str) -> bool {
            self.path.join(name).exists()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a formatter entry.
    fn formatter_spec(command: &str, args: &[&str], extensions: &[&str]) -> ProjectFormatterSpec {
        ProjectFormatterSpec {
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            extensions: extensions
                .iter()
                .map(|extension| extension.to_string())
                .collect(),
            ..Default::default()
        }
    }

    /// Builds a post-hook entry.
    fn hook_spec(command: &str, args: &[&str]) -> ProjectPostHookSpec {
        ProjectPostHookSpec {
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Builds a verification step.
    fn step_spec(
        command: &str,
        args: &[&str],
        parser: DiagnosticParserKind,
    ) -> ProjectCheckStepSpec {
        ProjectCheckStepSpec {
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            parser,
            ..Default::default()
        }
    }

    /// Builds a suite from already named steps.
    fn build_suite(steps: Vec<(&str, ProjectCheckStepSpec)>) -> ProjectCheckSuiteSpec {
        ProjectCheckSuiteSpec {
            steps: steps
                .into_iter()
                .map(|(name, spec)| (name.to_string(), spec))
                .collect(),
        }
    }

    /// Builds a change set from path literals.
    fn changed(files: &[&str]) -> Vec<String> {
        files.iter().map(|file| file.to_string()).collect()
    }

    /// Collects the names of the selected steps.
    fn step_names<'a>(steps: &[(&'a str, &'a ProjectCheckStepSpec)]) -> Vec<&'a str> {
        steps.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn test_synthesize_manifest_detects_rust_project() {
        let dir = TempDir::new();
        dir.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nedition = \"2021\"\n",
        );

        let manifest = synthesize_manifest(dir.path());

        let formatter = manifest.formatters.get("rust").unwrap();
        assert_eq!(formatter.command, "rustfmt");
        assert_eq!(formatter.args, ["--emit", "stdout", "--edition", "2021"]);
        assert_eq!(formatter.extensions, ["rs"]);

        let suites: Vec<&str> = manifest.checks.keys().map(String::as_str).collect();
        assert_eq!(suites, ["all", "check", "lint", "test"]);

        let steps = manifest.select_check_steps_for_suite("check", &changed(&["src/lib.rs"]));
        assert_eq!(step_names(&steps), ["rust-check"]);

        // Steps are scoped to their own ecosystem.
        assert!(manifest
            .select_check_steps_for_suite("check", &changed(&["web/app.ts"]))
            .is_empty());
    }

    #[test]
    fn test_synthesize_manifest_ignores_unknown_project() {
        let dir = TempDir::new();

        let manifest = synthesize_manifest(dir.path());
        assert!(manifest.formatters.is_empty());
        assert!(manifest.post_hooks.is_empty());
        assert!(manifest.checks.is_empty());
    }

    #[test]
    fn test_synthesize_manifest_supports_polyglot_project() {
        let dir = TempDir::new();
        dir.write("Cargo.toml", "[package]\nname = \"demo\"\n");
        dir.write("package.json", "{}\n");
        dir.write("go.mod", "module demo\n");

        let manifest = synthesize_manifest(dir.path());

        let formatters: Vec<&str> = manifest.formatters.keys().map(String::as_str).collect();
        assert_eq!(formatters, ["go", "rust"]);
        // `prettier` needs the file path on its command line, so no Node
        // formatter is synthesized.
        assert!(manifest.select_formatter_for_path("web/app.tsx").is_none());
        assert_eq!(
            manifest
                .select_formatter_for_path("cmd/main.go")
                .map(|(name, _)| name),
            Some("go")
        );

        let rust_only = manifest.select_check_steps_for_suite("check", &changed(&["src/lib.rs"]));
        assert_eq!(step_names(&rust_only), ["rust-check"]);

        let node_only = manifest.select_check_steps_for_suite("check", &changed(&["web/app.tsx"]));
        assert_eq!(step_names(&node_only), ["node-check"]);

        // The combined suite folds every phase of every detected ecosystem, so a
        // post-turn verification runs the compilers, the linters, and the tests.
        let every_change = changed(&["src/lib.rs", "web/app.tsx", "cmd/main.go"]);
        let steps = manifest.select_check_steps_for_suite("all", &every_change);
        assert_eq!(
            step_names(&steps),
            [
                "go-check",
                "go-lint",
                "go-test",
                "node-check",
                "node-test",
                "rust-check",
                "rust-lint",
                "rust-test",
            ]
        );

        let test_steps = manifest.select_check_steps_for_suite("test", &every_change);
        assert_eq!(
            step_names(&test_steps),
            ["go-test", "node-test", "rust-test"]
        );

        // Node declares no lint phase, so no lint suite is synthesized for it.
        let lint_steps = manifest.select_check_steps_for_suite("lint", &every_change);
        assert_eq!(step_names(&lint_steps), ["go-lint", "rust-lint"]);
    }

    #[test]
    fn test_load_prefers_the_declared_manifest() {
        let dir = TempDir::new();
        dir.write("Cargo.toml", "[package]\nname = \"demo\"\n");
        dir.write(
            "tauqe-project.toml",
            "[formatters.hooks]\ncommand = \"sh\"\nextensions = [\"txt\"]\n",
        );

        let runner = ProjectRunner::load(dir.path());
        let capabilities = runner.capabilities();
        assert!(capabilities.format);
        assert!(!capabilities.post_hook);
        assert!(capabilities.suites.is_empty());
        assert!(capabilities.diagnostics);
        assert!(!capabilities.docs);
        assert!(!capabilities.dependencies);

        // The declared manifest wins: the synthesized Rust formatter is absent.
        assert!(runner
            .manifest()
            .select_formatter_for_path("src/lib.rs")
            .is_none());
        assert_eq!(
            runner
                .manifest()
                .select_formatter_for_path("notes.txt")
                .map(|(name, _)| name),
            Some("hooks")
        );
    }

    #[test]
    fn test_load_falls_back_to_a_synthesized_manifest() {
        let dir = TempDir::new();
        dir.write("Cargo.toml", "[package]\nname = \"demo\"\n");
        dir.write("tauqe-project.toml", "this is not = a manifest\n");

        let runner = ProjectRunner::load(dir.path());
        assert!(runner.manifest().formatters.contains_key("rust"));
        assert!(runner.capabilities().format);
    }

    #[cfg(unix)]
    #[test]
    fn test_format_rewrites_content_in_memory() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.formatters.insert(
            "upper".to_string(),
            formatter_spec("sh", &["-c", "tr a-z A-Z"], &["txt"]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.format(&ProjectFormatRequest {
            path: "notes.txt".to_string(),
            content: "hello".to_string(),
        });

        assert!(response.formatted);
        assert_eq!(response.content, "HELLO");
        assert_eq!(response.formatter.as_deref(), Some("upper"));
        assert!(response.error.is_none());
        assert!(
            !dir.exists("notes.txt"),
            "formatting must not touch the file system"
        );
    }

    #[test]
    fn test_format_without_a_formatter_keeps_the_authored_content() {
        let dir = TempDir::new();
        let runner = ProjectRunner::with_manifest(dir.path(), ProjectManifest::default());

        let response = runner.format(&ProjectFormatRequest {
            path: "notes.md".to_string(),
            content: "# Notes\n".to_string(),
        });

        assert!(!response.formatted);
        assert_eq!(response.content, "# Notes\n");
        assert!(response.formatter.is_none());
        assert!(response.error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_format_keeps_the_authored_content_when_the_formatter_fails() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.formatters.insert(
            "broken".to_string(),
            formatter_spec("sh", &["-c", "exit 3"], &["txt"]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.format(&ProjectFormatRequest {
            path: "notes.txt".to_string(),
            content: "hello".to_string(),
        });

        assert!(!response.formatted);
        assert_eq!(response.content, "hello");
        assert_eq!(response.formatter.as_deref(), Some("broken"));
        assert!(response
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("status 3"));
    }

    #[cfg(unix)]
    #[test]
    fn test_format_keeps_the_authored_content_when_the_formatter_emits_nothing() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.formatters.insert(
            "silent".to_string(),
            formatter_spec("sh", &["-c", "cat > /dev/null"], &["txt"]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.format(&ProjectFormatRequest {
            path: "notes.txt".to_string(),
            content: "hello".to_string(),
        });

        assert!(!response.formatted);
        assert_eq!(response.content, "hello");
        assert!(response
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no output"));
    }

    #[cfg(unix)]
    #[test]
    fn test_post_hook_runs_the_matching_hook() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.post_hooks.insert(
            "marker".to_string(),
            hook_spec("sh", &["-c", "printf hook > marker.txt"]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.post_hook(&ProjectPostHookRequest {
            changed_files: changed(&["src/lib.rs"]),
        });

        assert!(response.success);
        assert_eq!(response.executed_hooks.len(), 1);
        assert_eq!(response.executed_hooks[0].name, "marker");
        assert!(response.executed_hooks[0].success);
        assert_eq!(dir.read("marker.txt"), "hook");
        assert!(response
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("1 post-hook"));
    }

    #[cfg(unix)]
    #[test]
    fn test_post_hook_passes_the_changed_files() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        let mut spec = hook_spec("sh", &["-c", "printf '%s\\n' \"$@\" > seen.txt", "sh"]);
        spec.pass_files = true;
        manifest.post_hooks.insert("record".to_string(), spec);
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.post_hook(&ProjectPostHookRequest {
            changed_files: changed(&["a.txt", "b.txt"]),
        });

        assert!(response.success);
        assert_eq!(dir.read("seen.txt"), "a.txt\nb.txt\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_post_hook_stops_after_the_first_failure() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest
            .post_hooks
            .insert("alpha".to_string(), hook_spec("sh", &["-c", "exit 1"]));
        manifest.post_hooks.insert(
            "beta".to_string(),
            hook_spec("sh", &["-c", "printf late > late.txt"]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.post_hook(&ProjectPostHookRequest {
            changed_files: Vec::new(),
        });

        assert!(!response.success);
        assert_eq!(response.executed_hooks.len(), 1);
        assert_eq!(response.executed_hooks[0].name, "alpha");
        assert!(!response.executed_hooks[0].success);
        assert!(!dir.exists("late.txt"));
        assert!(response
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("failed"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_reports_structured_diagnostics() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.checks.insert(
            "check".to_string(),
            build_suite(vec![(
                "lint",
                step_spec(
                    "sh",
                    &[
                        "-c",
                        "printf 'main.go:5:2: error: undefined: helper\\n'; exit 1",
                    ],
                    DiagnosticParserKind::Go,
                ),
            )]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.check(&ProjectCheckRequest {
            suite: "check".to_string(),
            changed_files: Vec::new(),
        });

        assert!(!response.success);
        assert_eq!(response.steps.len(), 1);
        assert_eq!(response.steps[0].name, "lint");
        assert!(!response.steps[0].success);
        assert!(response.steps[0].duration_ms.is_some());

        assert_eq!(response.diagnostics.len(), 1);
        let diagnostic = &response.diagnostics[0];
        assert_eq!(diagnostic.file.as_deref(), Some("main.go"));
        assert_eq!(diagnostic.line, Some(5));
        assert_eq!(diagnostic.column, Some(2));
        assert_eq!(diagnostic.message, "undefined: helper");
    }

    #[test]
    fn test_check_reports_an_unknown_suite() {
        let dir = TempDir::new();
        let runner = ProjectRunner::with_manifest(dir.path(), ProjectManifest::default());

        let response = runner.check(&ProjectCheckRequest {
            suite: "check".to_string(),
            changed_files: Vec::new(),
        });

        assert!(!response.success);
        assert_eq!(response.steps.len(), 1);
        assert!(response.steps[0]
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("unknown suite"));
        assert!(response.diagnostics.is_empty());
    }

    #[test]
    fn test_check_skips_steps_outside_the_change_set() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        let mut spec = step_spec("cargo", &["check"], DiagnosticParserKind::Plain);
        spec.paths = vec!["**/*.rs".to_string()];
        manifest
            .checks
            .insert("check".to_string(), build_suite(vec![("rust-check", spec)]));
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.check(&ProjectCheckRequest {
            suite: "check".to_string(),
            changed_files: changed(&["docs/guide.md"]),
        });

        assert!(response.success);
        assert!(response.steps.is_empty());
        assert!(response.diagnostics.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_check_stops_after_the_first_failing_step() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        manifest.checks.insert(
            "all".to_string(),
            build_suite(vec![
                (
                    "alpha",
                    step_spec("sh", &["-c", "exit 1"], DiagnosticParserKind::Plain),
                ),
                (
                    "beta",
                    step_spec(
                        "sh",
                        &["-c", "printf late > late.txt"],
                        DiagnosticParserKind::Plain,
                    ),
                ),
            ]),
        );
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let response = runner.check(&ProjectCheckRequest {
            suite: "all".to_string(),
            changed_files: Vec::new(),
        });

        assert!(!response.success);
        assert_eq!(response.steps.len(), 1);
        assert_eq!(response.steps[0].name, "alpha");
        assert!(!dir.exists("late.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn test_command_timeout_kills_the_process_group() {
        let dir = TempDir::new();
        let mut manifest = ProjectManifest::default();
        let mut spec = hook_spec(
            "sh",
            &[
                "-c",
                "sh -c 'while true; do echo tick >> heartbeat.log; sleep 0.1; done' & sleep 30",
            ],
        );
        spec.timeout_secs = Some(1);
        manifest.post_hooks.insert("slow".to_string(), spec);
        let runner = ProjectRunner::with_manifest(dir.path(), manifest);

        let started = Instant::now();
        let response = runner.post_hook(&ProjectPostHookRequest {
            changed_files: Vec::new(),
        });
        let elapsed = started.elapsed();

        assert!(!response.success);
        assert!(response
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("timed out"));
        assert!(
            elapsed < Duration::from_secs(5),
            "the timeout was not enforced: {elapsed:?}"
        );

        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_kill = std::fs::metadata(&heartbeat)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(size_at_kill > 0, "the grandchild never wrote");

        std::thread::sleep(Duration::from_millis(2200));
        let size_later = std::fs::metadata(&heartbeat)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild survived the process-group kill"
        );
    }
}
