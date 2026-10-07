use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolchainKind {
    Cargo,
    Npm,
    Go,
    Python,
    Custom(String),
    None,
}

#[derive(Debug, Clone)]
pub struct ToolchainCheckResult {
    pub success: bool,
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
            VerifyTarget::Test => vec!["cargo test".to_string()],
            VerifyTarget::All => vec![
                "cargo check --all-targets".to_string(),
                "cargo clippy --all-targets -- -D warnings".to_string(),
                "cargo test".to_string(),
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
        ToolchainKind::Python => match target {
            VerifyTarget::Check => vec!["python3 -m py_compile".to_string()],
            VerifyTarget::Clippy => vec!["flake8".to_string()],
            VerifyTarget::Test => vec!["pytest".to_string()],
            VerifyTarget::All => vec!["pytest".to_string()],
        },
        ToolchainKind::Custom(cmd) => vec![cmd.clone()],
        ToolchainKind::None => Vec::new(),
    }
}

/// Runs verification pipeline for the given target, stopping on first failure (fail-fast).
pub async fn run_verification_pipeline(
    repo_root: &Path,
    target: &VerifyTarget,
) -> (bool, Vec<ToolchainCheckResult>) {
    let (kind, _) = detect_toolchain(repo_root, None);
    let commands = resolve_verification_commands(repo_root, &kind, target);
    let mut results = Vec::new();

    for cmd in commands {
        let res = run_toolchain_check(repo_root, &cmd).await;
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

    if repo_root.join("pyproject.toml").is_file()
        || repo_root.join("setup.py").is_file()
        || repo_root.join("requirements.txt").is_file()
    {
        return (
            ToolchainKind::Python,
            Some("python3 -m py_compile".to_string()),
        );
    }

    (ToolchainKind::None, None)
}

/// Executes a toolchain verification command in repo root with a timeout.
pub async fn run_toolchain_check(repo_root: &Path, command_str: &str) -> ToolchainCheckResult {
    let parts: Vec<&str> = command_str.split_whitespace().collect();
    if parts.is_empty() {
        return ToolchainCheckResult {
            success: true,
            command: command_str.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: String::new(),
        };
    }

    let program = parts[0];
    let args = &parts[1..];

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.current_dir(repo_root);

    let output_res = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output()).await;

    match output_res {
        Ok(Ok(output)) => {
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
                command: command_str.to_string(),
                stdout,
                stderr,
                combined_output: combined,
            }
        }
        Ok(Err(err)) => ToolchainCheckResult {
            success: false,
            command: command_str.to_string(),
            stdout: String::new(),
            stderr: err.to_string(),
            combined_output: format!("Failed to run command '{}': {}", command_str, err),
        },
        Err(_) => ToolchainCheckResult {
            success: false,
            command: command_str.to_string(),
            stdout: String::new(),
            stderr: "Execution timed out (60s)".to_string(),
            combined_output: format!("Command '{}' timed out after 60s", command_str),
        },
    }
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
    }

    #[tokio::test]
    async fn test_run_toolchain_check_valid_command() {
        let dir = tempdir().unwrap();
        let res = run_toolchain_check(dir.path(), "git --version").await;
        assert!(res.success);
        assert!(res.stdout.contains("git version"));
    }
}
