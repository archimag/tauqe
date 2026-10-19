use crate::context::ContextFileContent;
use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use tauqe_protocol::review::ReviewSession;
use tauqe_protocol::PlanItem;

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
                out.push_str(&format!(
                    "- **#{} [{}] {}** (status: {})\n",
                    issue.id,
                    issue.issue_type,
                    escape_xml_text(&issue.title),
                    issue.status
                ));
                if !issue.files.is_empty() {
                    let joined = issue
                        .files
                        .iter()
                        .map(|f| format!("`{}`", escape_xml_text(f)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  Files: {}\n", joined));
                }
                if !issue.readonly_files.is_empty() {
                    let joined = issue
                        .readonly_files
                        .iter()
                        .map(|f| format!("`{}`", escape_xml_text(f)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  Protected Contracts (readonly): {}\n", joined));
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
                if !issue.plan.is_empty() {
                    out.push_str("  Decomposition Plan:\n");
                    render_plan_discussion(&mut out, &issue.plan, 0);
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the code review findings.\n");
    out.push_str("2. Select the response mode that the developer's request calls for. When the developer is reasoning about the findings, analyze the problem, its edge cases, and potential solutions conceptually. When the developer asks you to resolve a finding, apply the required code changes directly instead of only describing them, while preserving the surrounding review context.\n");
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications clearly in conversational text in `message`; the developer applies status changes through the review interface.\n");
    out.push_str("4. Treat every file listed as a protected contract as immutable: rely on its interfaces and invariants, but never modify it.\n");
    out.push_str("5. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
}

/// Renders a finding's decomposition plan as an indented outline.
fn render_plan_discussion(out: &mut String, items: &[PlanItem], depth: usize) {
    let indent = "  ".repeat(depth + 2);
    for sub in items {
        out.push_str(&format!(
            "{}- #{}: {} ({})\n",
            indent,
            sub.id,
            escape_xml_text(&sub.title),
            sub.status
        ));
        if !sub.children.is_empty() {
            render_plan_discussion(out, &sub.children, depth + 1);
        }
    }
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
    prompt.push_str("Do NOT output markdown headers, diffs, or XML edit tags; this is an audit-only review.\n");

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
