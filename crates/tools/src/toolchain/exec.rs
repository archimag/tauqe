//! Execution of a single project command with timeout and cancellation.
//!
//! A command runs in the repository root with its arguments passed through
//! `Command::args`, so a literal path or argument never reaches a shell. On Unix
//! the child is launched in its own process group, so a timeout or a client
//! cancellation tears down the whole process hierarchy (e.g. `rustc` and the test
//! binaries it spawned) instead of leaving orphans behind.

use crate::blocking::StartedOperations;
use crate::process::{self, ProcessEnd};
use crate::stop::HardCancellation;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::report::log_toolchain_check_result;
use super::ToolchainCheckResult;

/// Environment variable that carries the repository-relative path of the file
/// whose staged content is streamed through a project filter.
const FILE_PATH_ENV: &str = "TAUQE_FILE_PATH";

/// A program together with its literal argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandLine {
    program: String,
    args: Vec<String>,
}

impl CommandLine {
    /// Builds a command from an already split program and arguments.
    pub(super) fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// Builds a command from a configuration line honoring shell-style quoting.
    pub(super) fn tokenized(line: &str) -> Self {
        let mut tokens = tokenize_command(line);
        if tokens.is_empty() {
            return Self {
                program: String::new(),
                args: Vec::new(),
            };
        }
        let program = tokens.remove(0);
        Self {
            program,
            args: tokens,
        }
    }

    /// Executable to spawn.
    pub(super) fn program(&self) -> &str {
        &self.program
    }

    /// Literal arguments, never interpreted by a shell.
    pub(super) fn args(&self) -> &[String] {
        &self.args
    }

    /// How the command is reported in logs and result messages.
    pub(super) fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// Runs a command in `repo_root` and records its duration and outcome.
///
/// `stdin_payload` supplies repository-relative paths to verification and
/// post-hooks. Without it the child receives no input from the daemon.
pub(super) async fn run_command(
    repo_root: &Path,
    command: &CommandLine,
    timeout_secs: u64,
    stdin_payload: Option<&str>,
    cancel_rx: Option<&HardCancellation>,
) -> ToolchainCheckResult {
    run_command_with_operations(
        repo_root,
        command,
        timeout_secs,
        stdin_payload,
        cancel_rx,
        None,
    )
    .await
}

pub(super) async fn run_command_with_operations(
    repo_root: &Path,
    command: &CommandLine,
    timeout_secs: u64,
    stdin_payload: Option<&str>,
    cancel_rx: Option<&HardCancellation>,
    operations: Option<&StartedOperations>,
) -> ToolchainCheckResult {
    tracing::info!(
        command = %command.display(),
        timeout_secs,
        stdin_bytes = stdin_payload.map(str::len).unwrap_or(0),
        "toolchain command started"
    );
    let started = std::time::Instant::now();
    let result = run_command_core(
        repo_root,
        command,
        timeout_secs,
        stdin_payload,
        cancel_rx,
        operations,
    )
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    log_toolchain_check_result(&result, duration_ms);
    result
}

async fn run_command_core(
    repo_root: &Path,
    command: &CommandLine,
    timeout_secs: u64,
    stdin_payload: Option<&str>,
    cancel_rx: Option<&HardCancellation>,
    operations: Option<&StartedOperations>,
) -> ToolchainCheckResult {
    let display = command.display();
    if command.program().is_empty() {
        return ToolchainCheckResult {
            success: true,
            is_cancelled: false,
            command: display,
            stdout: String::new(),
            stderr: String::new(),
            combined_output: String::new(),
            diagnostics: Vec::new(),
        };
    }

    if let Some(rx) = cancel_rx {
        if rx.is_cancelled() {
            return cancelled_result(&display);
        }
    }

    let mut cmd = Command::new(command.program());
    cmd.args(command.args()).current_dir(repo_root);
    let payload = stdin_payload.map(str::as_bytes).map(<[u8]>::to_vec);
    let mut running = match operations {
        Some(operations) => {
            match process::start_owned(
                cmd,
                Duration::from_secs(timeout_secs),
                payload,
                operations,
                cancel_rx.cloned(),
            ) {
                Ok(running) => running,
                Err(error) => return failed_result(display, error.to_string()),
            }
        }
        None => process::start(
            cmd,
            Duration::from_secs(timeout_secs),
            payload,
            cancel_rx.cloned(),
        ),
    };
    let output = match running.wait_with_cancel(cancel_rx).await {
        Ok(output) => output,
        Err(error) => return failed_result(display, error.to_string()),
    };
    let end = output.end();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if let Some(error) = output.error {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&error);
    }
    match end {
        ProcessEnd::Cancelled => return cancelled_result(&display),
        ProcessEnd::TimedOut => {
            stderr.push_str(&format!("Execution timed out ({}s)", timeout_secs))
        }
        ProcessEnd::Exited | ProcessEnd::Unconfirmed => {}
    }
    let mut combined_output = stdout.clone();
    if !stderr.is_empty() {
        if !combined_output.is_empty() && !combined_output.ends_with('\n') {
            combined_output.push('\n');
        }
        combined_output.push_str(&stderr);
    }
    ToolchainCheckResult {
        success: end == ProcessEnd::Exited && output.code == Some(0),
        is_cancelled: false,
        command: display,
        stdout,
        stderr,
        combined_output,
        diagnostics: Vec::new(),
    }
}

