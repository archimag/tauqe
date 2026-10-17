use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolchainKind {
    Cargo,
    Npm,
    Go,
    Custom(String),
    None,
}

#[derive(Debug, Clone)]
pub struct ToolchainCheckResult {
    pub success: bool,
    pub is_cancelled: bool,
    pub command: String,
    pub stdout: String,
    pub stderr: String,
    pub combined_output: String,
}

use crate::edits::protocol::xml::VerifyTarget;

/// Reads the `scripts` table from `package.json` once so toolchain detection and
/// verification command resolution share a single manifest read.
fn read_package_scripts(repo_root: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let content = std::fs::read_to_string(repo_root.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("scripts")
        .and_then(|scripts| scripts.as_object())
        .cloned()
}

/// Returns the sequence of command strings to execute for the given verification target and toolchain.
///
/// Reads `package.json` once and delegates to `resolve_verification_commands_with_scripts`.
pub fn resolve_verification_commands(
    repo_root: &Path,
    kind: &ToolchainKind,
    target: &VerifyTarget,
) -> Vec<String> {
    let scripts = read_package_scripts(repo_root);
    resolve_verification_commands_with_scripts(repo_root, kind, target, scripts.as_ref())
}

/// Resolves verification commands from a `package.json` `scripts` table already
/// read by the caller, so toolchain detection and command resolution share a
/// single disk read.
fn resolve_verification_commands_with_scripts(
    repo_root: &Path,
    kind: &ToolchainKind,
    target: &VerifyTarget,
    scripts: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<String> {
    match kind {
        ToolchainKind::Cargo => match target {
            VerifyTarget::Check => vec!["cargo check --all-targets".to_string()],
            VerifyTarget::Clippy => vec!["cargo clippy --all-targets -- -D warnings".to_string()],
            VerifyTarget::Test => vec!["cargo test --quiet".to_string()],
            VerifyTarget::All => vec![
                "cargo check --all-targets".to_string(),
                "cargo clippy --all-targets -- -D warnings".to_string(),
                "cargo test --quiet".to_string(),
            ],
        },
        ToolchainKind::Npm => {
            let mut cmds = Vec::new();
            if repo_root.join("package.json").is_file() {
                let has_check = scripts.is_some_and(|s| s.contains_key("check"));
                let has_lint = scripts.is_some_and(|s| s.contains_key("lint"));
                let has_test = scripts.is_some_and(|s| s.contains_key("test"));

                match target {
                    VerifyTarget::Check => {
                        if has_check {
                            cmds.push("npm run check".to_string());
                        } else if repo_root.join("tsconfig.json").is_file() {
                            cmds.push("npx tsc --noEmit".to_string());
                        }
                    }
                    VerifyTarget::Clippy => {
                        if has_lint {
                            cmds.push("npm run lint".to_string());
                        }
                    }
                    VerifyTarget::Test => {
                        if has_test {
                            cmds.push("npm test".to_string());
                        }
                    }
                    VerifyTarget::All => {
                        if has_check {
                            cmds.push("npm run check".to_string());
                        } else if repo_root.join("tsconfig.json").is_file() {
                            cmds.push("npx tsc --noEmit".to_string());
                        }
                        if has_lint {
                            cmds.push("npm run lint".to_string());
                        }
                        if has_test {
                            cmds.push("npm test".to_string());
                        }
                    }
                }
            }
            if cmds.is_empty() {
                cmds.push("npm test".to_string());
            }
            cmds
        }
        ToolchainKind::Go => match target {
            VerifyTarget::Check => vec!["go vet ./...".to_string()],
            VerifyTarget::Clippy => vec!["go vet ./...".to_string()],
            VerifyTarget::Test => vec!["go test ./...".to_string()],
            VerifyTarget::All => vec!["go vet ./...".to_string(), "go test ./...".to_string()],
        },
        ToolchainKind::Custom(cmd) => vec![cmd.clone()],
        ToolchainKind::None => Vec::new(),
    }
}

/// Runs verification pipeline for the given target, stopping on first failure (fail-fast).
///
/// The `[toolchain]` configuration is resolved from the repository so a custom
/// `check_command` and the configured per-command `timeout_secs` are honored
/// instead of always falling back to auto-detection with a hardcoded timeout.
pub async fn run_verification_pipeline(
    repo_root: &Path,
    target: &VerifyTarget,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> (bool, Vec<ToolchainCheckResult>) {
    // Configuration loading and toolchain detection read `tauqe.toml` and
    // `package.json` from disk, so they run off the async executor thread.
    let (commands, timeout_secs) = crate::blocking::run_blocking(|| {
        let config = crate::config::load_toolchain_config(repo_root);
        let scripts = read_package_scripts(repo_root);
        let (kind, _) =
            detect_toolchain_with_scripts(repo_root, config.check_command.as_deref(), scripts.as_ref());
        let commands =
            resolve_verification_commands_with_scripts(repo_root, &kind, target, scripts.as_ref());
        (commands, config.effective_timeout_secs())
    });
    let mut results = Vec::new();

    for cmd in commands {
        let res = run_toolchain_check_with_timeout(repo_root, &cmd, timeout_secs, cancel_rx).await;
        let success = res.success;
        results.push(res);
        if !success {
            return (false, results);
        }
    }

    (true, results)
}

/// Deterministically detects the build/validation toolchain from project files in repo root.
///
/// Reads `package.json` once and delegates to `detect_toolchain_with_scripts`.
pub fn detect_toolchain(
    repo_root: &Path,
    custom_command: Option<&str>,
) -> (ToolchainKind, Option<String>) {
    let scripts = read_package_scripts(repo_root);
    detect_toolchain_with_scripts(repo_root, custom_command, scripts.as_ref())
}

/// Detects the toolchain using a `package.json` `scripts` table already read by
/// the caller, so detection and command resolution share one disk read.
fn detect_toolchain_with_scripts(
    repo_root: &Path,
    custom_command: Option<&str>,
    scripts: Option<&serde_json::Map<String, serde_json::Value>>,
) -> (ToolchainKind, Option<String>) {
    if let Some(cmd) = custom_command {
        if !cmd.trim().is_empty() {
            return (
                ToolchainKind::Custom(cmd.trim().to_string()),
                Some(cmd.trim().to_string()),
            );
        }
    }

    if repo_root.join("Cargo.toml").is_file() {
        return (ToolchainKind::Cargo, Some("cargo check".to_string()));
    }

    if repo_root.join("package.json").is_file() {
        if let Some(scripts) = scripts {
            if scripts.contains_key("check") {
                return (ToolchainKind::Npm, Some("npm run check".to_string()));
            }
            if scripts.contains_key("typecheck") {
                return (ToolchainKind::Npm, Some("npm run typecheck".to_string()));
            }
            if scripts.contains_key("test") {
                return (ToolchainKind::Npm, Some("npm test".to_string()));
            }
        }
        if repo_root.join("tsconfig.json").is_file() {
            return (ToolchainKind::Npm, Some("npx tsc --noEmit".to_string()));
        }
        return (ToolchainKind::Npm, Some("npm test".to_string()));
    }

    if repo_root.join("go.mod").is_file() {
        return (ToolchainKind::Go, Some("go vet ./...".to_string()));
    }

    (ToolchainKind::None, None)
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

/// Maximum number of diagnostic `error` lines captured in a compact failure summary.
const FAILURE_SUMMARY_MAX_ERROR_LINES: usize = 5;
/// Maximum number of distinct source paths captured in a compact failure summary.
const FAILURE_SUMMARY_MAX_PATHS: usize = 10;
/// Maximum characters captured from a single diagnostic line.
const FAILURE_SUMMARY_LINE_CHARS: usize = 200;

/// Records the outcome of a single verification command as a structured log record.
fn log_toolchain_check_result(result: &ToolchainCheckResult, duration_ms: u64) {
    if result.is_cancelled {
        tracing::info!(
            command = %result.command,
            duration_ms,
            cancelled = true,
            "toolchain command cancelled"
        );
    } else if result.success {
        tracing::info!(
            command = %result.command,
            duration_ms,
            success = true,
            "toolchain command finished"
        );
    } else {
        tracing::warn!(
            command = %result.command,
            duration_ms,
            success = false,
            summary = %summarize_toolchain_failure(result),
            "toolchain command failed"
        );
    }
}

/// Builds a compact, UTF-8-safe summary of a failed verification command.
///
/// The summary records the number of diagnostic `error` lines, a small number of
/// leading error lines truncated on a Unicode scalar boundary, and the distinct
/// source paths referenced by the output. The full `combined_output` is never
/// embedded, so raw build noise and source fragments stay out of the log.
fn summarize_toolchain_failure(result: &ToolchainCheckResult) -> String {
    let output = if result.combined_output.trim().is_empty() {
        result.stderr.trim()
    } else {
        result.combined_output.trim()
    };

    let error_lines: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("error"))
        .collect();

    let mut summary = format!(
        "command='{}' error_lines={}",
        result.command,
        error_lines.len()
    );

    if error_lines.is_empty() {
        if let Some(first) = output.lines().find(|line| !line.trim().is_empty()) {
            summary.push_str("; first_line=");
            summary.push_str(&truncate_on_char_boundary(
                first.trim(),
                FAILURE_SUMMARY_LINE_CHARS,
            ));
        }
    } else {
        let head: Vec<String> = error_lines
            .iter()
            .take(FAILURE_SUMMARY_MAX_ERROR_LINES)
            .map(|line| truncate_on_char_boundary(line.trim(), FAILURE_SUMMARY_LINE_CHARS))
            .collect();
        summary.push_str("; first_errors=");
        summary.push_str(&head.join(" | "));
    }

    let paths = extract_failure_paths(output);
    if !paths.is_empty() {
        summary.push_str("; error_paths=");
        summary.push_str(&paths.join(","));
    }

    summary
}

/// Truncates a single line on a Unicode scalar boundary, appending an ellipsis when clipped.
fn truncate_on_char_boundary(line: &str, max_chars: usize) -> String {
    let mut truncated: String = line.chars().take(max_chars).collect();
    if line.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

/// Extracts distinct source paths referenced in compiler, linter, or test output.
fn extract_failure_paths(output: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();

    for line in output.lines() {
        for word in line.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| {
                matches!(c, '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>')
            });
            let candidate = trimmed.strip_prefix("./").unwrap_or(trimmed);
            let path_part = candidate.split(':').next().unwrap_or("");
            if is_source_path(path_part)
                && seen.insert(path_part.to_string())
                && paths.len() < FAILURE_SUMMARY_MAX_PATHS
            {
                paths.push(path_part.to_string());
            }
        }
    }

    paths
}

/// Reports whether a token looks like a repository-relative source path.
fn is_source_path(candidate: &str) -> bool {
    if candidate.is_empty() || !candidate.contains('/') {
        return false;
    }
    candidate.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext,
            "rs" | "go"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "py"
                | "rb"
                | "java"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "cxx"
                | "hpp"
                | "cs"
                | "kt"
                | "swift"
        )
    })
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

