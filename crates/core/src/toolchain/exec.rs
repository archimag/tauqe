//! Execution of a single project command with timeout and cancellation.
//!
//! A command runs in the repository root with its arguments passed through
//! `Command::args`, so a literal path or argument never reaches a shell. On Unix
//! the child is launched in its own process group, so a timeout or a client
//! cancellation tears down the whole process hierarchy (e.g. `rustc` and the test
//! binaries it spawned) instead of leaving orphans behind.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use super::report::log_toolchain_check_result;
use super::ToolchainCheckResult;

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
pub(super) async fn run_command(
    repo_root: &Path,
    command: &CommandLine,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> ToolchainCheckResult {
    tracing::info!(
        command = %command.display(),
        timeout_secs,
        "toolchain command started"
    );
    let started = std::time::Instant::now();
    let result = run_command_core(repo_root, command, timeout_secs, cancel_rx).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    log_toolchain_check_result(&result, duration_ms);
    result
}

async fn run_command_core(
    repo_root: &Path,
    command: &CommandLine,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
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
        if *rx.borrow() {
            return cancelled_result(&display);
        }
    }

    let mut cmd = tokio::process::Command::new(command.program());
    cmd.args(command.args());
    cmd.current_dir(repo_root);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ToolchainCheckResult {
                success: false,
                is_cancelled: false,
                command: display,
                stdout: String::new(),
                stderr: err.to_string(),
                combined_output: format!("Failed to run command '{}': {}", command.display(), err),
                diagnostics: Vec::new(),
            };
        }
    };

    let child_pid = child.id();

    // Drain the child pipes on dedicated tasks. `Child::wait_with_output` is
    // deliberately avoided: it consumes and reaps the `Child`, so a timeout or a
    // cancellation that wins the race below would leave only a recycled PID to
    // signal. Keeping `child` owned here keeps its PID reserved and lets the
    // whole process group (the direct child and its descendants) be torn down
    // while the child is still alive.
    let stdout_task = spawn_pipe_reader(child.stdout.take());
    let stderr_task = spawn_pipe_reader(child.stderr.take());

    let cancel_fut = async {
        if let Some(rx) = cancel_rx {
            wait_for_cancel(rx.clone()).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    enum RaceOutcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    let outcome = tokio::select! {
        status = child.wait() => RaceOutcome::Exited(status),
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => RaceOutcome::TimedOut,
        _ = cancel_fut => RaceOutcome::Cancelled,
    };

    match outcome {
        RaceOutcome::Exited(status_res) => {
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            match status_res {
                Ok(status) => {
                    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
                    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
                    let mut combined = String::new();
                    if !stdout.is_empty() {
                        combined.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        if !combined.is_empty() && !combined.ends_with('\n') {
                            combined.push('\n');
                        }
                        combined.push_str(&stderr);
                    }

                    ToolchainCheckResult {
                        success: status.success(),
                        is_cancelled: false,
                        command: display,
                        stdout,
                        stderr,
                        combined_output: combined,
                        diagnostics: Vec::new(),
                    }
                }
                Err(err) => ToolchainCheckResult {
                    success: false,
                    is_cancelled: false,
                    command: display,
                    stdout: String::new(),
                    stderr: err.to_string(),
                    combined_output: format!(
                        "Failed to wait for command '{}': {}",
                        command.display(),
                        err
                    ),
                    diagnostics: Vec::new(),
                },
            }
        }
        RaceOutcome::TimedOut => {
            kill_and_reap(&mut child, child_pid).await;
            stdout_task.abort();
            stderr_task.abort();
            ToolchainCheckResult {
                success: false,
                is_cancelled: false,
                command: display.clone(),
                stdout: String::new(),
                stderr: format!("Execution timed out ({}s)", timeout_secs),
                combined_output: format!("Command '{}' timed out after {}s", display, timeout_secs),
                diagnostics: Vec::new(),
            }
        }
        RaceOutcome::Cancelled => {
            kill_and_reap(&mut child, child_pid).await;
            stdout_task.abort();
            stderr_task.abort();
            cancelled_result(&display)
        }
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

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    if let Ok(pid_i32) = i32::try_from(pid) {
        if pid_i32 > 1 {
            unsafe {
                let _ = kill(-pid_i32, 9);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

pub(super) async fn wait_for_cancel(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
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
/// the staged content of the file and can never modify the working tree. Like
/// [`run_command`], the child leads its own process group, so a timeout or a
/// cancellation tears down the descendants the command spawned instead of
/// leaving them behind.
pub(super) async fn run_streaming_command(
    repo_root: &Path,
    command: &CommandLine,
    input: &str,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
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

    if cancel_rx.is_some_and(|rx| *rx.borrow()) {
        return cancelled_streaming_result();
    }

    let mut cmd = tokio::process::Command::new(command.program());
    cmd.args(command.args());
    cmd.current_dir(repo_root);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return StreamingOutcome {
                success: false,
                is_cancelled: false,
                timed_out: false,
                stdout: String::new(),
                stderr: err.to_string(),
            };
        }
    };

    let child_pid = child.id();

    // The input is written on its own task: a command that produces a large
    // output before it consumed its whole input would otherwise deadlock the
    // caller against a full stdin pipe.
    let stdin_task = spawn_pipe_writer(child.stdin.take(), input.as_bytes().to_vec());
    let stdout_task = spawn_pipe_reader(child.stdout.take());
    let stderr_task = spawn_pipe_reader(child.stderr.take());

    let cancel_fut = async {
        if let Some(rx) = cancel_rx {
            wait_for_cancel(rx.clone()).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    enum RaceOutcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    let outcome = tokio::select! {
        status = child.wait() => RaceOutcome::Exited(status),
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => RaceOutcome::TimedOut,
        _ = cancel_fut => RaceOutcome::Cancelled,
    };

    match outcome {
        RaceOutcome::Exited(status_res) => {
            // The child is gone, so a pending write can only observe a broken
            // pipe. Aborting the writer also releases the turn when a lingering
            // descendant inherited the pipe.
            stdin_task.abort();
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
            match status_res {
                Ok(status) => StreamingOutcome {
                    success: status.success(),
                    is_cancelled: false,
                    timed_out: false,
                    stdout,
                    stderr,
                },
                Err(err) => StreamingOutcome {
                    success: false,
                    is_cancelled: false,
                    timed_out: false,
                    stdout,
                    stderr: err.to_string(),
                },
            }
        }
        RaceOutcome::TimedOut => {
            kill_and_reap(&mut child, child_pid).await;
            stdin_task.abort();
            stdout_task.abort();
            stderr_task.abort();
            StreamingOutcome {
                success: false,
                is_cancelled: false,
                timed_out: true,
                stdout: String::new(),
                stderr: format!("Execution timed out ({}s)", timeout_secs),
            }
        }
        RaceOutcome::Cancelled => {
            kill_and_reap(&mut child, child_pid).await;
            stdin_task.abort();
            stdout_task.abort();
            stderr_task.abort();
            cancelled_streaming_result()
        }
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

/// Feeds a child pipe from a buffer on a dedicated task.
///
/// Writing the input independently of `Child::wait` keeps a formatter that
/// produces output before it consumed its whole input from deadlocking the
/// caller, and it lets the caller abort the write once the child is gone.
fn spawn_pipe_writer(
    pipe: Option<tokio::process::ChildStdin>,
    data: Vec<u8>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Some(mut pipe) = pipe {
            let _ = pipe.write_all(&data).await;
            let _ = pipe.flush().await;
        }
    })
}

/// Drains a child pipe to EOF on a dedicated task.
///
/// Reading the pipes independently of `Child::wait` lets the caller keep the
/// `Child` handle (and therefore the reserved PID) alive while a timeout or a
/// cancellation runs, and it prevents the child from blocking on a full pipe.
fn spawn_pipe_reader<R>(pipe: Option<R>) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    })
}

/// Tears down and reaps the verification process.
///
/// The process group led by `pid` is signalled first, while the direct child is
/// still un-reaped, so the leader's PID cannot have been recycled and the whole
/// descendant tree is killed. The child is then terminated on every platform and
/// collected so no zombie survives and its PID is released only after the group
/// signal has been delivered.
pub(super) async fn kill_and_reap(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
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

#[cfg(test)]
mod tests {
    use super::super::run_toolchain_check_with_timeout;
    use super::*;
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
        let result = runtime.block_on(run_command(dir.path(), &command, 10, None));
        assert!(result.success, "command failed: {}", result.combined_output);
        assert_eq!(result.stdout, "[src/a b;rm -rf.rs]");
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
        let (cancel_tx, cancel_rx) = watch::channel(true);
        drop(cancel_tx);
        let res =
            run_toolchain_check_with_timeout(dir.path(), "git --version", 10, Some(&cancel_rx))
                .await;
        assert!(!res.success);
        assert!(res.is_cancelled);
        assert!(res.combined_output.contains("was cancelled"));
    }

    #[tokio::test]
    async fn test_run_toolchain_check_cancellation_during_execution() {
        let dir = tempdir().unwrap();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_toolchain_check_with_timeout(&root, "sleep 10", 10, Some(&cancel_rx)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = cancel_tx.send(true);
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
            "sh -c 'sh -c \"while true; do echo tick >> heartbeat.log; sleep 1; done\" & wait'";
        let res = run_toolchain_check_with_timeout(dir.path(), command, 1, None).await;
        assert!(!res.success, "expected a timeout: {}", res.combined_output);
        assert!(
            res.combined_output.contains("timed out"),
            "output: {}",
            res.combined_output
        );

        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_kill = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        let size_later = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after the process-group kill"
        );
    }
}
