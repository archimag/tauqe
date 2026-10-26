use crate::escape::{escape_xml_text, single_line};
use tauqe_protocol::review::ReviewSession;
use tauqe_protocol::{EditProtocolKind, PlanItem};

pub const REVIEW_STEP_RETRY_PREFIX: &str = "# Correction Required: Review Finding Turn Incomplete";

/// Joins identifiers or paths into a single escaped, comma-separated line.
fn escape_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| escape_xml_text(item))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders repository paths as escaped, backticked breadcrumbs.
fn render_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| format!("`{}`", escape_xml_text(path)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders the suffix a control tag carries, or an empty string when the active
/// protocol emits no tagged control channel.
fn marker_suffix(marker: Option<&str>) -> String {
    marker.map(|m| format!("_{}", m)).unwrap_or_default()
}

/// Builds a corrective reprompt for a review finding turn that ended without
/// valid code edits and without a terminal completion signal.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback (such as a missing edit block or toolchain verification failure),
/// then explicitly demands strict adherence to the active protocol on the next
/// turn: the structured contour asks for a change set and a successful
/// `outcome`, while the tagged contour asks for an edit block and the success
/// tag bound to `marker`.
pub fn format_review_step_retry_prompt(
    assistant_text: &str,
    error: Option<&str>,
    protocol: EditProtocolKind,
    marker: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(REVIEW_STEP_RETRY_PREFIX);
    out.push_str("\n\n");
    match protocol {
        EditProtocolKind::Structured => out.push_str(
            "Your previous turn ended without applying any verified code changes and without reporting a successful `outcome`. This violates the execution protocol.\n\n",
        ),
        EditProtocolKind::Xml | EditProtocolKind::Mixed => {
            out.push_str(&format!(
                "Your previous turn ended without applying any verified code changes and without emitting the terminal success tag (`<success{0}>`). This violates the execution protocol.\n\n",
                marker_suffix(marker)
            ));
        }
    }

    out.push_str("## Previous Assistant Response:\n");
    let previous = assistant_text.trim();
    if previous.is_empty() {
        out.push_str("(empty response)\n\n");
    } else {
        out.push_str("<previous_response>\n");
        out.push_str(&escape_xml_text(previous));
        out.push_str("\n</previous_response>\n\n");
    }

    if let Some(err) = error {
        let trimmed = err.trim();
        if !trimmed.is_empty() {
            out.push_str("## Diagnostic Feedback:\n");
            out.push_str(&escape_xml_text(trimmed));
            out.push_str("\n\n");
        }
    }

    out.push_str("## Required Actions:\n");
    match protocol {
        EditProtocolKind::Structured => {
            out.push_str("1. If the finding requires code changes, emit a valid change set in the `changes` array that strictly conforms to the structured schema.\n");
            out.push_str("2. If no code changes are required to resolve the finding, confirm completion explicitly by setting the `outcome` field to status \"success\".\n");
            out.push_str("3. Do NOT replace the structured fields with conversational text, and do NOT end the turn without either a valid `changes` entry or a successful `outcome`.\n");
        }
        EditProtocolKind::Xml | EditProtocolKind::Mixed => {
            out.push_str("1. If the finding requires code changes, emit a valid edit block that strictly conforms to the active edit protocol.\n");
            out.push_str(&format!(
                "2. If no code changes are required to resolve the finding, confirm completion explicitly by emitting the terminal success tag (`<success{0}>`).\n",
                marker_suffix(marker)
            ));
            out.push_str("3. Do NOT replace protocol operations with conversational text, and do NOT end the turn without either a valid edit block or the terminal success tag.\n");
        }
    }

    out
}

pub const REVIEW_STEP_EXECUTION_PREFIX: &str = "# Target Review Step Execution:";

/// Formats a targeted prompt for resolving an isolated review finding.
///
/// The prompt renders the enriched finding view: the `description` states the
/// defect, the `recommendations` carry the remediation strategy, the
/// `readonly_files` enumerate the protected contracts the fix must preserve
/// unchanged, and the `plan` exposes any decomposition already produced.
/// This builder embeds the concrete edit-protocol markers and therefore lives in
/// the core engine beside the sibling plan step prompt rather than in the
/// dependency-light wire crate.
///
/// The execution directives speak the vocabulary of the active protocol: the
/// structured contour names the response fields it expects (`outcome`,
/// `blocked`, `context_requests`, `verify`), while the tagged contour names its
/// control tags bound to `marker`. A structured turn emits no command tag, so
/// the marker it carries is never used.
pub fn format_review_step_execution_prompt(
    session: &ReviewSession,
    item_id: u32,
    protocol: EditProtocolKind,
    marker: Option<&str>,
) -> Result<String, String> {
    let marker_suffix = marker_suffix(marker);
    let issue = session
        .issues()
        .into_iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| {
            format!(
                "Review item #{} not found in review session '{}'",
                item_id, session.id
            )
        })?;

    let mut out = String::new();
    let clean_title = escape_xml_text(&single_line(&issue.title));
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        REVIEW_STEP_EXECUTION_PREFIX,
        single_line(&session.id),
        issue.id,
        clean_title
    ));

    let session_title = single_line(&session.title);
    let title = if session_title.is_empty() {
        session.id.as_str()
    } else {
        session_title.as_str()
    };
    out.push_str(&format!("## Review Session: {}\n", escape_xml_text(title)));
    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", escape_xml_text(trimmed)));
        }
    }

    out.push_str("### Target Finding to Resolve:\n");
    out.push_str(&format!("- **ID**: `#{}`\n", issue.id));
    out.push_str(&format!("- **Type**: {}\n", issue.issue_type));
    out.push_str(&format!("- **Title**: {}\n", escape_xml_text(&issue.title)));
    if !issue.files.is_empty() {
        out.push_str(&format!("- **Files**: {}\n", render_paths(&issue.files)));
    }
    if !issue.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Protected Contracts (readonly_files)**: {}\n",
            render_paths(&issue.readonly_files)
        ));
    }
    if !issue.description.trim().is_empty() {
        out.push_str(&format!(
            "- **Description**:\n{}\n",
            escape_xml_text(issue.description.trim())
        ));
    }
    if !issue.recommendations.is_empty() {
        out.push_str("- **Recommendations**:\n");
        for (index, rec) in issue.recommendations.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, escape_xml_text(rec)));
        }
    }
    if let Some(note) = &issue.note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "- **Resolution Note (from discussion)**:\n{}\n",
                escape_xml_text(trimmed)
            ));
        }
    }
    if !issue.plan.is_empty() {
        out.push_str("### Decomposed Subplan Steps:\n");
        render_subplan_exec(&mut out, &issue.plan, 0);
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on resolving review finding `#{}`.\n",
        issue.id
    ));
    out.push_str(
        "2. Do NOT touch unrelated code or address other findings outside of this scope.\n",
    );
    out.push_str(
        "3. Own the implementation (`How`) only, never the requirement (`What`): the architecture prescribed by the finding is fixed by the review. Replacing the designed data structures, rewriting the interfaces or contracts it relies on, substituting the selected components or libraries, or relaxing its constraints and invariants is strictly prohibited.\n",
    );
    out.push_str(
        "4. Treat every file listed under Protected Contracts as immutable: rely on its interfaces and invariants, but NEVER modify it.\n",
    );
    match protocol {
        EditProtocolKind::Structured => {
            out.push_str("5. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT fill the `verify` field.\n");
            out.push_str("6. When implementation and verification are complete, set the `outcome` field to status \"success\" and fill in `summary` describing the resolved finding.\n");
            out.push_str("7. If the finding is already resolved by earlier changes, is a false positive, or requires no code modifications, do NOT emit unnecessary edits; instead set the `outcome` field to status \"skip\" and fill in `reason` with a detailed explanation.\n");
            out.push_str("8. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately by listing them in the `context_requests` array before writing code or explanations: a repository-relative path is read-only, and an `editable:` prefix (such as `editable:crates/x/src/parser.rs`) marks it editable. Request all necessary files together in a single response to avoid exhausting discovery rounds. Do NOT halt with conversational text if the necessary files can simply be requested.\n");
            out.push_str("9. If the finding cannot be resolved within the approved architectural decision, because the fix would require a different data structure, a changed interface, a substituted component, or a relaxed constraint, do NOT improvise an alternative design. Stop and fill the `blocked` field with `is_blocked` true, a `reason`, and `details` describing the exact architectural conflict, so the harness starts a controlled replanning cycle.\n");
            out.push_str("10. If you encounter an insurmountable technical blocker that is unrelated to the architectural decision, set the `outcome` field to status \"error\" and fill in `message` with the exact blocker.\n");
        }
        EditProtocolKind::Xml | EditProtocolKind::Mixed => {
            out.push_str("5. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
            out.push_str(&format!(
                "6. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
                marker_suffix
            ));
            out.push_str(&format!(
                "7. If the finding is already resolved by earlier changes, is a false positive, or requires no code modifications, do NOT emit unnecessary edits; instead emit <skip{0} reason=\"...\">Detailed explanation</skip{0}>.\n",
                marker_suffix
            ));
            out.push_str(&format!(
                "8. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
                marker_suffix
            ));
            out.push_str(&format!(
                "9. If the finding cannot be resolved within the approved architectural decision, because the fix would require a different data structure, a changed interface, a substituted component, or a relaxed constraint, do NOT improvise an alternative design. Stop and emit <blocked{0} reason=\"...\" details=\"...\" /> describing the exact architectural conflict, so the harness starts a controlled replanning cycle.\n",
                marker_suffix
            ));
            out.push_str(&format!(
                "10. If you encounter an insurmountable technical blocker that is unrelated to the architectural decision, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
                marker_suffix
            ));
        }
    }

    Ok(out)
}

