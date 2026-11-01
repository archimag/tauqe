use crate::context::ContextFileContent;
use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use tauqe_protocol::review::{ReviewGroup, ReviewIssue, ReviewItem, ReviewSession};

use super::refine::render_path_list;
pub const REVIEW_DISCUSSION_PREFIX: &str = "# Review Discussion:";

/// Formats a targeted prompt for discussing and analyzing review findings in Structured Output mode.
pub fn format_review_discussion_prompt(
    session: &ReviewSession,
    focused_item_id: Option<u32>,
    user_comment: &str,
) -> String {
    let ids: Vec<u32> = focused_item_id.into_iter().collect();
    format_review_discussion_prompt_items(session, &ids, user_comment)
}

/// Formats a targeted prompt for discussing multiple focused review findings in Structured Output mode.
pub fn format_review_discussion_prompt_items(
    session: &ReviewSession,
    focused_item_ids: &[u32],
    user_comment: &str,
) -> String {
    // Findings are rendered through the enriched domain view: the description
    // states the defect, the recommendations carry the remediation strategy,
    // and the protected contracts define what a fix must rely on unchanged.
    let issues = session.issues();
    let mut out = String::new();
    let clean_title = single_line(&session.title);
    let title = if clean_title.is_empty() {
        session.id.as_str()
    } else {
        clean_title.as_str()
    };
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        REVIEW_DISCUSSION_PREFIX,
        single_line(&session.id),
        title
    ));

    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Review Scope & Description:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
        }
    }

    out.push_str("## Target Review Session Context:\n");
    let (total, fixed) = session.stats();
    out.push_str(&format!(
        "<active_review id=\"{}\" title=\"{}\" progress=\"{}/{} fixed\">\n",
        escape_xml_attr(&session.id),
        escape_xml_attr(title),
        fixed,
        total
    ));

    for issue in &issues {
        let is_focused = focused_item_ids.contains(&issue.id);
        let focus_mark = if is_focused { " [FOCUSED]" } else { "" };
        let loc = if issue.files.is_empty() {
            String::new()
        } else {
            let joined = issue
                .files
                .iter()
                .map(|f| escape_xml_text(f))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" ({})", joined)
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}{}\n",
            issue.id,
            issue.issue_type,
            escape_xml_text(&issue.title),
            loc,
            focus_mark
        ));
    }
    out.push_str("</active_review>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(issue) = issues.iter().find(|i| i.id == *id) {
                push_issue_details(&mut out, issue);
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str(
        "1. Address the developer's question or feedback regarding the code review findings.\n",
    );
    out.push_str("2. Select the response mode that the developer's request calls for. When the developer is reasoning about the findings, analyze the problem, its edge cases, and potential solutions conceptually. When the developer asks you to resolve a finding, apply the required code changes directly instead of only describing them, while preserving the surrounding review context.\n");
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications clearly in conversational text in `message`; the developer applies status changes through the review interface.\n");
    out.push_str("4. Treat every file listed as a protected contract as immutable: rely on its interfaces and invariants, but never modify it.\n");
    out.push_str("5. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
}

/// Renders one finding as detailed discussion material.
///
/// The description states the defect, the recommendations carry the remediation
/// strategy, and the protected contracts define what a fix must rely on
/// unchanged, so a dialogue over one finding and a dialogue over a whole group
/// present the same finding the same way.
fn push_issue_details(out: &mut String, issue: &ReviewIssue) {
    out.push_str(&format!(
        "- **#{} [{}] {}** (status: {})\n",
        issue.id,
        issue.issue_type,
        escape_xml_text(&issue.title),
        issue.status
    ));
    if !issue.files.is_empty() {
        out.push_str(&format!("  Files: {}\n", render_path_list(&issue.files)));
    }
    if !issue.readonly_files.is_empty() {
        out.push_str(&format!(
            "  Protected Contracts (readonly): {}\n",
            render_path_list(&issue.readonly_files)
        ));
    }
    if !issue.description.trim().is_empty() {
        out.push_str(&format!(
            "  Description: {}\n",
            escape_xml_text(issue.description.trim())
        ));
    }
    if !issue.recommendations.is_empty() {
        out.push_str("  Recommendations:\n");
        for (index, rec) in issue.recommendations.iter().enumerate() {
            out.push_str(&format!("    {}. {}\n", index + 1, escape_xml_text(rec)));
        }
    }
    if let Some(note) = &issue.note {
        if !note.trim().is_empty() {
            out.push_str(&format!(
                "  Resolution Note: {}\n",
                escape_xml_text(note.trim())
            ));
        }
    }
}

/// Formats a targeted prompt for discussing the architectural root cause of one
/// review group in Structured Output mode.
///
/// A group is the operational unit of remediation: its findings manifest one
/// shared cause and its blocker belongs to the group itself. The dialogue
/// therefore renders the cause, the active blocker, the recorded decision, and
/// every manifestation, and it instructs the model to record the settled
/// decision through the structured protocol instead of leaving the engineer to
/// re-enter the outcome by hand in the review interface.
pub fn format_review_group_discussion_prompt(
    session: &ReviewSession,
    group: &ReviewGroup,
    findings: &[ReviewItem],
    user_comment: &str,
) -> String {
    // Findings are rendered through the enriched domain view, exactly as a
    // focused finding dialogue renders them, so both contours of the review
    // discussion present a finding the same way.
    let issues: Vec<ReviewIssue> = findings.iter().cloned().map(ReviewIssue::from).collect();
    let mut out = String::new();
    let clean_group_title = single_line(&group.title);
    let group_title = if clean_group_title.is_empty() {
        single_line(&group.id)
    } else {
        clean_group_title
    };
    let clean_session_title = single_line(&session.title);
    let session_title = if clean_session_title.is_empty() {
        session.id.as_str()
    } else {
        clean_session_title.as_str()
    };
    out.push_str(&format!(
        "# Review Group Discussion: [{}] {}\n\n",
        single_line(&session.id),
        escape_xml_text(&group_title)
    ));

    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Review Scope & Description:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
        }
    }

    out.push_str("## Target Review Group:\n");
    out.push_str(&format!("- **ID**: `{}`\n", escape_xml_text(&group.id)));
    out.push_str(&format!("- **Title**: {}\n", escape_xml_text(&group_title)));
    let root_cause = group
        .description
        .as_deref()
        .map(str::trim)
        .filter(|cause| !cause.is_empty());
    if let Some(root_cause) = root_cause {
        out.push_str(&format!(
            "- **Root Cause**:\n{}\n",
            escape_xml_text(root_cause)
        ));
    }
    let blocker = group
        .blocked_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    match blocker {
        Some(blocker) => out.push_str(&format!(
            "- **Current Blocker**:\n{}\n",
            escape_xml_text(blocker)
        )),
        // The dialogue states the absence explicitly, so the model never reads a
        // missing section as a blocker it has to guess.
        None => out.push_str("- **Current Blocker**: none recorded\n"),
    }
    let decision = group
        .resolution_notes
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty());
    if let Some(decision) = decision {
        out.push_str(&format!(
            "- **Recorded Decision**:\n{}\n",
            escape_xml_text(decision)
        ));
    }
    out.push('\n');

    out.push_str("## Target Review Session Context:\n");
    let (total, fixed) = session.stats();
    out.push_str(&format!(
        "<active_review id=\"{}\" title=\"{}\" progress=\"{}/{} fixed\">\n",
        escape_xml_attr(&session.id),
        escape_xml_attr(session_title),
        fixed,
        total
    ));
    for issue in &issues {
        let loc = if issue.files.is_empty() {
            String::new()
        } else {
            format!(" ({})", render_path_list(&issue.files))
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}\n",
            issue.id,
            issue.issue_type,
            escape_xml_text(&issue.title),
            loc
        ));
    }
    out.push_str("</active_review>\n\n");

    out.push_str("### Findings the Group Collects:\n");
    for issue in &issues {
        push_issue_details(&mut out, issue);
    }
    out.push('\n');

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the group's shared root cause and the findings that manifest it.\n");
    out.push_str("2. Select the response mode that the developer's request calls for. When the developer is reasoning about the cause, analyze the problem, its edge cases, and potential solutions conceptually. When the developer asks you to resolve it, apply the required code changes directly instead of only describing them, while preserving the surrounding review context.\n");
    out.push_str("3. Record the decision once the developer has reached it: populate `review_update` with `group_id` naming this group, `item_id` naming the finding that carried the discussion, `status` naming what the group moves to (`todo` queues it for execution, `skipped` records that it is not reproducible, `rejected` records a false positive or an accepted trade-off), `note` stating the decision itself in the developer's words, and `recommendations` for the remediation the discussion agreed. The harness clears the group blocker, stamps the note onto every finding of the group, and moves the whole group to the decided status.\n");
    out.push_str("4. Keep `review_update` null while the decision is still open: a question, an analysis, or a trade-off review is not a decision, and a recorded decision is never restated as an open question later.\n");
    out.push_str("5. Treat every file listed as a protected contract as immutable: rely on its interfaces and invariants, but never modify it.\n");
    out.push_str("6. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
}

