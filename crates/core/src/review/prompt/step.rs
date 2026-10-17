use tauqe_protocol::review::ReviewSession;
use tauqe_protocol::PlanItem;

pub const REVIEW_STEP_RETRY_PREFIX: &str = "# Correction Required: Review Finding Turn Incomplete";

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
        out.push_str(previous);
        out.push_str("\n</previous_response>\n\n");
    }

    if let Some(err) = error {
        let trimmed = err.trim();
        if !trimmed.is_empty() {
            out.push_str("## Diagnostic Feedback:\n");
            out.push_str(trimmed);
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

/// Target review and item identifiers parsed from an isolated review step execution prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStepTarget {
    pub review_id: Option<String>,
    pub item_id: u32,
}

/// Returns true if the prompt represents an isolated review step execution turn.
pub fn is_review_step_execution_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    trimmed.starts_with(REVIEW_STEP_EXECUTION_PREFIX)
        || trimmed.starts_with(REVIEW_STEP_RETRY_PREFIX)
}

/// Extracts the target review ID and item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_target(prompt: &str) -> Option<ReviewStepTarget> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with(REVIEW_STEP_EXECUTION_PREFIX) {
        return None;
    }
    let rest = trimmed
        .strip_prefix(REVIEW_STEP_EXECUTION_PREFIX)?
        .trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some((rev_id, item_id_str)) = spec.split_once(':') {
            let rev_id = rev_id.trim();
            let item_id: u32 = item_id_str.trim().parse().ok()?;
            return Some(ReviewStepTarget {
                review_id: if rev_id.is_empty() {
                    None
                } else {
                    Some(rev_id.to_string())
                },
                item_id,
            });
        } else if let Ok(item_id) = spec.parse::<u32>() {
            return Some(ReviewStepTarget {
                review_id: None,
                item_id,
            });
        }
    }
    None
}

/// Extracts the target item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_id(prompt: &str) -> Option<u32> {
    extract_review_step_execution_target(prompt).map(|t| t.item_id)
}

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
    let clean_title = issue.title.replace(['\r', '\n'], " ").trim().to_string();
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        REVIEW_STEP_EXECUTION_PREFIX, session.id, issue.id, clean_title
    ));

    let title = if session.title.trim().is_empty() {
        &session.id
    } else {
        session.title.trim()
    };
    out.push_str(&format!("## Review Session: {}\n", title));
    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", trimmed));
        }
    }

    out.push_str("### Target Finding to Resolve:\n");
    out.push_str(&format!("- **ID**: `#{}`\n", issue.id));
    out.push_str(&format!("- **Type**: {}\n", issue.issue_type));
    out.push_str(&format!("- **Title**: {}\n", issue.title));
    if !issue.files.is_empty() {
        let joined = issue
            .files
            .iter()
            .map(|f| format!("`{}`", f))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("- **Files**: {}\n", joined));
    }
    if !issue.readonly_files.is_empty() {
        let joined = issue
            .readonly_files
            .iter()
            .map(|f| format!("`{}`", f))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- **Protected Contracts (readonly_files)**: {}\n",
            joined
        ));
    }
    if !issue.description.trim().is_empty() {
        out.push_str(&format!(
            "- **Description**:\n{}\n",
            issue.description.trim()
        ));
    }
    if !issue.recommendations.is_empty() {
        out.push_str("- **Recommendations**:\n");
        for (index, rec) in issue.recommendations.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, rec));
        }
    }
    if let Some(note) = &issue.note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "- **Resolution Note (from discussion)**:\n{}\n",
                trimmed
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
    out.push_str("2. Do NOT touch unrelated code or address other findings outside of this scope.\n");
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
            sub.id,
            sub.title
        ));
        if !sub.files.is_empty() {
            out.push_str(&format!(" (files: {})", sub.files.join(", ")));
        }
        if !sub.readonly_files.is_empty() {
            out.push_str(&format!(" (readonly: {})", sub.readonly_files.join(", ")));
        }
        if let Some(details) = &sub.details {
            let trimmed = details.trim();
            if !trimmed.is_empty() {
                out.push_str(&format!(": {}", trimmed));
            }
        }
        out.push('\n');
        if !sub.children.is_empty() {
            render_subplan_exec(out, &sub.children, depth + 1);
        }
    }
}

/// Extracts a `<skip>` reason and optional body from an assistant response.
///
/// Recognizes both turn-marker-bound tags (`<skip_MARKER ...>`) and unmarked
/// `<skip ...>` tags, combining the `reason` attribute and inner text when both
/// are present.
pub fn extract_review_skip_reason(text: &str, marker: Option<&str>) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(m) = marker {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            candidates.push(format!("skip_{}", trimmed));
        }
    }
    candidates.push("skip".to_string());

    for tag_name in &candidates {
        let open_prefix = format!("<{}", tag_name);
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(&open_prefix) {
            let start = search_from + rel;
            let after_name = start + open_prefix.len();
            let next_ch = text[after_name..].chars().next();
            if !matches!(next_ch, Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>') | Some('/')) {
                search_from = after_name;
                continue;
            }
            let Some(gt_rel) = text[after_name..].find('>') else {
                break;
            };
            let gt_pos = after_name + gt_rel;
            let header = &text[after_name..gt_pos];
            let self_closing = header.trim_end().ends_with('/');
            let reason_attr = extract_reason_attr(header);

            let body = if self_closing {
                String::new()
            } else {
                let close_tag = format!("</{}>", tag_name);
                if let Some(close_rel) = text[gt_pos + 1..].find(&close_tag) {
                    text[gt_pos + 1..gt_pos + 1 + close_rel].trim().to_string()
                } else {
                    String::new()
                }
            };

            let combined = match (reason_attr, body.is_empty()) {
                (Some(r), true) => r,
                (Some(r), false) if r == body => r,
                (Some(r), false) => format!("{}: {}", r, body),
                (None, false) => body,
                (None, true) => "Skipped without additional details".to_string(),
            };
            return Some(combined);
        }
    }
    None
}

fn extract_reason_attr(header: &str) -> Option<String> {
    let idx = header.find("reason")?;
    let rest = header[idx + "reason".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after_quote = &rest[quote.len_utf8()..];
    let end = after_quote.find(quote)?;
    let val = after_quote[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
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
}

impl ReviewStepFailure {
    /// Creates a domain failure: skip the current finding and continue.
    pub fn domain(reason: impl Into<String>) -> Self {
        Self {
            kind: ReviewStepFailureKind::Domain,
            reason: reason.into(),
        }
    }

    /// Creates an infrastructure failure: abort the whole batch.
    pub fn infrastructure(reason: impl Into<String>) -> Self {
        Self {
            kind: ReviewStepFailureKind::Infrastructure,
            reason: reason.into(),
        }
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
