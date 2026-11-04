//! In-memory project formatting.
//!
//! Formatting has exactly one contour: `[project.format]` declares one stream
//! filter that receives the staged text of a file on standard input and returns
//! the rewritten text on standard output, so the change set is formatted while
//! it still lives in virtual staging. The filter runs in its own process group,
//! and a non-zero exit status, a timeout, or a cancellation keeps the authored
//! content.
//!
//! The declaration is resolved once per scope: a staging attempt validates its
//! filter a single time and streams every staged file of the change set through
//! the same resolved command line and timeout, while a single-file caller
//! resolves its own.
//!
//! Every invocation carries the repository-relative path of the file it
//! rewrites: the path reaches the process through the `TAUQE_FILE_PATH`
//! environment variable, and a declaration that names the parameter `path` or
//! `file` receives it as one literal argument as well. A declaration that names
//! neither keeps its argument list exactly as written, so a plain filter such as
//! `rustfmt` still reads the staged text on standard input and nothing else.

use std::path::Path;

use tauqe_protocol::{ProjectConfig, ProjectParameterSpec};
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

/// Streams one file's content through the in-memory formatter a project names.
///
/// This is the single-file entry point: it resolves the declaration and streams
/// `content` through the resolved filter. A staging attempt resolves its
/// declaration once for the whole change set and hands every staged file to
/// [`ResolvedFormatter::format`] instead.
///
/// This entry point carries no repository-relative path, so it leaves the file
/// context empty: the filter receives neither the variable nor an argument.
///
/// Returns `None` when the configuration provides no in-memory formatter. A
/// declaration that cannot be resolved, a non-zero exit status, a timeout, a
/// cancellation, or a filter that emits nothing keeps the authored content and
/// reports why.
pub async fn format_with_project_config(
    repo_root: &Path,
    project: &ProjectConfig,
    content: &str,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> Option<InMemoryFormatOutcome> {
    match resolve_formatter(project) {
        FormatterResolution::Absent => None,
        FormatterResolution::Rejected(reason) => Some(InMemoryFormatOutcome {
            formatted: false,
            content: content.to_string(),
            command: String::new(),
            is_cancelled: false,
            error: Some(reason),
        }),
        FormatterResolution::Ready(formatter) => {
            Some(formatter.format(repo_root, "", content, cancel_rx).await)
        }
    }
}

/// Resolves the in-memory formatter a project declares.
///
/// A declaration is validated once here and turned into the literal command line
/// and the timeout every file of the scope is streamed through, so a staging
/// attempt pays that work once per change set instead of once per staged file.
/// An unsound declaration is reported once as [`FormatterResolution::Rejected`]:
/// the authored content of every file is kept and one compact reason is logged.
///
/// The filter comes from the resolved project configuration: a declared
/// `[project.format]` entry, or the `format` recipe the declared task runner
/// exports when the entry is omitted. A declaration always keeps precedence over
/// the export of the same role, so a project can pin the filter in `tauqe.toml`
/// or delegate it to its justfile without changing the bounded turn.
pub(crate) fn resolve_formatter(project: &ProjectConfig) -> FormatterResolution {
    let Some(spec) = project.format.as_ref() else {
        return FormatterResolution::Absent;
    };

    let resolved = match project.resolve("format", spec) {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                operation = "format",
                reason = %err,
                "in-memory formatter rejected; keeping the staged content"
            );
            return FormatterResolution::Rejected(err.to_string());
        }
    };

    let timeout_secs = resolved
        .timeout_secs
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_PROJECT_TIMEOUT_SECS);

    FormatterResolution::Ready(ResolvedFormatter {
        command: CommandLine::new(resolved.command, resolved.args),
        timeout_secs,
        parameters: resolved.parameters,
    })
}

/// Outcome of resolving the in-memory formatter a project declares.
#[derive(Debug)]
pub(crate) enum FormatterResolution {
    /// The project declares no in-memory formatter.
    Absent,
    /// The declaration resolved into the filter of the whole scope.
    Ready(ResolvedFormatter),
    /// The project declares a formatter, but the declaration is unsound.
    Rejected(String),
}

/// The `[project.format]` declaration of a project, resolved once per scope.
///
/// Resolution produces the literal command line and the timeout, so every file of
/// one staging attempt streams through the same resolved value. The filter still
/// runs once per file: its staged text enters the child on standard input and the
/// rewritten text comes back on standard output, so no byte reaches disk and the
/// filter can never touch the working tree. The child leads its own process
/// group, so a timeout or a cancellation tears down every process the formatter
/// spawned.
#[derive(Debug)]
pub(crate) struct ResolvedFormatter {
    command: CommandLine,
    timeout_secs: u64,
    /// Parameter contract the declaration carries, in declaration order.
    parameters: Vec<ProjectParameterSpec>,
}

