//! In-memory project formatting and best-effort formatter execution.
//!
//! Formatting has two contours, and neither lets a half-authored change set
//! escape verification:
//!
//! * `[project.format]` declares one stream filter. It receives the staged text
//!   of a file on standard input and returns the rewritten text on standard
//!   output, so the change set is formatted while it still lives in virtual
//!   staging. The filter runs in its own process group, and a non-zero exit
//!   status, a timeout, or a cancellation keeps the authored content.
//! * `[toolchain.fmt.<name>]` entries run after a verified build over the
//!   changed files whose extension the entry lists, and receive those paths as
//!   literal process arguments. That phase stays best-effort: a formatter
//!   failure is reported and never aborts the turn or invalidates verified code.

use std::collections::BTreeMap;
use std::path::Path;

use tauqe_protocol::ProjectConfig;
use tokio::sync::watch;

use super::exec::{run_command, run_streaming_command, CommandLine, StreamingOutcome};
use super::ToolchainCheckResult;
use crate::config::{FormatterConfig, DEFAULT_TOOLCHAIN_TIMEOUT_SECS};

/// Outcome of streaming one file's staged content through `[project.format]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryFormatOutcome {
    /// Whether the declared command rewrote the content.
    pub formatted: bool,
    /// Content to keep: the rewritten text on success, the authored text
    /// whenever the command failed, timed out, or was cancelled.
    pub content: String,
    /// Command line that was invoked.
    pub command: String,
    /// Whether the invocation was interrupted before it finished.
    pub is_cancelled: bool,
    /// Compact failure reason when the content could not be rewritten.
    pub error: Option<String>,
}

/// Streams `content` through the declared `[project.format]` command.
///
/// The command receives the staged text on standard input and returns the
/// rewritten text on standard output, so the file is formatted while its change
/// set still lives in virtual staging: no byte reaches disk and the filter can
/// never touch the working tree. The child leads its own process group, so a
/// timeout or a cancellation tears down every process the formatter spawned.
///
/// Returns `None` when the repository declares no in-memory formatter, so the
/// caller keeps its own formatting path. A non-zero exit status, a timeout, a
/// configuration that cannot be resolved, or a cancelled invocation keeps the
/// authored content and reports why.
pub async fn format_staged_content(
    repo_root: &Path,
    content: &str,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Option<InMemoryFormatOutcome> {
    // The `[project]` section is read from `tauqe.toml`, so the load runs off
    // the async executor thread.
    let config_root = repo_root.to_path_buf();
    let project = crate::blocking::run_blocking(|| {
        crate::config::load_section::<ProjectConfig>(&config_root, "project")
    });
    let spec = project.format.as_ref()?;

    let resolved = match project.resolve("format", spec) {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                operation = "format",
                reason = %err,
                "in-memory formatter rejected; keeping the staged content"
            );
            return Some(InMemoryFormatOutcome {
                formatted: false,
                content: content.to_string(),
                command: String::new(),
                is_cancelled: false,
                error: Some(err.to_string()),
            });
        }
    };

    let command = CommandLine::new(resolved.command, resolved.args);
    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_TOOLCHAIN_TIMEOUT_SECS);
    let outcome =
        run_streaming_command(repo_root, &command, content, timeout_secs, cancel_rx).await;

    if outcome.is_cancelled {
        tracing::warn!(
            command = %command.display(),
            "in-memory formatter cancelled; keeping the staged content"
        );
        return Some(InMemoryFormatOutcome {
            formatted: false,
            content: content.to_string(),
            command: command.display(),
            is_cancelled: true,
            error: Some("the in-memory formatter was cancelled".to_string()),
        });
    }

    if !outcome.success {
        let reason = failure_reason(&outcome);
        tracing::warn!(
            command = %command.display(),
            timed_out = outcome.timed_out,
            reason = %reason,
            "in-memory formatter failed; keeping the staged content"
        );
        return Some(InMemoryFormatOutcome {
            formatted: false,
            content: content.to_string(),
            command: command.display(),
            is_cancelled: false,
            error: Some(reason),
        });
    }

    // A filter that emits nothing while the staged text was not empty did not
    // rewrite the code: keeping the authored content is safer than erasing it.
    if outcome.stdout.is_empty() && !content.trim().is_empty() {
        return Some(InMemoryFormatOutcome {
            formatted: false,
            content: content.to_string(),
            command: command.display(),
            is_cancelled: false,
            error: Some(format!(
                "formatter `{}` produced no output",
                command.program()
            )),
        });
    }

    Some(InMemoryFormatOutcome {
        formatted: true,
        content: outcome.stdout,
        command: command.display(),
        is_cancelled: false,
        error: None,
    })
}

