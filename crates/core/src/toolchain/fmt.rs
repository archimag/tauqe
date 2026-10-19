//! Best-effort execution of the configured formatter registry.
//!
//! Each `[toolchain.fmt.<name>]` entry runs only for changed files whose
//! extension the entry lists, and receives those paths as literal process
//! arguments, so a file name with spaces or shell metacharacters is never
//! interpreted. The extension filter is the mandatory selection rule of a
//! monorepo: one formatter per language can coexist without touching unrelated
//! files.
//!
//! Formatting runs after a verified build and stays best-effort: a formatter
//! failure is reported and never aborts the turn or invalidates verified code.

use std::collections::BTreeMap;
use std::path::Path;

use tokio::sync::watch;

use super::exec::{run_command, CommandLine};
use super::ToolchainCheckResult;
use crate::config::FormatterConfig;

/// Outcome of one configured formatter invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatterRunResult {
    /// Name of the `[toolchain.fmt.<name>]` entry.
    pub formatter: String,
    /// Command line that was executed.
    pub command: String,
    /// Changed files handed to the formatter.
    pub files: Vec<String>,
    /// Whether the formatter completed successfully.
    pub success: bool,
    /// Whether the invocation was cancelled before it finished.
    pub is_cancelled: bool,
    /// Trimmed command output, or the failure reason.
    pub message: String,
}

/// One formatter invocation prepared from the registry.
struct FormatterInvocation {
    name: String,
    command: CommandLine,
    files: Vec<String>,
    timeout_secs: u64,
}

