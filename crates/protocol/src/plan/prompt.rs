use super::item::{ConceptItem, PlanItem};
use super::node::Plan;

fn escape_xml_attr(val: &str) -> String {
    val.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub const PLAN_DISCUSSION_PREFIX: &str = "# Plan Discussion:";

/// Returns true if the prompt represents an explicit plan discussion turn.
pub fn is_plan_discussion_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(PLAN_DISCUSSION_PREFIX)
}

/// Extracts the target plan ID from an explicit plan discussion prompt.
pub fn extract_plan_discussion_id(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with(PLAN_DISCUSSION_PREFIX) {
        return None;
    }
    let rest = trimmed.strip_prefix(PLAN_DISCUSSION_PREFIX)?.trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let id = spec.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// Formats a targeted prompt for discussing and refining a plan or specific marked items.
///
/// Preserves session history and passes the targeted plan structure, focused items,
/// and developer question or instructions for architectural refinement.
pub fn format_plan_discussion_prompt(
    plan: &Plan,
    focused_item_ids: &[String],
    user_comment: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        PLAN_DISCUSSION_PREFIX, plan.id, plan.title
    ));

    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("## Plan Architecture & Context:\n{}\n\n", trimmed));
        }
    }

    out.push_str("## Target Plan Context:\n");
    let (total, completed) = plan.count_stats();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        escape_xml_attr(&plan.id),
        escape_xml_attr(&plan.title),
        completed,
        total
    ));
    if let Some(desc) = &plan.description {
        out.push_str(&format!("  <description>{}</description>\n", desc.trim()));
    }

    if !plan.concept.is_empty() {
        out.push_str("  <concept>\n");
        fn render_concept_xml(
            out: &mut String,
            items: &[ConceptItem],
            depth: usize,
            focused_item_ids: &[String],
        ) {
            let indent = "  ".repeat(depth + 2);
            for item in items {
                let is_focused = focused_item_ids.iter().any(|id| id == &item.id);
                let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
                out.push_str(&format!("{}- #{}{} {}", indent, item.id, focus_marker, item.title));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                if !item.children.is_empty() {
                    render_concept_xml(out, &item.children, depth + 1, focused_item_ids);
                }
            }
        }
        render_concept_xml(&mut out, &plan.concept, 0, focused_item_ids);
        out.push_str("  </concept>\n");
    }

    fn render_discussion_items(
        out: &mut String,
        items: &[PlanItem],
        depth: usize,
        focused_item_ids: &[String],
    ) {
        let indent = "  ".repeat(depth + 1);
        for item in items {
            let is_focused = focused_item_ids.iter().any(|id| id == &item.id);
            let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
            out.push_str(&format!(
                "{}- {}{} (id: \"{}\", status: {})",
                indent, item.title, focus_marker, item.id, item.status
            ));
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", trimmed));
                }
            }
            out.push('\n');
            if !item.children.is_empty() {
                render_discussion_items(out, &item.children, depth + 1, focused_item_ids);
            }
        }
    }
    render_discussion_items(&mut out, &plan.items, 0, focused_item_ids);
    out.push_str("</active_plan>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = plan.find_item(id) {
                out.push_str(&format!("- **#{} {}** (status: {})\n", item.id, item.title, item.status));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", trimmed));
                    }
                }
            } else if let Some(concept) = plan.find_concept_item(id) {
                out.push_str(&format!("- **[CONCEPT] #{} {}**\n", concept.id, concept.title));
                if let Some(details) = &concept.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", trimmed));
                    }
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the plan architecture and steps.\n");
    out.push_str("2. Reins Invariant: Never mutate the plan (keep `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`.\n");
    out.push_str("3. When explicitly instructed to update the plan, explain the proposed modifications in `message` and populate the structured `plan_update` field.\n");
    out.push_str("4. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite the plan (mandatory `concept` tree defining architectural intent, with optional implementation `items`). Use `action: \"update\"` to modify individual existing items or concept nodes by ID without altering unmentioned items. Use `action: \"delete\"` to remove the plan.\n");
    out.push_str("5. Keep explanations clear, rigorous, and directly aligned with codebase design.\n");
    out.push_str("6. Language Consistency Directive: Detect the natural language of the plan title and description. Formulate all plan titles, descriptions, step titles, step details, and conversational explanations strictly in that same language.\n");

    out
}
