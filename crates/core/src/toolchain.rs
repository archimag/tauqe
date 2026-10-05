use std::path::Path;
use serde::{Deserialize, Serialize};

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

    let output_res = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        cmd.output(),
    )
    .await;

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

    #[tokio::test]
    async fn test_run_toolchain_check_valid_command() {
        let dir = tempdir().unwrap();
        let res = run_toolchain_check(dir.path(), "git --version").await;
        assert!(res.success);
        assert!(res.stdout.contains("git version"));
    }
}
