use crate::escape::{escape_xml_text, single_line};
use tauqe_protocol::review::ReviewSession;
use tauqe_protocol::PlanItem;

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

/// Builds a corrective reprompt for a review finding turn that ended without
/// valid code edits and without a terminal success tag.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback (such as a missing edit block or toolchain verification failure),
/// then explicitly demands strict adherence to the active edit protocol on the
/// next turn.
pub fn format_review_step_retry_prompt(assistant_text: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(REVIEW_STEP_RETRY_PREFIX);
    out.push_str("\n\n");
    out.push_str("Your previous turn ended without applying any verified code changes and without emitting the terminal success tag (`<success>`). This violates the execution protocol.\n\n");

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
    out.push_str("1. If the finding requires code changes, emit a valid edit block that strictly conforms to the active edit protocol.\n");
    out.push_str("2. If no code changes are required to resolve the finding, confirm completion explicitly by emitting the terminal success tag.\n");
    out.push_str("3. Do NOT replace protocol operations with conversational text, and do NOT end the turn without either a valid edit block or the terminal success tag.\n");

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
pub fn format_review_step_execution_prompt(
    session: &ReviewSession,
    item_id: u32,
    marker_suffix: &str,
) -> Result<String, String> {
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
    out.push_str("3. Treat every file listed under Protected Contracts as immutable: rely on its interfaces and invariants, but NEVER modify it.\n");
    out.push_str("4. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
    out.push_str(&format!(
        "5. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "6. If the finding is already resolved by earlier changes, is a false positive, or requires no code modifications, do NOT emit unnecessary edits; instead emit <skip{0} reason=\"...\">Detailed explanation</skip{0}>.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "7. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "8. If you encounter an insurmountable technical blocker, contradictory requirements, or need developer decisions, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
        marker_suffix
    ));

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
