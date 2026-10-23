//! Wire contract of the project driver protocol.
//!
//! TAUQE delegates every operation that depends on the concrete technology of a
//! code base — formatting, disk-side code generation, and verification — to an
//! out-of-process *project driver*. The harness itself stays technology
//! agnostic: it knows nothing about which formatter, compiler, or build system
//! a repository uses.
//!
//! The driver speaks a request/response protocol over stdio, and every message
//! of that protocol is defined here. The contract mirrors the three phases of a
//! code modification:
//!
//! 1. [`ProjectRequest::Format`] rewrites source text **in memory**, while the
//!    change set still lives in virtual staging and nothing has touched disk.
//! 2. [`ProjectRequest::PostHook`] runs disk-side side effects (code
//!    generation, lock-file refreshes) strictly after the edit was written.
//! 3. [`ProjectRequest::Check`] runs a named verification suite and reports
//!    typed [`Diagnostic`] values instead of raw terminal output.
//!
//! Verification failures travel as structured diagnostics, so a
//! verification-healing round reasons about the offending file, line, and
//! message without paying for ANSI escapes and full compiler transcripts in its
//! context window. [`format_diagnostics`] renders them deterministically for
//! exactly that use.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

pub mod config;
pub mod manifest;

// The manifest model is part of the project driver wire contract: route
// selection is what lets one driver serve a polyglot repository. The facade
// re-exports the declaration types so callers reach them through the `project`
// module without knowing the internal submodule layout.
pub use manifest::{
    DiagnosticParserKind, ProjectCheckStepSpec, ProjectCheckSuiteSpec, ProjectFormatterSpec,
    ProjectManifest, ProjectPostHookSpec,
};

// The `[project]` declaration of `tauqe.toml` is re-exported through the facade
// for the same reason: callers reach it without knowing the submodule layout.
pub use config::{
    ProjectCommandSpec, ProjectConfig, ProjectConfigError, ResolvedProjectCommand, TaskRunnerKind,
};

/// Severity of a [`Diagnostic`] reported by a project driver.
///
/// The set is closed, and the declaration order is the canonical reporting
/// order: errors precede warnings, warnings precede notes, and notes precede
/// help. Every spelling that cannot be read resolves to
/// [`DiagnosticSeverity::Error`], so an unreadable severity can never downgrade
/// a real failure into a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// A failure that invalidates the verified change.
    #[default]
    Error,
    /// A problem that does not invalidate the verified change.
    Warning,
    /// Additional information attached to a preceding diagnostic.
    Note,
    /// A suggestion on how to resolve a preceding diagnostic.
    Help,
}

impl DiagnosticSeverity {
    /// Canonical lower-case identifier used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
            Self::Help => "help",
        }
    }

    /// Tolerant constructor: trims the input, matches case-insensitively, and
    /// resolves any unknown or empty spelling to [`DiagnosticSeverity::Error`].
    pub fn parse_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "warning" => Self::Warning,
            "note" => Self::Note,
            "help" => Self::Help,
            _ => Self::Error,
        }
    }
}

impl fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Tolerant decoding is confined to the wire boundary: a `null`, an absent
// value, or an unknown spelling resolves to `Error` instead of failing the
// whole diagnostic list, so one odd severity never discards the remaining
// findings of a failed verification.
impl<'de> Deserialize<'de> for DiagnosticSeverity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        Ok(raw.as_deref().map(Self::parse_lossy).unwrap_or_default())
    }
}

/// A structured verification diagnostic reported by a project driver.
///
/// The driver parses compiler, linter, and test-runner output into this shape,
/// so the harness never reasons about terminal transcripts. Every optional field
/// reflects what a toolchain actually provides: a linker failure has no
/// location, a test failure has no diagnostic code, and a structured linter
/// emits no pre-rendered line at all.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Repository-relative path the diagnostic points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// One-based line number inside [`Diagnostic::file`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// One-based column number inside [`Diagnostic::file`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// Severity of the diagnostic.
    #[serde(default)]
    pub severity: DiagnosticSeverity,
    /// Tool-specific code, such as a compiler error number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable message.
    pub message: String,
    /// The driver's own pre-rendered line, retained verbatim for fidelity.
    ///
    /// [`format_diagnostics`] ignores it and renders from the typed fields
    /// instead, so two reports of the same problem never render twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
}