async fn wait_for_cancel(mut rx: watch::Receiver<bool>) {
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
    tracing::info!(command = %command_str, timeout_secs, "toolchain command started");
    let started = std::time::Instant::now();
    let result = run_toolchain_check_core(repo_root, command_str, timeout_secs, cancel_rx).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    log_toolchain_check_result(&result, duration_ms);
    result
}

async fn run_toolchain_check_core(
    repo_root: &Path,
    command_str: &str,
    timeout_secs: u64,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> ToolchainCheckResult {
    let tokens = tokenize_command(command_str);
    let Some((program, args)) = tokens.split_first() else {
        return ToolchainCheckResult {
            success: true,
            is_cancelled: false,
            command: command_str.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: String::new(),
        };
    };

    if let Some(rx) = cancel_rx {
        if *rx.borrow() {
            return ToolchainCheckResult {
                success: false,
                is_cancelled: true,
                command: command_str.to_string(),
                stdout: String::new(),
                stderr: "Execution cancelled".to_string(),
                combined_output: format!("Command '{}' was cancelled", command_str),
            };
        }
    }

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
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
                command: command_str.to_string(),
                stdout: String::new(),
                stderr: err.to_string(),
                combined_output: format!("Failed to run command '{}': {}", command_str, err),
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
                        command: command_str.to_string(),
                        stdout,
                        stderr,
                        combined_output: combined,
                    }
                }
                Err(err) => ToolchainCheckResult {
                    success: false,
                    is_cancelled: false,
                    command: command_str.to_string(),
                    stdout: String::new(),
                    stderr: err.to_string(),
                    combined_output: format!("Failed to wait for command '{}': {}", command_str, err),
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
                command: command_str.to_string(),
                stdout: String::new(),
                stderr: format!("Execution timed out ({}s)", timeout_secs),
                combined_output: format!("Command '{}' timed out after {}s", command_str, timeout_secs),
            }
        }
        RaceOutcome::Cancelled => {
            kill_and_reap(&mut child, child_pid).await;
            stdout_task.abort();
            stderr_task.abort();
            ToolchainCheckResult {
                success: false,
                is_cancelled: true,
                command: command_str.to_string(),
                stdout: String::new(),
                stderr: "Execution cancelled".to_string(),
                combined_output: format!("Command '{}' was cancelled", command_str),
            }
        }
    }
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
async fn kill_and_reap(child: &mut tokio::process::Child, pid: Option<u32>) {
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
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_detect_toolchain_cargo() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let (kind, cmd) = detect_toolchain(dir.path(), None);
        assert_eq!(kind, ToolchainKind::Cargo);
        assert_eq!(cmd, Some("cargo check".to_string()));
    }

    #[test]
    fn test_detect_toolchain_npm_with_script() {
        let dir = tempdir().unwrap();
        let pkg = r#"{"name":"test","scripts":{"typecheck":"tsc"}}"#;
        std::fs::write(dir.path().join("package.json"), pkg).unwrap();
        let (kind, cmd) = detect_toolchain(dir.path(), None);
        assert_eq!(kind, ToolchainKind::Npm);
        assert_eq!(cmd, Some("npm run typecheck".to_string()));
    }

    #[test]
    fn test_detect_toolchain_custom() {
        let dir = tempdir().unwrap();
        let (kind, cmd) = detect_toolchain(dir.path(), Some("make test"));
        assert_eq!(kind, ToolchainKind::Custom("make test".to_string()));
        assert_eq!(cmd, Some("make test".to_string()));
    }

    #[test]
    fn test_detect_toolchain_none() {
        let dir = tempdir().unwrap();
        let (kind, cmd) = detect_toolchain(dir.path(), None);
        assert_eq!(kind, ToolchainKind::None);
        assert_eq!(cmd, None);
    }

    #[test]
    fn test_resolve_verification_commands_cargo() {
        let dir = tempdir().unwrap();
        let cmds = resolve_verification_commands(
            dir.path(),
            &ToolchainKind::Cargo,
            &VerifyTarget::Clippy,
        );
        assert_eq!(cmds, vec!["cargo clippy --all-targets -- -D warnings"]);

        let test_cmds = resolve_verification_commands(
            dir.path(),
            &ToolchainKind::Cargo,
            &VerifyTarget::Test,
        );
        assert_eq!(test_cmds, vec!["cargo test --quiet"]);
    }

    #[tokio::test]
    async fn test_run_toolchain_check_valid_command() {
        let dir = tempdir().unwrap();
        let res = run_toolchain_check(dir.path(), "git --version").await;
        assert!(res.success);
        assert!(res.stdout.contains("git version"));
    }

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

    #[tokio::test]
    async fn test_run_toolchain_check_with_timeout_preserves_quoted_arguments() {
        let dir = tempdir().unwrap();
        let res = run_toolchain_check_with_timeout(dir.path(), "printf '%s' 'a b c'", 10, None).await;
        assert!(res.success, "command failed: {}", res.combined_output);
        assert_eq!(res.stdout, "a b c");
    }

    #[tokio::test]
    async fn test_run_toolchain_check_with_timeout_pre_cancelled() {
        let dir = tempdir().unwrap();
        let (cancel_tx, cancel_rx) = watch::channel(true);
        drop(cancel_tx);
        let res = run_toolchain_check_with_timeout(dir.path(), "git --version", 10, Some(&cancel_rx)).await;
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

    #[test]
    fn test_summarize_toolchain_failure_is_compact_and_utf8_safe() {
        let output = "\
error: used `unwrap()` on an `Option` value
  --> crates/core/src/lib.rs:10:5
   |
10 |     let x = y.unwrap();
   |             ^^^^^^^^^^

error: mismatched types
  --> crates/core/src/toolchain.rs:20:9

error: aborting due to 2 previous errors
warning: unused import: `std::fmt`
";
        let result = ToolchainCheckResult {
            success: false,
            is_cancelled: false,
            command: "cargo check --all-targets".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: output.to_string(),
        };
        let summary = summarize_toolchain_failure(&result);
        assert!(summary.contains("error_lines=3"), "summary: {summary}");
        assert!(summary.contains("crates/core/src/lib.rs"), "summary: {summary}");
        assert!(
            summary.contains("crates/core/src/toolchain.rs"),
            "summary: {summary}"
        );
        assert!(!summary.contains("unused import"), "summary: {summary}");
    }

    #[test]
    fn test_summarize_toolchain_failure_truncates_multibyte_on_boundary() {
        let long_line = "error: ".to_string() + &"ошибка ".repeat(100);
        let result = ToolchainCheckResult {
            success: false,
            is_cancelled: false,
            command: "cargo build".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: long_line,
        };
        let summary = summarize_toolchain_failure(&result);
        assert!(summary.contains("first_errors="));
        assert!(summary.contains('…'));
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