fn failed_result(command: String, error: String) -> ToolchainCheckResult {
    ToolchainCheckResult {
        success: false,
        is_cancelled: false,
        command,
        stdout: String::new(),
        combined_output: error.clone(),
        stderr: error,
        diagnostics: Vec::new(),
    }
}

/// Builds the result of a command that never ran because the turn was cancelled.
fn cancelled_result(display: &str) -> ToolchainCheckResult {
    ToolchainCheckResult {
        success: false,
        is_cancelled: true,
        command: display.to_string(),
        stdout: String::new(),
        stderr: "Execution cancelled".to_string(),
        combined_output: format!("Command '{}' was cancelled", display),
        diagnostics: Vec::new(),
    }
}

/// Outcome of a command that received its input on standard input.
///
/// The rewritten content comes back on standard output, so a caller keeps the
/// authored text whenever the command fails, times out, or is cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamingOutcome {
    /// Whether the command exited with a success status.
    pub(super) success: bool,
    /// Whether the invocation was interrupted before it finished.
    pub(super) is_cancelled: bool,
    /// Whether the command exceeded its wall-clock budget.
    pub(super) timed_out: bool,
    /// Captured standard output, which is the rewritten content.
    pub(super) stdout: String,
    /// Captured standard error, which is the failure reason.
    pub(super) stderr: String,
}

/// Runs a command as a stream filter: the input arrives on its standard input
/// and the rewritten text comes back on its standard output.
///
/// Nothing is read from or written to the file system, so a formatter sees only
/// the staged content of the file. Like [`run_command`], it awaits confirmed
/// shutdown of the process group before accepting the rewritten text.
///
/// `path` is the repository-relative path of the file whose staged content is
/// streamed, and it reaches the child through [`FILE_PATH_ENV`], so a filter can
/// dispatch on the file type without parsing a single byte of the content. An
/// empty path describes a caller that knows no repository path, so the variable
/// stays unset and the child sees no file context at all.
pub(super) async fn run_streaming_command(
    repo_root: &Path,
    command: &CommandLine,
    path: &str,
    input: &str,
    timeout_secs: u64,
    cancel_rx: Option<&HardCancellation>,
) -> StreamingOutcome {
    run_streaming_command_with_operations(
        repo_root,
        command,
        path,
        input,
        timeout_secs,
        cancel_rx,
        None,
    )
    .await
}

