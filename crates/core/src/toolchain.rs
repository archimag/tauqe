use serde::{Deserialize, Serialize};
use std::path::Path;
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

/// Returns the sequence of command strings to execute for the given verification target and toolchain.
pub fn resolve_verification_commands(
    repo_root: &Path,
    kind: &ToolchainKind,
    target: &VerifyTarget,
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
                if let Ok(content) = std::fs::read_to_string(repo_root.join("package.json")) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        let scripts = json.get("scripts").and_then(|s| s.as_object());
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
    let config = crate::config::load_toolchain_config(repo_root);
    let (kind, _) = detect_toolchain(repo_root, config.check_command.as_deref());
    let commands = resolve_verification_commands(repo_root, &kind, target);
    let timeout_secs = config.effective_timeout_secs();
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
pub fn detect_toolchain(
    repo_root: &Path,
    custom_command: Option<&str>,
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
        if let Ok(content) = std::fs::read_to_string(repo_root.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
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
pub async fn run_toolchain_check_with_timeout(
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

    let child = match cmd.spawn() {
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
    let cancel_fut = async {
        if let Some(rx) = cancel_rx {
            wait_for_cancel(rx.clone()).await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        output_res = child.wait_with_output() => {
            match output_res {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
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
                        success: output.status.success(),
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
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            if let Some(pid) = child_pid {
                kill_process_group(pid);
            }
            ToolchainCheckResult {
                success: false,
                is_cancelled: false,
                command: command_str.to_string(),
                stdout: String::new(),
                stderr: format!("Execution timed out ({}s)", timeout_secs),
                combined_output: format!("Command '{}' timed out after {}s", command_str, timeout_secs),
            }
        }
        _ = cancel_fut => {
            if let Some(pid) = child_pid {
                kill_process_group(pid);
            }
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
}
