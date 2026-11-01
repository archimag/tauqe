//! In-memory project formatting.
//!
//! Formatting has exactly one contour: `[project.format]` declares one stream
//! filter that receives the staged text of a file on standard input and returns
//! the rewritten text on standard output, so the change set is formatted while
//! it still lives in virtual staging. The filter runs in its own process group,
//! and a non-zero exit status, a timeout, or a cancellation keeps the authored
//! content.

use std::path::Path;

use tauqe_protocol::ProjectConfig;
use tokio::sync::watch;

use super::exec::{run_streaming_command, CommandLine, StreamingOutcome};
use super::DEFAULT_PROJECT_TIMEOUT_SECS;

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

/// Streams `content` through the in-memory formatter a project names.
///
/// The filter comes from the resolved project configuration: a declared
/// `[project.format]` entry, or the `format` recipe the declared task runner
/// exports when the entry is omitted. A declaration always keeps precedence over
/// the export of the same role, so a project can pin the filter in `tauqe.toml`
/// or delegate it to its justfile without changing the bounded turn.
///
/// The command receives the staged text on standard input and returns the
/// rewritten text on standard output, so the file is formatted while its change
/// set still lives in virtual staging: no byte reaches disk and the filter can
/// never touch the working tree. The child leads its own process group, so a
/// timeout or a cancellation tears down every process the formatter spawned.
///
/// Returns `None` when the configuration provides no in-memory formatter, so the
/// caller keeps its own formatting path. A non-zero exit status, a timeout, a
/// configuration that cannot be resolved, or a cancelled invocation keeps the
/// authored content and reports why.
pub async fn format_with_project_config(
    repo_root: &Path,
    project: &ProjectConfig,
    content: &str,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Option<InMemoryFormatOutcome> {
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
        .unwrap_or(DEFAULT_PROJECT_TIMEOUT_SECS);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ProjectCommandSpec;

    /// Parses a `[project]` declaration from its JSON form.
    ///
    /// The model is serde-generic, so the JSON form pins the same shape the TOML
    /// reader produces without depending on a TOML parser in this module.
    fn project_config(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the [project] section parses")
    }

    /// Creates a temporary repository the filter runs in.
    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn test_format_with_project_config_needs_a_declared_project_formatter() {
        let dir = repo();
        let project = ProjectConfig::default();

        let outcome =
            format_with_project_config(dir.path(), &project, "fn main() {}\n", None).await;

        assert!(
            outcome.is_none(),
            "a project that declares no in-memory formatter has none"
        );
    }

    /// Builds the manifest a task runner exports for the in-memory filter.
    #[cfg(unix)]
    fn runner_format_manifest(script: &str) -> crate::toolchain::runner::RunnerManifest {
        crate::toolchain::runner::RunnerManifest {
            format: Some(crate::toolchain::runner::RunnerCommand {
                name: "format".to_string(),
                spec: ProjectCommandSpec {
                    command: "sh".to_string(),
                    args: vec!["-c".to_string(), script.to_string()],
                    timeout_secs: None,
                    ..Default::default()
                },
            }),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_with_project_config_uses_the_filter_a_task_runner_exports() {
        let dir = repo();
        // The project declares nothing: the task runner supplies the filter.
        let mut project = ProjectConfig::default();
        crate::toolchain::merge_manifest(&mut project, runner_format_manifest("tr a-z A-Z"));

        let authored = "pub fn value() -> i32 {0}\n";
        let outcome = format_with_project_config(dir.path(), &project, authored, None)
            .await
            .expect("the runner exports an in-memory formatter");

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "PUB FN VALUE() -> I32 {0}\n");
        assert_eq!(outcome.command, "sh -c tr a-z A-Z");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_declared_project_formatter_overrides_the_runner_recipe() {
        let dir = repo();
        let mut project = ProjectConfig {
            format: Some(ProjectCommandSpec {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "tr a-z A-Z".to_string()],
                timeout_secs: None,
                ..Default::default()
            }),
            ..ProjectConfig::default()
        };
        // The exported recipe would erase the stream; the declaration must win.
        crate::toolchain::merge_manifest(&mut project, runner_format_manifest("cat > /dev/null"));

        let outcome = format_with_project_config(dir.path(), &project, "pub fn value() {}\n", None)
            .await
            .expect("the declared formatter is used");

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "PUB FN VALUE() {}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_with_project_config_streams_through_stdin_and_stdout() {
        let dir = repo();
        let project =
            project_config(r#"{"format": {"command": "sh", "args": ["-c", "tr a-z A-Z"]}}"#);
        let authored = "pub fn value() -> i32 {0}\n";

        let outcome = format_with_project_config(dir.path(), &project, authored, None)
            .await
            .expect("the project declares an in-memory formatter");

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
        assert!(entries.is_empty(), "the filter wrote to disk: {entries:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_with_project_config_keeps_authored_content_on_failure() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "printf 'mismatched\nbraces' >&2; exit 3"]}}"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_with_project_config(dir.path(), &project, authored, None)
            .await
            .expect("the project declares an in-memory formatter");

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
    async fn test_format_with_project_config_keeps_authored_content_when_the_filter_is_silent() {
        let dir = repo();
        let project =
            project_config(r#"{"format": {"command": "sh", "args": ["-c", "cat > /dev/null"]}}"#);
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_with_project_config(dir.path(), &project, authored, None)
            .await
            .expect("the project declares an in-memory formatter");

        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
        assert!(outcome
            .error
            .unwrap_or_default()
            .contains("produced no output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_format_with_project_config_times_out_and_keeps_authored_content() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "cat; sh -c 'while true; do echo tick >> heartbeat.log; sleep 0.1; done' & sleep 30"], "timeout_secs": 1}}"#,
        );
        let authored = "pub fn value() -> i32 { 0 }\n";

        let outcome = format_with_project_config(dir.path(), &project, authored, None)
            .await
            .expect("the project declares an in-memory formatter");

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
    async fn test_format_with_project_config_reports_cancellation() {
        let dir = repo();
        let project = project_config(r#"{"format": {"command": "sh", "args": ["-c", "cat"]}}"#);
        let authored = "pub fn value() -> i32 { 0 }\n";
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let outcome = format_with_project_config(dir.path(), &project, authored, Some(&cancel_rx))
            .await
            .expect("the project declares an in-memory formatter");

        assert!(outcome.is_cancelled);
        assert!(!outcome.formatted);
        assert_eq!(outcome.content, authored);
    }
}