pub(super) async fn run_streaming_command_with_operations(
    repo_root: &Path,
    command: &CommandLine,
    path: &str,
    input: &str,
    timeout_secs: u64,
    cancel_rx: Option<&HardCancellation>,
    operations: Option<&StartedOperations>,
) -> StreamingOutcome {
    tracing::info!(
        command = %command.display(),
        timeout_secs,
        input_bytes = input.len(),
        "streaming command started"
    );

    if command.program().is_empty() {
        return StreamingOutcome {
            success: true,
            is_cancelled: false,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
        };
    }

    if cancel_rx.is_some_and(HardCancellation::is_cancelled) {
        return cancelled_streaming_result();
    }

    let mut cmd = Command::new(command.program());
    cmd.args(command.args()).current_dir(repo_root);
    if !path.is_empty() {
        cmd.env(FILE_PATH_ENV, path);
    }
    let payload = Some(input.as_bytes().to_vec());
    let mut running = match operations {
        Some(operations) => {
            match process::start_owned(
                cmd,
                Duration::from_secs(timeout_secs),
                payload,
                operations,
                cancel_rx.cloned(),
            ) {
                Ok(running) => running,
                Err(error) => {
                    return StreamingOutcome {
                        success: false,
                        is_cancelled: false,
                        timed_out: false,
                        stdout: String::new(),
                        stderr: error.to_string(),
                    }
                }
            }
        }
        None => process::start(
            cmd,
            Duration::from_secs(timeout_secs),
            payload,
            cancel_rx.cloned(),
        ),
    };
    let output = match running.wait_with_cancel(cancel_rx).await {
        Ok(output) => output,
        Err(error) => {
            return StreamingOutcome {
                success: false,
                is_cancelled: false,
                timed_out: false,
                stdout: String::new(),
                stderr: error.to_string(),
            }
        }
    };
    let end = output.end();
    if end == ProcessEnd::Cancelled {
        return cancelled_streaming_result();
    }
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if let Some(error) = output.error {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&error);
    }
    if end == ProcessEnd::TimedOut {
        stderr.push_str(&format!("Execution timed out ({}s)", timeout_secs));
    }
    StreamingOutcome {
        success: end == ProcessEnd::Exited && output.code == Some(0),
        is_cancelled: false,
        timed_out: end == ProcessEnd::TimedOut,
        stdout: if end == ProcessEnd::Exited {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            String::new()
        },
        stderr,
    }
}

/// Builds the outcome of a stream filter that never ran because the turn was cancelled.
fn cancelled_streaming_result() -> StreamingOutcome {
    StreamingOutcome {
        success: false,
        is_cancelled: true,
        timed_out: false,
        stdout: String::new(),
        stderr: "Execution cancelled".to_string(),
    }
}

/// Splits a command line into program and arguments honoring shell-style quoting.
///
/// Custom verification commands frequently carry quoted arguments
/// (`make test ARGS='a b'`) that a plain whitespace split tears apart. This
/// tokenizer understands single quotes, double quotes, and backslash escapes so
/// the intended argument boundaries reach the process. An unmatched quote or a
/// dangling escape is taken literally rather than treated as fatal, keeping
/// malformed configuration from aborting the pipeline.
fn tokenize_command(input: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote = Quote::None;
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    has_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    has_token = true;
                }
                '\\' => {
                    match chars.next() {
                        Some(escaped) => current.push(escaped),
                        None => current.push('\\'),
                    }
                    has_token = true;
                }
                ch if ch.is_whitespace() => {
                    if has_token {
                        tokens.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                }
                ch => {
                    current.push(ch);
                    has_token = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => match chars.next() {
                    Some(escaped) => current.push(escaped),
                    None => current.push('\\'),
                },
                ch => current.push(ch),
            },
        }
    }

    if has_token {
        tokens.push(current);
    }

    tokens
}