/// Renders diagnostics as a deterministic, line-oriented report.
///
/// The report is the verification-healing input: it carries no ANSI escapes, no
/// tool banners, and no duplicated findings.
///
/// Ordering is total and independent of the order the driver reported: severity
/// first (as declared by [`DiagnosticSeverity`]), then located diagnostics
/// before unlocated ones, then file, line, column, code, and message. Findings
/// that differ only in their pre-rendered [`Diagnostic::rendered`] text are
/// reported once.
///
/// A diagnostic renders as `path:line:column: severity[code]: message`, keeping
/// only the location parts it actually has: the column is rendered only when the
/// line is known, and a location without a file is dropped in favor of the bare
/// message. Message whitespace is collapsed onto one line, and no field is
/// truncated because output budgets belong to configuration, not to rendering.
pub fn format_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut ordered: Vec<&Diagnostic> = diagnostics.iter().collect();
    ordered.sort_by(|left, right| compare_diagnostics(left, right));

    let mut report = String::new();
    let mut last: Option<&Diagnostic> = None;
    for diagnostic in ordered {
        if last.is_some_and(|previous| same_finding(previous, diagnostic)) {
            continue;
        }
        if !report.is_empty() {
            report.push('\n');
        }
        report.push_str(&render_diagnostic(diagnostic));
        last = Some(diagnostic);
    }
    report
}

/// Total order over diagnostics that ignores the pre-rendered text.
fn compare_diagnostics(left: &Diagnostic, right: &Diagnostic) -> Ordering {
    left.severity
        .cmp(&right.severity)
        .then_with(|| location_rank(left).cmp(&location_rank(right)))
        .then_with(|| left.file.cmp(&right.file))
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.column.cmp(&right.column))
        .then_with(|| left.code.cmp(&right.code))
        .then_with(|| left.message.cmp(&right.message))
}

/// Reports whether two diagnostics describe the same finding.
///
/// The pre-rendered text is deliberately excluded: it is a presentation detail
/// of the producing driver, so the same problem reported twice still collapses
/// into a single line.
fn same_finding(left: &Diagnostic, right: &Diagnostic) -> bool {
    left.severity == right.severity
        && left.file == right.file
        && left.line == right.line
        && left.column == right.column
        && left.code == right.code
        && left.message == right.message
}

/// Ranks a diagnostic by whether it carries a usable file location, so located
/// findings sort before unlocated ones.
fn location_rank(diagnostic: &Diagnostic) -> u8 {
    match located_file(diagnostic) {
        Some(_) => 0,
        None => 1,
    }
}

/// Returns the trimmed file path of a diagnostic when it is non-empty.
fn located_file(diagnostic: &Diagnostic) -> Option<&str> {
    diagnostic
        .file
        .as_deref()
        .map(str::trim)
        .filter(|file| !file.is_empty())
}

/// Returns the trimmed diagnostic code when it is non-empty.
fn diagnostic_code(diagnostic: &Diagnostic) -> Option<&str> {
    diagnostic
        .code
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty())
}

/// Renders one diagnostic as a single line.
fn render_diagnostic(diagnostic: &Diagnostic) -> String {
    let mut rendered = String::new();

    if let Some(file) = located_file(diagnostic) {
        rendered.push_str(file);
        if let Some(line) = diagnostic.line {
            rendered.push(':');
            rendered.push_str(&line.to_string());
            if let Some(column) = diagnostic.column {
                rendered.push(':');
                rendered.push_str(&column.to_string());
            }
        }
        rendered.push_str(": ");
    }

    rendered.push_str(diagnostic.severity.as_str());
    if let Some(code) = diagnostic_code(diagnostic) {
        rendered.push('[');
        rendered.push_str(code);
        rendered.push(']');
    }

    let message = collapse_whitespace(&diagnostic.message);
    if !message.is_empty() {
        rendered.push_str(": ");
        rendered.push_str(&message);
    }

    rendered
}

