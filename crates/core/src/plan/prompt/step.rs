use crate::escape::{escape_xml_text, single_line};
use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

pub const PLAN_STEP_EXECUTION_PREFIX: &str = "# Target Plan Step Execution:";

pub const PLAN_STEP_RETRY_PREFIX: &str = "# Correction Required: Plan Step Turn Incomplete";

/// Joins identifiers or paths into a single escaped, comma-separated line.
fn escape_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| escape_xml_text(item))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Formats a targeted prompt for executing a specific leaf step of a plan in isolation.
///
/// Excludes unrelated chat history and focuses the model entirely on completing
/// the designated step, while providing architectural plan context and completed prerequisites.
pub fn format_plan_step_execution_prompt(
    plan: &Plan,
    step_id: &str,
    marker_suffix: &str,
) -> Result<String, String> {
    let step = plan
        .find_item(step_id)
        .ok_or_else(|| format!("Step '{}' not found in plan '{}'", step_id, plan.id))?;

    if !step.is_leaf() {
        return Err(format!(
            "Step '{}' ({}) is not a leaf task; execute individual sub-items instead",
            step.id, step.title
        ));
    }

    let mut out = String::new();
    let clean_title = escape_xml_text(&single_line(&step.title));
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        PLAN_STEP_EXECUTION_PREFIX,
        single_line(&plan.id),
        single_line(&step.id),
        clean_title
    ));

    out.push_str(&format!(
        "## Plan: {}\n",
        escape_xml_text(&single_line(&plan.title))
    ));
    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", escape_xml_text(trimmed)));
        }
    }

    if !plan.concept.is_empty() {
        out.push_str("### Architectural Concept (System Invariants & Focus):\n");
        fn render_concept(out: &mut String, items: &[tauqe_protocol::ConceptItem], depth: usize) {
            let indent = "  ".repeat(depth);
            for item in items {
                out.push_str(&format!(
                    "{}- #{} {}",
                    indent,
                    escape_xml_text(&item.id),
                    escape_xml_text(&item.title)
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                    }
                }
                out.push('\n');
                render_concept(out, &item.children, depth + 1);
            }
        }
        render_concept(&mut out, &plan.concept, 0);
        out.push('\n');
    }

    let mut completed_items = Vec::new();
    fn collect_completed<'a>(items: &'a [PlanItem], acc: &mut Vec<(&'a str, &'a str)>) {
        for item in items {
            if item.status == PlanItemStatus::Done {
                acc.push((&item.id, &item.title));
            }
            collect_completed(&item.children, acc);
        }
    }
    collect_completed(&plan.items, &mut completed_items);

    if !completed_items.is_empty() {
        out.push_str("### Already Completed Prerequisites:\n");
        for (id, title) in completed_items {
            out.push_str(&format!(
                "- [x] #{} {}\n",
                escape_xml_text(id),
                escape_xml_text(title)
            ));
        }
        out.push('\n');
    }

    out.push_str("### Target Step to Implement:\n");
    out.push_str(&format!("- **ID**: `{}`\n", escape_xml_text(&step.id)));
    out.push_str(&format!(
        "- **Title**: {}\n",
        escape_xml_text(&step.title)
    ));
    if !step.concept_refs.is_empty() {
        out.push_str(&format!(
            "- **Addressed Concept Nodes**: {}\n",
            escape_list(&step.concept_refs)
        ));
    }
    if !step.files.is_empty() {
        out.push_str(&format!(
            "- **Target Files (to modify)**: {}\n",
            escape_list(&step.files)
        ));
    }
    if !step.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Reference Files (read-only context)**: {}\n",
            escape_list(&step.readonly_files)
        ));
    }
    if !step.depends_on.is_empty() {
        out.push_str(&format!(
            "- **Prerequisite Steps (must be DONE)**: {}\n",
            escape_list(&step.depends_on)
        ));
    }
    if let Some(details) = &step.details {
        let trimmed = details.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "- **Details**:\n{}\n",
                escape_xml_text(trimmed)
            ));
        }
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on implementing step `{}` while upholding the Architectural Concept invariants.\n",
        escape_xml_text(&step.id)
    ));
    out.push_str("2. Do NOT implement any upcoming or subsequent steps from the plan.\n");
    out.push_str("3. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "5. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Request all necessary files together in a single turn to avoid exhausting discovery rounds. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "6. If you encounter an insurmountable technical blocker, contradictory requirements, or need developer decisions, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
        marker_suffix
    ));

    Ok(out)
}

/// Builds a corrective reprompt for a plan step turn that ended without valid
/// code edits and without a terminal success tag.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback, then explicitly demands strict adherence to the active edit
/// protocol on the next turn.
pub fn format_plan_step_retry_prompt(assistant_text: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(PLAN_STEP_RETRY_PREFIX);
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
    out.push_str("1. If the step requires code changes, emit a valid edit block that strictly conforms to the active edit protocol.\n");
    out.push_str("2. If no code changes are required to complete the step, confirm completion explicitly by emitting the terminal success tag.\n");
    out.push_str("3. Do NOT replace protocol operations with conversational text, and do NOT end the turn without either a valid edit block or the terminal success tag.\n");

    out
}