/// How long a heartbeat fixture is observed after a process-group kill or a
/// cancellation.
///
/// The fixtures append one tick every 100 ms, so a writer that survived the
/// teardown adds several ticks inside this window and changes the log size.
/// Watching the log for this window proves the teardown in far less wall-clock
/// time than a fixed multi-second sleep.
#[cfg(test)]
pub(super) const PROCESS_GROUP_STABILITY_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(400);

#[cfg(test)]
mod tests {
    use super::super::run_toolchain_check_with_timeout;
    use super::*;
    use crate::stop::StopSource;
    use tempfile::tempdir;

    #[test]
    fn test_tokenize_command_splits_plain_arguments() {
        assert_eq!(
            tokenize_command("cargo test -- --nocapture"),
            vec!["cargo", "test", "--", "--nocapture"]
        );
    }

    #[test]
    fn test_tokenize_command_preserves_quoted_arguments() {
        assert_eq!(
            tokenize_command("make test ARGS='a b'"),
            vec!["make", "test", "ARGS=a b"]
        );
        assert_eq!(
            tokenize_command("echo \"double quoted\""),
            vec!["echo", "double quoted"]
        );
        assert_eq!(tokenize_command("run 'a b' c"), vec!["run", "a b", "c"]);
    }

    #[test]
    fn test_tokenize_command_handles_escapes_empty_and_unmatched_quotes() {
        assert_eq!(tokenize_command("a\\ b"), vec!["a b"]);
        assert_eq!(tokenize_command(""), Vec::<String>::new());
        assert_eq!(tokenize_command("   "), Vec::<String>::new());
        assert_eq!(tokenize_command("''"), vec![""]);
        assert_eq!(
            tokenize_command("git commit -m 'unterminated"),
            vec!["git", "commit", "-m", "unterminated"]
        );
    }

    #[test]
    fn test_command_line_tokenized_splits_program_from_arguments() {
        let command = CommandLine::tokenized("npm run lint");
        assert_eq!(command.program(), "npm");
        assert_eq!(command.args(), ["run".to_string(), "lint".to_string()]);
        assert_eq!(command.display(), "npm run lint");

        let empty = CommandLine::tokenized("   ");
        assert!(empty.program().is_empty());
        assert!(empty.args().is_empty());
    }

    #[test]
    fn test_command_line_passes_literal_arguments_untouched() {
        let command = CommandLine::new(
            "printf",
            vec!["[%s]".to_string(), "src/a b;rm -rf.rs".to_string()],
        );
        let dir = tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(run_command(dir.path(), &command, 10, None, None));
        assert!(result.success, "command failed: {}", result.combined_output);
        assert_eq!(result.stdout, "[src/a b;rm -rf.rs]");
    }

    #[tokio::test]
    async fn test_a_command_that_ignores_its_stdin_still_completes() {
        let dir = tempdir().unwrap();
        let command = CommandLine::tokenized("git --version");

        // A command is free to exit without reading the change set; the broken
        // pipe must stay a diagnostic instead of failing or hanging the turn.
        let result =
            run_command(dir.path(), &command, 10, Some("a.rs\ngenerated.rs\n"), None).await;

        assert!(result.success, "command failed: {}", result.combined_output);
        assert!(result.stdout.contains("git version"));
    }

    #[tokio::test]
    async fn test_run_toolchain_check_valid_command() {
        let dir = tempdir().unwrap();
        let res = super::super::run_toolchain_check(dir.path(), "git --version").await;
        assert!(res.success);
        assert!(res.stdout.contains("git version"));
    }

    #[tokio::test]
    async fn test_run_toolchain_check_with_timeout_preserves_quoted_arguments() {
        let dir = tempdir().unwrap();
        let res =
            run_toolchain_check_with_timeout(dir.path(), "printf '%s' 'a b c'", 10, None).await;
        assert!(res.success, "command failed: {}", res.combined_output);
        assert_eq!(res.stdout, "a b c");
    }