/// Collapses every run of Unicode whitespace into a single space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// A request sent to the project driver.
///
/// Requests are internally tagged by `type`, and every variant flattens its
/// payload into the same object, so a request is always one flat JSON object:
/// `{"type":"format","path":"src/lib.rs","content":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectRequest {
    /// Asks the driver to describe the facets it implements.
    Capabilities,
    /// Formats source text in memory.
    Format(ProjectFormatRequest),
    /// Runs disk-side side effects after an edit was written.
    PostHook(ProjectPostHookRequest),
    /// Runs a named verification suite.
    Check(ProjectCheckRequest),
}

/// A response produced by the project driver.
///
/// A response carries the same `type` tag as the request it answers, so the
/// caller correlates the two without an additional identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectResponse {
    /// Answer to [`ProjectRequest::Capabilities`].
    Capabilities { capabilities: ProjectCapabilities },
    /// Answer to [`ProjectRequest::Format`].
    Format(ProjectFormatResponse),
    /// Answer to [`ProjectRequest::PostHook`].
    PostHook(ProjectPostHookResponse),
    /// Answer to [`ProjectRequest::Check`].
    Check(ProjectCheckResponse),
}

/// Facets a project driver implements.
///
/// The driver describes itself once, so the harness can degrade gracefully
/// instead of issuing a request the driver cannot answer. An absent flag means
/// the facet is unavailable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectCapabilities {
    /// In-memory formatting is available through [`ProjectRequest::Format`].
    #[serde(default)]
    pub format: bool,
    /// A disk post-hook is declared and answers [`ProjectRequest::PostHook`].
    #[serde(default)]
    pub post_hook: bool,
    /// Named check suites the driver accepts, in lexicographic name order.
    ///
    /// The manifest keys its suites by name, so this list is the sorted set of
    /// declared names rather than the order the entries appear in the file.
    /// Selection over formatters, hooks, and steps obeys the same rule, so a
    /// manifest author controls precedence by naming.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suites: Vec<String>,
    /// Verification reports structured [`Diagnostic`] values rather than raw text.
    #[serde(default)]
    pub diagnostics: bool,
    /// Future facet: project documentation access.
    #[serde(default)]
    pub docs: bool,
    /// Future facet: dependency management operations.
    #[serde(default)]
    pub dependencies: bool,
}

/// Request to format one file in memory.
///
/// The driver rewrites `content` without touching the file system, and the
/// caller feeds the result back into virtual staging. `path` is the
/// repository-relative path of the file, so the driver can select a formatter by
/// extension and pick up the formatter configuration nearest to that path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFormatRequest {
    /// Repository-relative path of the file being formatted.
    pub path: String,
    /// Full in-memory content to format.
    pub content: String,
}

/// Result of formatting one file in memory.
///
/// `content` is always the content the caller must keep: the formatted result on
/// success, the unmodified input when no formatter claimed the file or the
/// formatter failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFormatResponse {
    /// Whether a formatter processed the file.
    pub formatted: bool,
    /// Content to keep, as described above.
    pub content: String,
    /// Name of the formatter entry that claimed the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatter: Option<String>,
    /// Failure reason when the formatter could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request to run disk-side side effects after an edit was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPostHookRequest {
    /// Repository-relative paths changed by the applied patch, so the driver can
    /// run only the hooks those paths require.
    #[serde(default)]
    pub changed_files: Vec<String>,
}

/// Result of the disk post-hook phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPostHookResponse {
    /// Whether every executed hook succeeded.
    pub success: bool,
    /// Per-hook outcomes in execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executed_hooks: Vec<ProjectHookOutcome>,
    /// Compact summary of the phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Outcome of one executed post-hook entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectHookOutcome {
    /// Name of the hook entry as declared by the project manifest.
    pub name: String,
    /// Whether the hook completed successfully.
    pub success: bool,
    /// Compact failure reason or summary, never the raw command transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Request to run one named verification suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCheckRequest {
    /// Suite name declared by the project manifest and advertised in
    /// [`ProjectCapabilities::suites`].
    pub suite: String,
    /// Repository-relative paths the current turn changed.
    #[serde(default)]
    pub changed_files: Vec<String>,
}