/// Renders a decomposition plan as an indented outline of its steps.
fn render_subplan_exec(out: &mut String, items: &[PlanItem], depth: usize) {
    let indent = "  ".repeat(depth);
    for sub in items {
        out.push_str(&format!(
            "{}- [{}] #{} {}",
            indent,
            sub.status,
            escape_xml_text(&sub.id),
            escape_xml_text(&sub.title)
        ));
        if !sub.files.is_empty() {
            out.push_str(&format!(" (files: {})", escape_list(&sub.files)));
        }
        if !sub.readonly_files.is_empty() {
            out.push_str(&format!(
                " (readonly: {})",
                escape_list(&sub.readonly_files)
            ));
        }
        if let Some(details) = &sub.details {
            let trimmed = details.trim();
            if !trimmed.is_empty() {
                out.push_str(&format!(": {}", escape_xml_text(trimmed)));
            }
        }
        out.push('\n');
        if !sub.children.is_empty() {
            render_subplan_exec(out, &sub.children, depth + 1);
        }
    }
}

/// Prefix recorded in `resolution_notes` when a domain failure skips a finding.
const REVIEW_STEP_FAILURE_NOTE_PREFIX: &str = "Skipped after execution failure";

/// Category of a review step execution failure, controlling batch behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStepFailureKind {
    /// The finding could not be resolved in code: a patch failed to apply, the
    /// compiler still reported errors after healing, the retry budget was
    /// exhausted, or the model emitted a semantic block. The finding is marked
    /// `Skipped` and the batch continues with the next one.
    Domain,
    /// The turn failed before producing a usable result: a provider or
    /// transport error, a missing credential, an operation-slot conflict, or an
    /// explicit cancellation. The failure is fatal to the whole batch.
    Infrastructure,
}