    #[tokio::test]
    async fn test_run_toolchain_check_with_timeout_pre_cancelled() {
        let dir = tempdir().unwrap();
        let source = StopSource::new();
        source.request_hard();
        let cancel = source.token().hard_cancellation();
        let res =
            run_toolchain_check_with_timeout(dir.path(), "git --version", 10, Some(&cancel)).await;
        assert!(!res.success);
        assert!(res.is_cancelled);
        assert!(res.combined_output.contains("was cancelled"));
    }

    #[tokio::test]
    async fn test_run_toolchain_check_cancellation_during_execution() {
        let dir = tempdir().unwrap();
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_toolchain_check_with_timeout(&root, "sleep 10", 10, Some(&cancel)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        source.request_hard();
        let res = task.await.unwrap();
        assert!(!res.success);
        assert!(res.is_cancelled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_toolchain_check_timeout_kills_entire_process_group() {
        let dir = tempdir().unwrap();
        // The direct child spawns a grandchild that keeps a heartbeat alive. A
        // timeout must tear down the whole process group, so the grandchild has
        // to stop writing as well.
        let command =
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & wait'";
        let res = run_toolchain_check_with_timeout(dir.path(), command, 1, None).await;
        assert!(!res.success, "expected a timeout: {}", res.combined_output);
        assert!(
            res.combined_output.contains("timed out"),
            "output: {}",
            res.combined_output
        );

        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_kill = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        let size_later = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after the process-group kill"
        );
    }

    /// A cancelled turn tears down the whole process group of the verification
    /// command: the direct child and every descendant it spawned.
    ///
    /// The verification command leads its own process group, so the kill that
    /// follows the cancellation signal must reach the grandchildren too. The
    /// heartbeat is written by a grandchild of the direct child, and it stops
    /// growing once the turn is cancelled.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cancellation_kills_entire_process_group() {
        let dir = tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat.log");
        let command =
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & wait'";
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_command(
                &root,
                &CommandLine::tokenized(command),
                30,
                None,
                Some(&cancel),
            )
            .await
        });

        // Cancel only once the grandchild is demonstrably writing, so the halt
        // of the heartbeat proves the teardown instead of a slow start.
        let mut writing = false;
        for _ in 0..100 {
            if std::fs::metadata(&heartbeat)
                .map(|meta| meta.len())
                .unwrap_or(0)
                > 0
            {
                writing = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(writing, "the grandchild never wrote");
        source.request_hard();

        let res = task.await.unwrap();
        assert!(!res.success);
        assert!(res.is_cancelled);
        assert!(res.combined_output.contains("was cancelled"));

        let size_at_kill = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        let size_later = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after the cancellation"
        );
    }

