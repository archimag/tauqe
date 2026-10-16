use crate::context::ContextFileContent;
use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use tauqe_protocol::review::ReviewSession;
use tauqe_protocol::PlanItem;

pub const REVIEW_DISCUSSION_PREFIX: &str = "# Review Discussion:";

/// Returns true if the prompt represents an explicit review discussion turn.
pub fn is_review_discussion_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(REVIEW_DISCUSSION_PREFIX)
}

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

    for item in &session.items {
        let is_focused = focused_item_ids.contains(&item.id);
        let focus_mark = if is_focused { " [FOCUSED]" } else { "" };
        let loc = if item.files.is_empty() {
            String::new()
        } else {
            let joined = item
                .files
                .iter()
                .map(|f| escape_xml_text(f))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" ({})", joined)
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}{}\n",
            item.id,
            item.severity,
            escape_xml_text(&item.title),
            loc,
            focus_mark
        ));
    }
    out.push_str("</active_review>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = session.find_item(*id) {
                out.push_str(&format!(
                    "- **#{} [{}] {}** (status: {})\n",
                    item.id,
                    item.severity,
                    escape_xml_text(&item.title),
                    item.status
                ));
                if !item.files.is_empty() {
                    let joined = item
                        .files
                        .iter()
                        .map(|f| format!("`{}`", escape_xml_text(f)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  Files: {}\n", joined));
                }
                if let Some(notes) = &item.resolution_notes {
                    if !notes.trim().is_empty() {
                        out.push_str(&format!(
                            "  Resolution Strategy: {}\n",
                            escape_xml_text(notes.trim())
                        ));
                    }
                }
                if !item.body.trim().is_empty() {
                    out.push_str(&format!("  Details: {}\n", escape_xml_text(item.body.trim())));
                }
                if item.has_subplan() {
                    out.push_str("  Subplan:\n");
                    fn render_subplan_discussion(
                        out: &mut String,
                        items: &[PlanItem],
                        depth: usize,
                    ) {
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
                                render_subplan_discussion(out, &sub.children, depth + 1);
                            }
                        }
                    }
                    render_subplan_discussion(&mut out, &item.subplan, 0);
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
    out.push_str("2. Discuss the problem, edge cases, and potential solutions conceptually. Do NOT emit file edits or code patches in this discussion turn.\n");
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications clearly in conversational text in `message`; the developer applies status changes through the review interface.\n");
    out.push_str("4. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

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
    prompt.push_str("Formulate the `summary` Markdown narrative and all finding titles and descriptions in the primary natural language established by the user's instructions or prompt, while keeping code identifiers, symbols, and keywords exact.\n\n");

    prompt.push_str("## Output Format Specification\n");
    prompt.push_str("You MUST respond with a single JSON object that conforms to the enforced structured output schema. Do not wrap the JSON in markdown code fences and do not add any prose outside the JSON object.\n\n");
    prompt.push_str("The object contains exactly two fields, emitted in this order:\n");
    prompt.push_str("- `summary`: a high-level conclusion formatted as Markdown. Before enumerating concrete findings, synthesize the overall assessment of architectural integrity, separation of concerns, concurrency and safety risks, error handling, and reliability across the reviewed files. Do not merely restate the findings list.\n");
    prompt.push_str("- `findings`: an array of finding objects with the following fields:\n");
    prompt.push_str("- `title`: concise finding title.\n");
    prompt.push_str("- `severity`: exactly one of the following string values:\n");
    prompt.push_str("  - `critical`: severe bug, panic, data corruption, memory leak, or security flaw.\n");
    prompt.push_str("  - `warning`: architecture violation, potential runtime issue, or poor error handling.\n");
    prompt.push_str("  - `suggestion`: code cleanup, optimization, or idiomatic improvement.\n");
    prompt.push_str("  - `info`: general observation or architectural insight.\n");
    prompt.push_str("- `files`: array of repository-relative file paths affected by this finding (use `[]` when not file-specific).\n");
    prompt.push_str("- `body`: clear explanation of the problem, why it occurs, and concrete recommendations for how to resolve it.\n\n");

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
            out.push_str(&format!("<file path=\"{}\">\n", f.path));
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