/// A review step execution failure paired with its batch-impact category.
///
/// The category decides whether the batch supervisor aborts or advances: a
/// [`ReviewStepFailureKind::Domain`] failure is confined to the current finding,
/// while a [`ReviewStepFailureKind::Infrastructure`] failure stops the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStepFailure {
    kind: ReviewStepFailureKind,
    reason: String,
    failed_step: Option<String>,
}

impl ReviewStepFailure {
    /// Creates a domain failure: skip the current finding and continue.
    pub fn domain(reason: impl Into<String>) -> Self {
        Self {
            kind: ReviewStepFailureKind::Domain,
            reason: reason.into(),
            failed_step: None,
        }
    }

    /// Creates an infrastructure failure: abort the whole batch.
    pub fn infrastructure(reason: impl Into<String>) -> Self {
        Self {
            kind: ReviewStepFailureKind::Infrastructure,
            reason: reason.into(),
            failed_step: None,
        }
    }

    /// Attributes the failure to the sub-step whose execution failed.
    ///
    /// A sub-step identity is the diagnostic the Architect needs to reproject the
    /// remaining horizon around the failed approach, so it is attached where the
    /// sub-step actually failed rather than reconstructed by the caller.
    pub fn with_step(mut self, step_id: impl Into<String>) -> Self {
        self.failed_step = Some(step_id.into());
        self
    }

    /// The sub-step whose execution failed, when the failure is attributable to one.
    pub fn failed_step(&self) -> Option<&str> {
        self.failed_step.as_deref()
    }

    /// Whether the failure is confined to the current finding.
    pub fn is_domain(&self) -> bool {
        matches!(self.kind, ReviewStepFailureKind::Domain)
    }

    /// Human-readable diagnostic carried by the failure.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Consumes the failure and returns its diagnostic.
    pub fn into_reason(self) -> String {
        self.reason
    }
}

