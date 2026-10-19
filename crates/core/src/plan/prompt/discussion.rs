use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use tauqe_protocol::{ConceptItem, Plan, PlanItem, PlanItemStatus};

pub const PLAN_DISCUSSION_PREFIX: &str = "# Plan Discussion:";

/// Joins identifiers or paths into a single escaped, comma-separated line.
fn join_escaped(items: &[String]) -> String {
    items
        .iter()
        .map(|item| escape_xml_text(item))
        .collect::<Vec<_>>()
        .join(", ")
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
        PLAN_DISCUSSION_PREFIX,
        single_line(&plan.id),
        single_line(&plan.title)
    ));

    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Plan Architecture & Context:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
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
        out.push_str(&format!(
            "  <description>{}</description>\n",
            escape_xml_text(desc.trim())
        ));
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
                let is_focused = focused_item_ids
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&item.id));
                let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
                out.push_str(&format!(
                    "{}- #{}{} {}",
                    indent,
                    item.id,
                    focus_marker,
                    escape_xml_text(&item.title)
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", escape_xml_text(trimmed)));
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
            let is_focused = focused_item_ids
                .iter()
                .any(|id| id.eq_ignore_ascii_case(&item.id));
            let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
            out.push_str(&format!(
                "{}- {}{} (id: \"{}\", status: {})",
                indent,
                escape_xml_text(&item.title),
                focus_marker,
                escape_xml_text(&item.id),
                item.status
            ));
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", escape_xml_text(trimmed)));
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
                out.push_str(&format!(
                    "- **#{} {}** (status: {})\n",
                    item.id,
                    escape_xml_text(&item.title),
                    item.status
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", escape_xml_text(trimmed)));
                    }
                }
            } else if let Some(concept) = plan.find_concept_item(id) {
                out.push_str(&format!(
                    "- **[CONCEPT] #{} {}**\n",
                    concept.id,
                    escape_xml_text(&concept.title)
                ));
                if let Some(details) = &concept.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", escape_xml_text(trimmed)));
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
    out.push_str(
        "5. Keep explanations clear, rigorous, and directly aligned with codebase design.\n",
    );
    out.push_str("6. Language Consistency Directive: Detect the natural language of the plan title and description. Formulate all plan titles, descriptions, step titles, step details, and conversational explanations strictly in that same language.\n");

    out
}

/// Formats the plan, its concept tree, and implementation items into a prompt block for LLM context.
pub fn format_active_plan_context(plan: &Plan) -> Option<String> {
    if plan.concept.is_empty() && plan.items.is_empty() {
        return None;
    }

    let (total, completed) = plan.count_stats();
    let mut out = String::new();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        escape_xml_attr(&plan.id),
        escape_xml_attr(&plan.title),
        completed,
        total
    ));

    if let Some(desc) = &plan.description {
        out.push_str(&format!(
            "  <description>{}</description>\n",
            escape_xml_text(desc.trim())
        ));
    }

    if !plan.concept.is_empty() {
        out.push_str("  <concept>\n");
        fn render_concept(out: &mut String, items: &[tauqe_protocol::ConceptItem], depth: usize) {
            let indent = "  ".repeat(depth + 2);
            for item in items {
                out.push_str(&format!(
                    "{}- {} (id: \"{}\")",
                    indent,
                    escape_xml_text(&item.title),
                    escape_xml_text(&item.id)
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                    }
                }
                out.push('\n');
                if !item.children.is_empty() {
                    render_concept(out, &item.children, depth + 1);
                }
            }
        }
        render_concept(&mut out, &plan.concept, 0);
        out.push_str("  </concept>\n");
    }

    if !plan.items.is_empty() {
        fn render_items(out: &mut String, items: &[PlanItem], depth: usize) {
            let indent = "  ".repeat(depth + 1);
            for item in items {
                let status_mark = match item.status {
                    PlanItemStatus::Discussion => "DISCUSSION",
                    PlanItemStatus::Todo => "TODO",
                    PlanItemStatus::InProgress => "IN_PROGRESS",
                    PlanItemStatus::Done => "DONE",
                    PlanItemStatus::Blocked => "BLOCKED",
                    PlanItemStatus::Cancelled => "CANCELLED",
                };

                out.push_str(&format!(
                    "{}- {} (id: \"{}\", status: {})",
                    indent,
                    escape_xml_text(&item.title),
                    escape_xml_text(&item.id),
                    status_mark
                ));

                if !item.concept_refs.is_empty() {
                    out.push_str(&format!(
                        " [concept_refs: {}]",
                        join_escaped(&item.concept_refs)
                    ));
                }
                if !item.files.is_empty() {
                    out.push_str(&format!(" [files: {}]", join_escaped(&item.files)));
                }
                if !item.depends_on.is_empty() {
                    out.push_str(&format!(
                        " [depends_on: {}]",
                        join_escaped(&item.depends_on)
                    ));
                }

                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                    }
                }
                out.push('\n');

                if !item.children.is_empty() {
                    render_items(out, &item.children, depth + 1);
                }
            }
        }

        render_items(&mut out, &plan.items, 0);
    }
    out.push_str("</active_plan>");

    Some(out)
}