/// Builds the system prompt for the isolated code review session.
/// Focuses purely on architectural soundness, safety, edge cases, and performance.
pub fn build_review_system_prompt() -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an expert Principal Systems Architect and Code Reviewer.\n\n");
    prompt.push_str("## Mission\n");
    prompt.push_str("Your task is to conduct an in-depth, rigorous, and actionable code review of the provided codebase files.\n");
    prompt.push_str("Focus on:\n");
    prompt.push_str("1. Architectural integrity, separation of concerns, and cohesion.\n");
    prompt.push_str("2. Concurrency, data races, deadlocks, resource leaks, and panic safety.\n");
    prompt.push_str("3. Edge-case and error handling robustness.\n");
    prompt.push_str("4. Performance bottlenecks and unnecessary allocations.\n");
    prompt.push_str("5. Idiomatic conventions and testability.\n\n");

    prompt.push_str("## Language Invariant\n");
    prompt.push_str("Formulate the `summary` Markdown narrative and all finding titles, descriptions, and recommendations in the primary natural language established by the user's instructions or prompt, while keeping code identifiers, symbols, and keywords exact.\n\n");

    prompt.push_str("## Output Format Specification\n");
    prompt.push_str("You MUST respond with a single JSON object that conforms to the enforced structured output schema. Do not wrap the JSON in markdown code fences and do not add any prose outside the JSON object.\n\n");
    prompt.push_str("The object contains exactly three fields, emitted in this order:\n");
    prompt.push_str("- `summary`: a high-level conclusion formatted as Markdown. Before enumerating concrete findings, synthesize the overall assessment of architectural integrity, separation of concerns, concurrency and safety risks, error handling, and reliability across the reviewed files. Do not merely restate the findings list.\n");
    prompt.push_str("- `title`: a concise, human-readable title for the review session.\n");
    prompt.push_str("- `findings`: an array of finding objects with the following fields:\n");
    prompt.push_str("  - `title`: concise finding title.\n");
    prompt.push_str("  - `type`: exactly one of the following string values:\n");
    prompt.push_str("    - `bug`: incorrect behavior, panic, data corruption, or resource leak.\n");
    prompt.push_str("    - `security`: security flaw or unsafe boundary.\n");
    prompt.push_str("    - `architecture`: structural or runtime design risk that must be decomposed before implementation.\n");
    prompt.push_str("    - `performance`: costly hot path or wasted resources.\n");
    prompt.push_str("    - `quality`: maintainability, readability, or dead-code cleanup.\n");
    prompt.push_str("  - `description`: the technical defect: what is wrong, why it occurs, and its root cause. State the defect itself; never fold remediation into the description.\n");
    prompt.push_str("  - `recommendations`: an ordered array of concrete, actionable remediation steps that resolve the defect.\n");
    prompt.push_str("  - `files`: an array of repository-relative paths to modify when resolving the finding (use `[]` when the finding is not file-specific).\n");
    prompt.push_str("  - `readonly_files`: an array of repository-relative paths of immutable contracts, interfaces, or invariants the fix must rely on but must never modify (use `[]` when the finding depends on none). These files are protected context.\n\n");

    prompt.push_str("## Decomposition Guidance\n");
    prompt.push_str("When a finding is a complex architectural defect that cannot be resolved by a single localized change, its `recommendations` MUST begin by instructing that the finding be decomposed through architectural refinement before any implementation begins, so the engineer decomposes the defect into executable steps first.\n\n");

    prompt.push_str("If there are no issues, respond with an empty `findings` array.\n");
    prompt.push_str(
        "Do NOT output markdown headers, diffs, or XML edit tags; this is an audit-only review.\n",
    );

    prompt
}

/// Builds the user prompt containing pure context files without history or repo map.
pub fn build_review_user_prompt(
    files: &[ContextFileContent],
    custom_prompt: Option<&str>,
) -> String {
    let mut out = String::new();

    if let Some(user_req) = custom_prompt {
        let trimmed = user_req.trim();
        if !trimmed.is_empty() {
            out.push_str("## Review Focus & User Instructions\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    out.push_str("## Code Files Under Review\n\n");
    if files.is_empty() {
        out.push_str("No files provided in context.\n");
    } else {
        for f in files {
            out.push_str(&format!("<file path=\"{}\">\n", escape_xml_attr(&f.path)));
            out.push_str(&f.content);
            if !f.content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</file>\n\n");
        }
    }

    out.push_str("Please provide your structured code review findings following the requested specification.\n");
    out
}