impl ResolvedFormatter {
    /// Streams one file's staged content through the resolved filter.
    ///
    /// A non-zero exit status, a timeout, a cancellation, or a filter that emits
    /// nothing while the staged text was not empty keeps the authored content of
    /// that file and reports why, so a broken formatter never blocks an attempt.
    ///
    /// `path` is the repository-relative path of the file whose staged content is
    /// streamed. It reaches the child as `TAUQE_FILE_PATH`, and a declaration
    /// that names the `path` or `file` parameter receives it as one literal
    /// argument as well, which is how a polyglot dispatcher selects a formatter
    /// by file type.
    pub(crate) async fn format(
        &self,
        repo_root: &Path,
        path: &str,
        content: &str,
        cancel_rx: Option<&watch::Receiver<bool>>,
    ) -> InMemoryFormatOutcome {
        let command = self.command_for(path);
        let outcome = run_streaming_command(
            repo_root,
            &command,
            path,
            content,
            self.timeout_secs,
            cancel_rx,
        )
        .await;

        if outcome.is_cancelled {
            tracing::warn!(
                command = %command.display(),
                "in-memory formatter cancelled; keeping the staged content"
            );
            return InMemoryFormatOutcome {
                formatted: false,
                content: content.to_string(),
                command: command.display(),
                is_cancelled: true,
                error: Some("the in-memory formatter was cancelled".to_string()),
            };
        }

        if !outcome.success {
            let reason = failure_reason(&outcome);
            tracing::warn!(
                command = %command.display(),
                timed_out = outcome.timed_out,
                reason = %reason,
                "in-memory formatter failed; keeping the staged content"
            );
            return InMemoryFormatOutcome {
                formatted: false,
                content: content.to_string(),
                command: command.display(),
                is_cancelled: false,
                error: Some(reason),
            };
        }

        // A filter that emits nothing while the staged text was not empty did
        // not rewrite the code: keeping the authored content is safer than
        // erasing it. A document that holds only whitespace still carries
        // content, so it is kept as well instead of being erased.
        if outcome.stdout.is_empty() && !content.is_empty() {
            return InMemoryFormatOutcome {
                formatted: false,
                content: content.to_string(),
                command: command.display(),
                is_cancelled: false,
                error: Some(format!(
                    "formatter `{}` produced no output",
                    command.program()
                )),
            };
        }

        InMemoryFormatOutcome {
            formatted: true,
            content: outcome.stdout,
            command: command.display(),
            is_cancelled: false,
            error: None,
        }
    }

    /// Builds the command line one file of the change set is streamed through.
    ///
    /// The declaration decides the argument list. A declaration that names the
    /// `path` or `file` parameter receives the repository-relative path as one
    /// literal argument, so a task runner recipe such as `format file:` is
    /// addressed exactly as it is on a terminal. A declaration that names neither
    /// keeps its argument list unchanged, which is what a plain filter such as
    /// `rustfmt` expects: it reads the staged text on standard input and knows
    /// nothing about repository paths.
    fn command_for(&self, path: &str) -> CommandLine {
        let mut args = self.command.args().to_vec();
        if !path.is_empty() && carries_path_parameter(&self.parameters) {
            args.push(path.to_string());
        }
        CommandLine::new(self.command.program(), args)
    }
}