    /// A cancelled turn stops waiting for a pipe a lingering descendant holds.
    ///
    /// The direct child can exit long before its descendants do, and a
    /// descendant that inherited the standard output keeps the pipe from
    /// reaching EOF. The drain therefore races the cancellation signal: the
    /// turn ends when it is cancelled, not when the last descendant exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cancellation_releases_a_drain_a_lingering_descendant_holds() {
        let dir = tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat.log");
        let command =
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & sleep 30'";
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_command(
                &root,
                &CommandLine::tokenized(command),
                30,
                None,
                Some(&cancel),
            )
            .await
        });

        for _ in 0..100 {
            if std::fs::metadata(&heartbeat).is_ok_and(|meta| meta.len() > 0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(std::fs::metadata(&heartbeat).is_ok_and(|meta| meta.len() > 0));
        let started = std::time::Instant::now();
        source.request_hard();

        let res = task.await.unwrap();
        assert!(res.is_cancelled, "the drain outlived the cancellation");
        assert!(res.combined_output.contains("was cancelled"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the cancelled turn waited for the lingering descendant: {:?}",
            started.elapsed()
        );
        let size_at_stop = std::fs::metadata(&heartbeat).unwrap().len();
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), size_at_stop);
    }

    /// An exited leader cannot hide a descendant holding its output pipes open.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_command_without_cancellation_is_bounded_by_the_deadline() {
        let dir = tempdir().unwrap();
        let command = CommandLine::tokenized(
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & sleep 0.2; exit 0'",
        );

        let started = std::time::Instant::now();
        let result = run_command(dir.path(), &command, 1, None, None).await;

        assert!(
            result.success,
            "group shutdown must be confirmed: {}",
            result.combined_output
        );
        assert!(!result.is_cancelled);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(4000),
            "the turn waited for the pipe-holding descendant: {:?}",
            started.elapsed()
        );
        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_stop = std::fs::metadata(&heartbeat)
            .expect("descendant started")
            .len();
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), size_at_stop);
    }
    /// A stream filter round-trips its standard input through standard
    /// output: the staged text that comes back is exactly the bytes the
    /// child emitted for the staged input.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_round_trips_the_staged_text() {
        let dir = tempdir().unwrap();
        let command = CommandLine::tokenized("cat");
        let input = "staged line one\nstaged line two\n";

        let outcome =
            run_streaming_command(dir.path(), &command, "src/lib.rs", input, 10, None).await;

        assert!(outcome.success, "command failed: {}", outcome.stderr);
        assert!(!outcome.is_cancelled);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.stdout, input);
        assert!(outcome.stderr.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_handles_large_input_and_output() {
        let dir = tempdir().unwrap();
        let input = "staged text\n".repeat(100_000);
        let outcome = run_streaming_command(
            dir.path(),
            &CommandLine::tokenized("cat"),
            "src/lib.rs",
            &input,
            10,
            None,
        )
        .await;
        assert!(outcome.success, "command failed: {}", outcome.stderr);
        assert_eq!(outcome.stdout, input);
    }

    /// A stream filter that exceeds its wall-clock budget reports a timeout,
    /// never a cancellation, and returns no rewritten content.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_times_out_and_returns_no_content() {
        let dir = tempdir().unwrap();
        let command = CommandLine::tokenized("sleep 10");

        let outcome =
            run_streaming_command(dir.path(), &command, "src/lib.rs", "staged", 1, None).await;

        assert!(!outcome.success);
        assert!(outcome.timed_out);
        assert!(!outcome.is_cancelled);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.contains("timed out"));
    }

    /// A pre-cancelled turn never spawns the stream filter: no side effect
    /// of the command can occur, and the outcome reports cancellation, not
    /// failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_pre_cancelled_never_spawns() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join("spawned.marker");
        let command = CommandLine::new("touch", vec![marker.display().to_string()]);
        let source = StopSource::new();
        source.request_hard();
        let cancel = source.token().hard_cancellation();

        let outcome = run_streaming_command(
            dir.path(),
            &command,
            "src/lib.rs",
            "staged",
            10,
            Some(&cancel),
        )
        .await;

        assert!(!outcome.success);
        assert!(outcome.is_cancelled);
        assert!(!outcome.timed_out);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.contains("cancelled"));
        assert!(
            !marker.exists(),
            "a pre-cancelled filter must never spawn its command"
        );
    }

    /// A cancelled stream filter tears down the whole process group it
    /// leads: a grandchild that inherited the pipes and keeps writing stops
    /// together with the direct child.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_cancellation_kills_entire_process_group() {
        let dir = tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat.log");
        let command =
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & wait'";
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_streaming_command(
                &root,
                &CommandLine::tokenized(command),
                "src/lib.rs",
                "staged input\n",
                30,
                Some(&cancel),
            )
            .await
        });

        // Cancel only once the grandchild is demonstrably writing, so the
        // halt of the heartbeat proves the teardown instead of a slow start.
        let mut writing = false;
        for _ in 0..100 {
            if std::fs::metadata(&heartbeat)
                .map(|meta| meta.len())
                .unwrap_or(0)
                > 0
            {
                writing = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(writing, "the grandchild never wrote");
        source.request_hard();

        let outcome = task.await.unwrap();
        assert!(!outcome.success);
        assert!(outcome.is_cancelled);
        assert!(outcome.stdout.is_empty());

        let size_at_kill = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        let size_later = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after the cancellation"
        );
    }

    /// A cancelled stream filter stops waiting for a pipe a lingering
    /// descendant holds.
    ///
    /// The direct child can exit long before its descendants do, and a
    /// descendant that inherited the standard output keeps the pipe from
    /// reaching EOF. The drain therefore races the cancellation signal: the
    /// turn ends when it is cancelled, not when the last descendant exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_cancellation_releases_a_lingering_drain() {
        let dir = tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat.log");
        let command =
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & sleep 30'";
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_streaming_command(
                &root,
                &CommandLine::tokenized(command),
                "src/lib.rs",
                "staged input\n",
                30,
                Some(&cancel),
            )
            .await
        });

        for _ in 0..100 {
            if std::fs::metadata(&heartbeat).is_ok_and(|meta| meta.len() > 0) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(std::fs::metadata(&heartbeat).is_ok_and(|meta| meta.len() > 0));
        let started = std::time::Instant::now();
        source.request_hard();

        let outcome = task.await.unwrap();
        assert!(outcome.is_cancelled, "the drain outlived the cancellation");
        assert!(!outcome.timed_out);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.contains("cancelled"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the cancelled turn waited for the lingering descendant: {:?}",
            started.elapsed()
        );
        let size_at_stop = std::fs::metadata(&heartbeat).unwrap().len();
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), size_at_stop);
    }

    /// An exited filter leader stops descendants before accepting output.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_streaming_command_without_cancellation_is_bounded_by_the_deadline() {
        let dir = tempdir().unwrap();
        let command = CommandLine::tokenized(
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 0.1; done\" & sleep 0.2; exit 0'",
        );

        let started = std::time::Instant::now();
        let outcome =
            run_streaming_command(dir.path(), &command, "src/lib.rs", "staged", 1, None).await;

        assert!(
            outcome.success,
            "group shutdown must be confirmed: {}",
            outcome.stderr
        );
        assert!(!outcome.timed_out);
        assert!(!outcome.is_cancelled);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty(), "stderr: {}", outcome.stderr);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(4000),
            "the turn waited for the pipe-holding descendant: {:?}",
            started.elapsed()
        );
        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_stop = std::fs::metadata(&heartbeat)
            .expect("descendant started")
            .len();
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), size_at_stop);
    }

    #[tokio::test]
    async fn soft_allows_command_and_hard_refuses_spawn() {
        let dir = tempdir().unwrap();
        let source = StopSource::new();
        source.request_soft();
        let cancel = source.token().hard_cancellation();
        let command = CommandLine::new("touch", vec!["marker".into()]);
        assert!(
            run_command(dir.path(), &command, 5, None, Some(&cancel))
                .await
                .success
        );
        assert!(dir.path().join("marker").exists());
        std::fs::remove_file(dir.path().join("marker")).unwrap();
        source.request_hard();
        assert!(
            run_command(dir.path(), &command, 5, None, Some(&cancel))
                .await
                .is_cancelled
        );
        assert!(!dir.path().join("marker").exists());
    }

    #[tokio::test]
    async fn soft_then_hard_cancels_running_command_but_timeout_is_distinct() {
        let dir = tempdir().unwrap();
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_command(
                &root,
                &CommandLine::new("sleep", vec!["30".into()]),
                5,
                None,
                Some(&cancel),
            )
            .await
        });
        source.request_soft();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!task.is_finished());
        source.request_hard();
        assert!(task.await.unwrap().is_cancelled);
        let timeout = run_command(
            dir.path(),
            &CommandLine::new("sleep", vec!["30".into()]),
            0,
            None,
            None,
        )
        .await;
        assert!(!timeout.is_cancelled);
        assert!(timeout.stderr.contains("timed out"));
    }
}