/// Builds the compact failure reason reported for a failed stream filter.
fn failure_reason(outcome: &StreamingOutcome) -> String {
    if outcome.timed_out {
        return "the in-memory formatter timed out".to_string();
    }
    let reason = compact(&outcome.stderr);
    if reason.is_empty() {
        "the in-memory formatter exited with a failure status".to_string()
    } else {
        reason
    }
}

/// Collapses a multi-line diagnostic into one line.
fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

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

    /// Creates a temporary repository declaring an in-memory project formatter.
    fn repo_with_project_formatter(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tauqe.toml"), config).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_format_staged_content_needs_a_declared_project_formatter() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            format_staged_content(dir.path(), "fn main() {}\n", None)
                .await
                .is_none(),
            "a repository that declares nothing has no in-memory formatter"
        );

        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[toolchain]\nauto_verify = true\n",
        )
        .unwrap();
        assert!(
            format_staged_content(dir.path(), "fn main() {}\n", None)
                .await
                .is_none(),
            "the disk formatter registry is not an in-memory project formatter"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_staged_content_streams_through_stdin_and_stdout() {
        let dir = repo_with_project_formatter(
            r#"
[project.format]
command = "sh"
args = ["-c", "tr a-z A-Z"]
"#,
        );
        let authored = "pub fn value() -> i32 {0}\n";

        let outcome = format_staged_content(dir.path(), authored, None)
            .await
            .expect("the repository declares an in-memory formatter");

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "PUB FN VALUE() -> I32 {0}\n");
        assert!(!outcome.is_cancelled);
        assert!(outcome.error.is_none());
        assert_eq!(outcome.command, "sh -c tr a-z A-Z");

        // The filter is a pure stream: the content never reaches the disk.
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["tauqe.toml".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_staged_content_keeps_authored_content_on_failure() {
        let dir = repo_with_project_formatter(
            r#"
[project.format]
command = "sh"
args = ["-c", "printf 'mismatched\nbraces' >&2; exit 3"]
"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_staged_content(dir.path(), authored, None)
            .await
            .expect("the repository declares an in-memory formatter");

        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
        let reason = outcome.error.unwrap_or_default();
        assert!(
            reason.contains("mismatched braces"),
            "the reported reason must be a single compact line: {reason}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_staged_content_keeps_authored_content_when_the_filter_is_silent() {
        let dir = repo_with_project_formatter(
            r#"
[project.format]
command = "sh"
args = ["-c", "cat > /dev/null"]
"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_staged_content(dir.path(), authored, None)
            .await
            .expect("the repository declares an in-memory formatter");

        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
        assert!(outcome
            .error
            .unwrap_or_default()
            .contains("produced no output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_staged_content_times_out_and_keeps_authored_content() {
        let dir = repo_with_project_formatter(
            r#"
[project.format]
command = "sh"
args = ["-c", "cat; sh -c 'while true; do echo tick >> heartbeat.log; sleep 0.1; done' & sleep 30"]
timeout_secs = 1
"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_staged_content(dir.path(), authored, None)
            .await
            .expect("the repository declares an in-memory formatter");

        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
        assert!(outcome.error.unwrap_or_default().contains("timed out"));

        // The whole process group is torn down, so the grandchild the formatter
        // spawned stops writing instead of outliving the turn.
        let heartbeat = dir.path().join("heartbeat.log");
        let size_at_kill = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert!(size_at_kill > 0, "the grandchild never wrote");
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        let size_later = std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild survived the process-group kill"
        );
    }

    #[tokio::test]
    async fn test_format_staged_content_reports_cancellation() {
        let dir = repo_with_project_formatter(
            r#"
[project.format]
command = "sh"
args = ["-c", "cat"]
"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let outcome = format_staged_content(dir.path(), authored, Some(&cancel_rx))
            .await
            .expect("the repository declares an in-memory formatter");

        assert!(outcome.is_cancelled);
        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
    }
}