/// Reports whether a declaration names the parameter that receives the file path.
///
/// The name is the whole contract: a declaration that spells the parameter `path`
/// or `file` receives the repository-relative path of the formatted file, and
/// every other declaration receives no argument at all. The formatter always
/// supplies exactly one value, because the path is one literal argument and the
/// filter itself stays a pure stream.
fn carries_path_parameter(parameters: &[ProjectParameterSpec]) -> bool {
    parameters
        .iter()
        .map(|parameter| parameter.name.trim())
        .any(|name| name.eq_ignore_ascii_case("path") || name.eq_ignore_ascii_case("file"))
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
    use super::super::exec::PROCESS_GROUP_STABILITY_WINDOW;
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

        // A document that holds code is never erased by a silent filter.
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

        // A document that holds only whitespace is still content, so the rule
        // keeps it instead of treating the silent filter as a rewrite.
        let whitespace_only = "\n";
        let outcome = format_with_project_config(dir.path(), &project, whitespace_only, None)
            .await
            .expect("the project declares an in-memory formatter");
        assert!(!outcome.formatted);
        assert_eq!(outcome.content, whitespace_only);
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
        tokio::time::sleep(PROCESS_GROUP_STABILITY_WINDOW).await;
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
    /// One resolved value streams every file of an attempt through the same
    /// declaration, so the second file never re-resolves the filter.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_one_resolved_formatter_streams_every_file_of_an_attempt() {
        let dir = repo();
        let project =
            project_config(r#"{"format": {"command": "sh", "args": ["-c", "tr a-z A-Z"]}}"#);
        let FormatterResolution::Ready(formatter) = resolve_formatter(&project) else {
            panic!("the project declares an in-memory formatter");
        };

        let first = formatter
            .format(dir.path(), "src/first.rs", "pub fn first() {}\n", None)
            .await;
        let second = formatter
            .format(dir.path(), "src/second.rs", "pub fn second() {}\n", None)
            .await;

        assert!(first.formatted, "error: {:?}", first.error);
        assert_eq!(first.content, "PUB FN FIRST() {}\n");
        assert!(second.formatted, "error: {:?}", second.error);
        assert_eq!(second.content, "PUB FN SECOND() {}\n");
        assert_eq!(first.command, second.command);
        assert_eq!(first.command, "sh -c tr a-z A-Z");
    }

    /// One resolved value keeps per-file outcomes: a file the shared filter
    /// refuses keeps its authored content while another file of the same
    /// resolution is rewritten.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_one_resolved_formatter_keeps_per_file_outcomes() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "read -r line; case \"$line\" in keep*) echo refused >&2; exit 3;; esac; echo \"$line\" | tr a-z A-Z"]}}"#,
        );
        let FormatterResolution::Ready(formatter) = resolve_formatter(&project) else {
            panic!("the project declares an in-memory formatter");
        };

        let accepted = formatter
            .format(dir.path(), "src/rewrite.rs", "rewrite me\n", None)
            .await;
        let refused = formatter
            .format(dir.path(), "src/keep.rs", "keep me\n", None)
            .await;

        assert!(accepted.formatted, "error: {:?}", accepted.error);
        assert_eq!(accepted.content, "REWRITE ME\n");
        assert!(!refused.formatted);
        assert_eq!(refused.content, "keep me\n");
        assert_eq!(refused.command, accepted.command);
        let reason = refused.error.unwrap_or_default();
        assert!(reason.contains("refused"), "unexpected reason: {reason}");
    }

    /// A project that declares nothing resolves to no filter, and a declaration
    /// the configuration rejects is reported once with its reason.
    #[tokio::test]
    async fn test_resolve_formatter_reports_absence_and_rejects_an_unsound_declaration() {
        assert!(matches!(
            resolve_formatter(&ProjectConfig::default()),
            FormatterResolution::Absent
        ));

        let project = project_config(r#"{"format": {"command": "@missing"}}"#);
        let FormatterResolution::Rejected(reason) = resolve_formatter(&project) else {
            panic!("an unsound declaration cannot resolve");
        };
        assert!(reason.contains("missing"), "unexpected reason: {reason}");
    }

    /// The filter learns which file it rewrites from the environment, so a
    /// dispatcher can select a formatter by file type while the staged text still
    /// arrives on standard input.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_the_filter_reads_the_formatted_path_from_the_environment() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "printf '%s\\n' \"$TAUQE_FILE_PATH\"; cat > /dev/null"]}}"#,
        );
        let FormatterResolution::Ready(formatter) = resolve_formatter(&project) else {
            panic!("the project declares an in-memory formatter");
        };

        let outcome = formatter
            .format(dir.path(), "src/main.ts", "const value = 1;\n", None)
            .await;

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "src/main.ts\n");
    }

    /// A declaration that names the path parameter receives the file path as one
    /// literal argument, so a task runner recipe such as `format file:` is
    /// addressed the way it is on a terminal.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_declared_path_parameter_receives_the_formatted_path() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "printf '%s\\n' \"$1\"; cat > /dev/null", "sh"], "parameters": [{"name": "file"}]}}"#,
        );
        let FormatterResolution::Ready(formatter) = resolve_formatter(&project) else {
            panic!("the project declares an in-memory formatter");
        };

        let outcome = formatter
            .format(dir.path(), "crates/tui/src/main.rs", "fn main() {}\n", None)
            .await;

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "crates/tui/src/main.rs\n");
    }

    /// A declaration that names no path parameter keeps its argument list
    /// unchanged, so a plain filter such as `rustfmt` still receives the staged
    /// text on standard input and no argument at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_declaration_without_a_path_parameter_keeps_its_command_line() {
        let dir = repo();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "tr a-z A-Z"], "parameters": []}}"#,
        );
        let FormatterResolution::Ready(formatter) = resolve_formatter(&project) else {
            panic!("the project declares an in-memory formatter");
        };

        let outcome = formatter
            .format(dir.path(), "src/lib.rs", "pub fn value() {}\n", None)
            .await;

        assert!(outcome.formatted, "error: {:?}", outcome.error);
        assert_eq!(outcome.content, "PUB FN VALUE() {}\n");
        // The reported command line is the declared one: the path the declaration
        // never named does not appear as an argument.
        assert_eq!(outcome.command, "sh -c tr a-z A-Z");

        // The name is the whole contract, so a declared parameter that is not the
        // path never receives it either.
        let other = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "tr a-z A-Z"], "parameters": [{"name": "target"}]}}"#,
        );
        let FormatterResolution::Ready(formatter) = resolve_formatter(&other) else {
            panic!("the project declares an in-memory formatter");
        };
        let outcome = formatter
            .format(dir.path(), "src/lib.rs", "pub fn value() {}\n", None)
            .await;
        assert_eq!(outcome.content, "PUB FN VALUE() {}\n");
        assert_eq!(outcome.command, "sh -c tr a-z A-Z");
    }
}