/// Runs every configured formatter over the changed files it applies to.
///
/// Returns one result per executed formatter in registry order. A missing
/// registry, an empty change set, or a set of changed files no formatter claims
/// yields an empty list. A cancelled invocation terminates the sequence and is
/// reported with `is_cancelled`.
pub async fn run_formatters(
    repo_root: &Path,
    changed_files: &[String],
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Vec<FormatterRunResult> {
    // The formatter registry is read from `tauqe.toml`, so it is loaded off the
    // async executor thread.
    let config_root = repo_root.to_path_buf();
    let registry =
        crate::blocking::run_blocking(|| crate::config::load_toolchain_config(&config_root).fmt);

    let mut results = Vec::new();
    for invocation in plan_invocations(&registry, changed_files) {
        tracing::info!(
            formatter = %invocation.name,
            command = %invocation.command.display(),
            files = invocation.files.len(),
            "formatter started"
        );
        let outcome = run_command(
            repo_root,
            &invocation.command,
            invocation.timeout_secs,
            cancel_rx,
        )
        .await;
        if !outcome.success && !outcome.is_cancelled {
            tracing::warn!(
                formatter = %invocation.name,
                command = %outcome.command,
                "formatter failed; keeping verified code"
            );
        }
        results.push(FormatterRunResult {
            formatter: invocation.name,
            command: outcome.command.clone(),
            files: invocation.files,
            success: outcome.success,
            is_cancelled: outcome.is_cancelled,
            message: outcome_message(&outcome),
        });
        if outcome.is_cancelled {
            break;
        }
    }

    results
}

/// Turns a command outcome into a short report message.
fn outcome_message(outcome: &ToolchainCheckResult) -> String {
    let output = outcome.combined_output.trim();
    if !output.is_empty() {
        return output.to_string();
    }
    let stderr = outcome.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    if outcome.success {
        String::new()
    } else {
        format!("`{}` exited with a failure status", outcome.command)
    }
}

/// Prepares one invocation per usable formatter that claims at least one changed file.
fn plan_invocations(
    registry: &BTreeMap<String, FormatterConfig>,
    changed_files: &[String],
) -> Vec<FormatterInvocation> {
    let mut invocations = Vec::new();
    for (name, formatter) in registry {
        let Some((program, mut args)) = formatter.command_line() else {
            continue;
        };
        let mut files: Vec<String> = Vec::new();
        for file in changed_files {
            let file = file.trim();
            if file.is_empty() || files.iter().any(|existing| existing == file) {
                continue;
            }
            if formatter.matches_path(Path::new(file)) {
                files.push(file.to_string());
            }
        }
        if files.is_empty() {
            continue;
        }
        // Configured arguments come first; file paths follow as literal arguments.
        args.extend(files.iter().cloned());
        invocations.push(FormatterInvocation {
            name: name.clone(),
            command: CommandLine::new(program, args),
            files,
            timeout_secs: formatter.effective_timeout_secs(),
        });
    }
    invocations
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a temporary repository with a `tauqe.toml` formatter registry.
    fn repo_with_formatters(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        dir
    }

    #[test]
    fn test_plan_invocations_filters_extensions_and_appends_paths() {
        let mut registry = BTreeMap::new();
        registry.insert(
            "probe".to_string(),
            FormatterConfig {
                command: Some("printf".to_string()),
                args: vec!["[%s]".to_string()],
                extensions: vec!["rs".to_string(), ".TS".to_string()],
                timeout_secs: Some(45),
            },
        );
        let changed = vec![
            "src/a b;rm.rs".to_string(),
            "src/c.ts".to_string(),
            " notes.txt ".to_string(),
            "src/a b;rm.rs".to_string(),
        ];

        let invocations = plan_invocations(&registry, &changed);
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].files,
            vec!["src/a b;rm.rs".to_string(), "src/c.ts".to_string()]
        );
        assert_eq!(
            invocations[0].command.args(),
            [
                "[%s]".to_string(),
                "src/a b;rm.rs".to_string(),
                "src/c.ts".to_string(),
            ]
        );
        assert_eq!(invocations[0].timeout_secs, 45);
    }

    #[test]
    fn test_plan_invocations_skips_unclaimed_or_unusable_entries() {
        let mut registry = BTreeMap::new();
        registry.insert(
            "go_only".to_string(),
            FormatterConfig {
                command: Some("gofmt".to_string()),
                args: Vec::new(),
                extensions: vec!["go".to_string()],
                timeout_secs: None,
            },
        );
        registry.insert(
            "no_command".to_string(),
            FormatterConfig {
                command: None,
                args: Vec::new(),
                extensions: vec!["rs".to_string()],
                timeout_secs: None,
            },
        );

        let invocations = plan_invocations(&registry, &["src/lib.rs".to_string()]);
        assert!(invocations.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_formatters_passes_literal_paths_without_shell() {
        let dir = repo_with_formatters(
            r#"
[toolchain.fmt.probe]
command = "printf"
args = ["[%s]"]
extensions = ["rs", "ts"]
"#,
        );
        let changed = vec![
            "src/a b;rm -rf.rs".to_string(),
            "src/c.ts".to_string(),
            "notes.txt".to_string(),
        ];

        let results = run_formatters(dir.path(), &changed, None).await;
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.formatter, "probe");
        assert!(result.success, "message: {}", result.message);
        assert!(!result.is_cancelled);
        assert_eq!(
            result.files,
            vec!["src/a b;rm -rf.rs".to_string(), "src/c.ts".to_string()]
        );
        assert_eq!(result.message, "[src/a b;rm -rf.rs][src/c.ts]");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_formatters_reports_failure_and_keeps_going() {
        let dir = repo_with_formatters(
            r#"
[toolchain.fmt.aaa_probe]
command = "printf"
args = ["ok:%s"]
extensions = ["rs"]

[toolchain.fmt.zzz_broken]
command = "sh"
args = ["-c", "exit 3"]
extensions = ["rs"]
"#,
        );

        let results = run_formatters(dir.path(), &["src/lib.rs".to_string()], None).await;
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].formatter, "aaa_probe");
        assert!(results[0].success);
        assert_eq!(results[0].message, "ok:src/lib.rs");

        assert_eq!(results[1].formatter, "zzz_broken");
        assert!(!results[1].success);
        assert!(!results[1].is_cancelled);
    }

    #[tokio::test]
    async fn test_run_formatters_without_registry_does_nothing() {
        let dir = tempfile::tempdir().unwrap();

        let results = run_formatters(dir.path(), &["src/lib.rs".to_string()], None).await;
        assert!(results.is_empty());
    }
}