/// Result of one verification suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCheckResponse {
    /// Whether every executed step succeeded.
    pub success: bool,
    /// Per-step results in execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<ProjectStepResult>,
    /// Structured findings parsed from the failing steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Outcome of one executed verification step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStepResult {
    /// Name of the step, such as the phase a suite ran.
    pub name: String,
    /// Command line that was executed.
    pub command: String,
    /// Whether the step succeeded.
    pub success: bool,
    /// Wall-clock duration of the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Compact failure reason or summary, never the raw command transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a located diagnostic without a code or pre-rendered text.
    fn located(
        severity: DiagnosticSeverity,
        file: &str,
        line: u32,
        column: u32,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            file: Some(file.to_string()),
            line: Some(line),
            column: Some(column),
            severity,
            code: None,
            message: message.to_string(),
            rendered: None,
        }
    }

    /// Encodes a value and decodes it back as a JSON value for shape checks.
    fn as_value<T: Serialize>(value: &T) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    #[test]
    fn test_project_request_serde_roundtrip() {
        let cases = [
            ProjectRequest::Capabilities,
            ProjectRequest::Format(ProjectFormatRequest {
                path: "src/lib.rs".to_string(),
                content: "fn  main( ){}".to_string(),
            }),
            ProjectRequest::PostHook(ProjectPostHookRequest {
                changed_files: vec!["src/lib.rs".to_string()],
            }),
            ProjectRequest::Check(ProjectCheckRequest {
                suite: "all".to_string(),
                changed_files: vec!["src/lib.rs".to_string()],
            }),
        ];

        for request in cases {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<ProjectRequest>(&json).unwrap(),
                request
            );
        }

        // The tag and the payload share one flat object.
        assert_eq!(
            as_value(&ProjectRequest::Format(ProjectFormatRequest {
                path: "src/lib.rs".to_string(),
                content: "x".to_string(),
            })),
            serde_json::json!({
                "type": "format",
                "path": "src/lib.rs",
                "content": "x",
            })
        );
        assert_eq!(
            as_value(&ProjectRequest::Capabilities),
            serde_json::json!({ "type": "capabilities" })
        );
    }

    #[test]
    fn test_project_response_serde_roundtrip() {
        let cases = [
            ProjectResponse::Capabilities {
                capabilities: ProjectCapabilities {
                    format: true,
                    post_hook: false,
                    suites: vec!["check".to_string(), "test".to_string()],
                    diagnostics: true,
                    docs: false,
                    dependencies: true,
                },
            },
            ProjectResponse::Format(ProjectFormatResponse {
                formatted: true,
                content: "fn main() {}\n".to_string(),
                formatter: Some("rustfmt".to_string()),
                error: None,
            }),
            ProjectResponse::PostHook(ProjectPostHookResponse {
                success: true,
                executed_hooks: vec![ProjectHookOutcome {
                    name: "protobuf".to_string(),
                    success: true,
                    message: None,
                }],
                message: Some("regenerated 2 files".to_string()),
            }),
            ProjectResponse::Check(ProjectCheckResponse {
                success: false,
                steps: vec![ProjectStepResult {
                    name: "check".to_string(),
                    command: "cargo check --all-targets".to_string(),
                    success: false,
                    duration_ms: Some(1200),
                    message: Some("2 errors".to_string()),
                }],
                diagnostics: vec![located(
                    DiagnosticSeverity::Error,
                    "src/lib.rs",
                    4,
                    9,
                    "mismatched types",
                )],
            }),
        ];

        for response in cases {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(
                serde_json::from_str::<ProjectResponse>(&json).unwrap(),
                response
            );
        }

        // A check response nests the typed diagnostics rather than raw output.
        let value = as_value(&ProjectResponse::Check(ProjectCheckResponse {
            success: false,
            steps: Vec::new(),
            diagnostics: vec![located(
                DiagnosticSeverity::Error,
                "src/lib.rs",
                4,
                9,
                "mismatched types",
            )],
        }));
        assert_eq!(
            value,
            serde_json::json!({
                "type": "check",
                "success": false,
                "diagnostics": [{
                    "file": "src/lib.rs",
                    "line": 4,
                    "column": 9,
                    "severity": "error",
                    "message": "mismatched types",
                }],
            })
        );
    }

    #[test]
    fn test_project_messages_reject_unknown_or_missing_type() {
        assert!(serde_json::from_str::<ProjectRequest>(r#"{"type":"teleport"}"#).is_err());
        assert!(serde_json::from_str::<ProjectResponse>(r#"{"type":"teleport"}"#).is_err());
        assert!(serde_json::from_str::<ProjectRequest>(r#"{"path":"src/lib.rs"}"#).is_err());
    }

    #[test]
    fn test_project_capabilities_defaults_and_absent_facets() {
        let defaults = ProjectCapabilities::default();
        assert!(!defaults.format);
        assert!(!defaults.post_hook);
        assert!(defaults.suites.is_empty());
        assert!(!defaults.diagnostics);
        assert!(!defaults.docs);
        assert!(!defaults.dependencies);

        // An absent flag means the facet is unavailable, and absent future
        // facets decode as unsupported instead of failing the handshake.
        let minimal: ProjectCapabilities = serde_json::from_str(r#"{"format":true}"#).unwrap();
        assert!(minimal.format);
        assert!(!minimal.post_hook);
        assert!(!minimal.diagnostics);
        assert!(!minimal.docs);
        assert!(!minimal.dependencies);

        let encoded = as_value(&minimal);
        assert_eq!(encoded["docs"], serde_json::json!(false));
        assert_eq!(encoded["dependencies"], serde_json::json!(false));
        assert!(encoded.get("suites").is_none());
    }

    #[test]
    fn test_diagnostic_serde_roundtrip_and_omitted_optionals() {
        // A diagnostic without a location and without a code still decodes.
        let minimal: Diagnostic = serde_json::from_str(r#"{"message":"linker failed"}"#).unwrap();
        assert_eq!(minimal.severity, DiagnosticSeverity::Error);
        assert!(minimal.file.is_none());
        assert!(minimal.line.is_none());
        assert!(minimal.column.is_none());
        assert!(minimal.code.is_none());
        assert!(minimal.rendered.is_none());

        let encoded = as_value(&minimal);
        assert_eq!(
            encoded,
            serde_json::json!({ "severity": "error", "message": "linker failed" })
        );

        let full = Diagnostic {
            file: Some("crates/core/src/lib.rs".to_string()),
            line: Some(12),
            column: Some(5),
            severity: DiagnosticSeverity::Warning,
            code: Some("unused_variables".to_string()),
            message: "unused variable `x`".to_string(),
            rendered: Some("warning: unused variable `x`".to_string()),
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<Diagnostic>(&json).unwrap(), full);
    }

    #[test]
    fn test_diagnostic_severity_tolerant_parsing() {
        let cases = [
            ("\"error\"", DiagnosticSeverity::Error),
            ("\"  WARNING \"", DiagnosticSeverity::Warning),
            ("\"Note\"", DiagnosticSeverity::Note),
            ("\"help\"", DiagnosticSeverity::Help),
            ("\"fatal\"", DiagnosticSeverity::Error),
            ("\"\"", DiagnosticSeverity::Error),
            ("null", DiagnosticSeverity::Error),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                serde_json::from_str::<DiagnosticSeverity>(raw).unwrap(),
                expected
            );
        }

        for severity in [
            DiagnosticSeverity::Error,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Note,
            DiagnosticSeverity::Help,
        ] {
            let encoded = serde_json::to_string(&severity).unwrap();
            assert_eq!(encoded, format!("\"{}\"", severity.as_str()));
            assert_eq!(severity.to_string(), severity.as_str());
            assert_eq!(DiagnosticSeverity::parse_lossy(severity.as_str()), severity);
        }

        // Declaration order is the canonical reporting order.
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Note);
        assert!(DiagnosticSeverity::Note < DiagnosticSeverity::Help);
        assert_eq!(DiagnosticSeverity::default(), DiagnosticSeverity::Error);
    }

    #[test]
    fn test_format_diagnostics_orders_dedups_and_renders() {
        let diagnostics = vec![
            located(
                DiagnosticSeverity::Warning,
                "src/b.rs",
                10,
                5,
                "unused import",
            ),
            // The same finding with a pre-rendered line must be reported once.
            Diagnostic {
                rendered: Some("\u{1b}[33mwarning\u{1b}[0m unused import".to_string()),
                ..located(
                    DiagnosticSeverity::Warning,
                    "src/b.rs",
                    10,
                    5,
                    "unused import",
                )
            },
            Diagnostic {
                code: Some("E0308".to_string()),
                ..located(
                    DiagnosticSeverity::Error,
                    "src/a.rs",
                    2,
                    1,
                    "mismatched   types",
                )
            },
            Diagnostic {
                file: None,
                line: None,
                column: None,
                severity: DiagnosticSeverity::Error,
                code: None,
                message: "aborting due to 2 previous errors".to_string(),
                rendered: None,
            },
        ];

        let report = format_diagnostics(&diagnostics);
        // Severity outranks location: the unlocated summary of the failing
        // errors stays ahead of the located warning, while a located
        // diagnostic keeps precedence over an unlocated one of equal severity.
        assert_eq!(
            report,
            "src/a.rs:2:1: error[E0308]: mismatched types\n\
             error: aborting due to 2 previous errors\n\
             src/b.rs:10:5: warning: unused import"
        );
        assert!(!report.contains('\u{1b}'));
        assert!(!report.ends_with('\n'));
    }

    #[test]
    fn test_format_diagnostics_is_order_independent() {
        let mut diagnostics = vec![
            located(
                DiagnosticSeverity::Help,
                "src/z.rs",
                3,
                1,
                "consider borrowing",
            ),
            located(
                DiagnosticSeverity::Error,
                "src/a.rs",
                9,
                2,
                "mismatched types",
            ),
            Diagnostic {
                file: None,
                line: None,
                column: None,
                severity: DiagnosticSeverity::Note,
                code: None,
                message: "note: defined here".to_string(),
                rendered: None,
            },
            located(
                DiagnosticSeverity::Warning,
                "src/a.rs",
                1,
                7,
                "unused import",
            ),
        ];

        let forward = format_diagnostics(&diagnostics);
        diagnostics.reverse();
        assert_eq!(format_diagnostics(&diagnostics), forward);
    }

    #[test]
    fn test_format_diagnostics_normalizes_location_and_message() {
        let diagnostic = Diagnostic {
            file: Some("  crates/core/src/lib.rs  ".to_string()),
            line: Some(7),
            column: None,
            severity: DiagnosticSeverity::Note,
            code: Some(" note-code ".to_string()),
            message: "expected\n\tone   argument".to_string(),
            rendered: None,
        };
        assert_eq!(
            format_diagnostics(&[diagnostic]),
            "crates/core/src/lib.rs:7: note[note-code]: expected one argument"
        );

        // A blank message leaves only the classified finding.
        let blank = Diagnostic {
            message: "   ".to_string(),
            ..located(DiagnosticSeverity::Warning, "src/x.rs", 3, 1, "")
        };
        assert_eq!(format_diagnostics(&[blank]), "src/x.rs:3:1: warning");

        // An empty finding list renders an empty report.
        assert_eq!(format_diagnostics(&[]), "");
    }

    #[test]
    fn test_format_diagnostics_drops_incomplete_locations() {
        // A location without a file is not addressable, so only the message stays.
        let orphan = Diagnostic {
            line: Some(12),
            column: Some(3),
            message: "cannot open input".to_string(),
            ..Diagnostic::default()
        };
        assert_eq!(format_diagnostics(&[orphan]), "error: cannot open input");

        // A column without a line would read as a line number, so it is dropped.
        let column_only = Diagnostic {
            file: Some("src/y.rs".to_string()),
            column: Some(4),
            message: "invalid argument".to_string(),
            ..Diagnostic::default()
        };
        assert_eq!(
            format_diagnostics(&[column_only]),
            "src/y.rs: error: invalid argument"
        );
    }
}