/// Builds the `resolution_notes` diagnostic recorded when a finding is skipped
/// because a domain failure prevented it from being resolved in code.
pub fn format_review_step_failure_note(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        format!("{} without diagnostics", REVIEW_STEP_FAILURE_NOTE_PREFIX)
    } else {
        format!("{}: {}", REVIEW_STEP_FAILURE_NOTE_PREFIX, trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueStatus, IssueType, ReviewIssue, ReviewItem};

    /// Builds a session holding one finding with a protected contract file.
    fn session() -> ReviewSession {
        let issue = ReviewIssue {
            id: 3,
            title: "Guard the parser".to_string(),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: "The parser panics on truncated input.".to_string(),
            recommendations: vec!["Return a structured error".to_string()],
            files: vec!["crates/x/src/parser.rs".to_string()],
            readonly_files: vec!["crates/x/src/api.rs".to_string()],
            plan: Vec::new(),
            note: Some("Keep the public signature stable.".to_string()),
        };
        ReviewSession {
            id: "R-1".to_string(),
            title: "Parser hardening".to_string(),
            items: vec![ReviewItem::from(issue)],
            ..Default::default()
        }
    }

    #[test]
    fn execution_prompt_carries_the_finding_context() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Xml, None)
                .expect("prompt");

        assert!(prompt.contains("# Target Review Step Execution"));
        assert!(prompt.contains("Guard the parser"));
        assert!(prompt.contains("The parser panics on truncated input."));
        assert!(prompt.contains("Return a structured error"));
        assert!(prompt.contains("crates/x/src/parser.rs"));
        assert!(prompt.contains("Keep the public signature stable."));
        // No decomposition was produced, so no subplan section is rendered.
        assert!(!prompt.contains("Decomposed Subplan Steps"));
    }

    #[test]
    fn execution_prompt_forbids_architectural_drift() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Xml, None)
                .expect("prompt");

        // The finding fixes the decision; the turn owns only its execution.
        assert!(prompt.contains("never the requirement (`What`)"));
        assert!(prompt.contains("Replacing the designed data structures"));
        assert!(prompt.contains("do NOT improvise an alternative design"));
        assert!(prompt.contains("Protected Contracts"));
        assert!(prompt.contains("NEVER modify it"));
    }

    #[test]
    fn execution_prompt_routes_control_tags_through_the_marker() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Xml, Some("M7"))
                .expect("prompt");

        for tag in [
            "<success_M7 summary=",
            "<skip_M7 reason=",
            "<blocked_M7 reason=",
            "<error_M7 message=",
            "<context_request_M7 path=",
        ] {
            assert!(prompt.contains(tag), "missing {tag}");
        }
        assert!(!prompt.contains("<success summary="));
    }

    #[test]
    fn structured_execution_prompt_speaks_the_field_vocabulary() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Structured, None)
                .expect("prompt");

        // A structured turn emits no command tag at all.
        for tag in [
            "<success",
            "<skip",
            "<context_request",
            "<blocked",
            "<error",
            "<verify",
        ] {
            assert!(!prompt.contains(tag), "unexpected command tag {tag}");
        }

        // The directives name the response fields of the structured schema.
        assert!(prompt.contains("`outcome`"));
        assert!(prompt.contains("`blocked`"));
        assert!(prompt.contains("`context_requests`"));
        assert!(prompt.contains("`verify`"));

        // Payload blocks the model reads stay in both contours.
        assert!(prompt.contains("<repo_map>"));
        assert!(prompt.contains("<context>"));
    }

    #[test]
    fn structured_execution_prompt_ignores_the_marker() {
        let prompt = format_review_step_execution_prompt(
            &session(),
            3,
            EditProtocolKind::Structured,
            Some("M7"),
        )
        .expect("prompt");

        assert!(!prompt.contains("M7"));
    }

    #[test]
    fn structured_skip_directive_closes_the_reason_field_reference() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Structured, None)
                .expect("prompt");

        // The skip directive names the field with balanced backticks, so the
        // rendered prompt carries no stray quote character.
        assert!(prompt.contains("fill in `reason` with a detailed explanation"));
        assert!(!prompt.contains("`reason\""));
    }

    #[test]
    fn mixed_execution_prompt_keeps_the_tagged_contour() {
        let prompt =
            format_review_step_execution_prompt(&session(), 3, EditProtocolKind::Mixed, Some("M7"))
                .expect("prompt");

        // Mixed encodes code edits through the tagged contour.
        assert!(prompt.contains("<success_M7 summary="));
    }

    #[test]
    fn execution_prompt_rejects_an_unknown_finding() {
        let unknown =
            format_review_step_execution_prompt(&session(), 99, EditProtocolKind::Xml, None);
        assert!(unknown.is_err());
    }

    #[test]
    fn retry_prompt_demands_a_protocol_operation() {
        let prompt = format_review_step_retry_prompt(
            "finished <edit> nothing",
            Some("no edits"),
            EditProtocolKind::Xml,
            None,
        );

        assert!(prompt.starts_with(REVIEW_STEP_RETRY_PREFIX));
        assert!(prompt.contains("&lt;edit&gt;"));
        assert!(prompt.contains("no edits"));
        assert!(prompt.contains("terminal success tag"));
    }

    #[test]
    fn structured_retry_prompt_speaks_the_field_vocabulary() {
        let prompt = format_review_step_retry_prompt(
            "I stopped early.",
            Some("no edits"),
            EditProtocolKind::Structured,
            Some("M7"),
        );

        assert!(!prompt.contains("<success"));
        assert!(!prompt.contains("M7"));
        assert!(prompt.contains("`changes`"));
        assert!(prompt.contains("`outcome`"));
    }
}
