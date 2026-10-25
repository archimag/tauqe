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
    out.push_str(&format!("- **Title**: {}\n", escape_xml_text(&step.title)));
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
            out.push_str(&format!("- **Details**:\n{}\n", escape_xml_text(trimmed)));
        }
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on implementing step `{}` while upholding the Architectural Concept invariants.\n",
        escape_xml_text(&step.id)
    ));
    out.push_str("2. Own the implementation (`How`) only, never the requirement (`What`): the architecture of this step is fixed by the plan. Replacing the designed data structures, rewriting the interfaces or contracts the concept defines, substituting the selected components or libraries, or relaxing the given invariants is strictly prohibited.\n");
    out.push_str("3. Do NOT implement any upcoming or subsequent steps from the plan.\n");
    out.push_str("4. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
    out.push_str(&format!(
        "5. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "6. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Request all necessary files together in a single turn to avoid exhausting discovery rounds. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "7. If the step cannot be implemented within the approved architectural decision, because the implementation would require a different data structure, a changed interface, a substituted component, or a relaxed concept invariant, do NOT improvise an alternative design. Stop and emit <blocked{0} reason=\"...\" details=\"...\" /> describing the exact architectural conflict, so the harness starts a controlled replanning cycle.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "8. If you encounter an insurmountable technical blocker that is unrelated to the architectural decision, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
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

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ConceptItem;

    /// Builds a plan whose single leaf step addresses one concept invariant.
    fn leaf_plan() -> Plan {
        Plan {
            id: "P-1".to_string(),
            title: "Harden the scanner".to_string(),
            description: Some("Introduce a bounded emit window.".to_string()),
            concept: vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Decision Immutability".to_string(),
                details: Some("The step owns only how the decision is implemented.".to_string()),
                children: Vec::new(),
            }],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Bound the emit length".to_string(),
                details: Some("Reuse the existing scanner.".to_string()),
                status: PlanItemStatus::Todo,
                files: vec!["crates/core/src/scan.rs".to_string()],
                readonly_files: vec!["crates/core/src/api.rs".to_string()],
                concept_refs: vec!["c-1".to_string()],
                depends_on: Vec::new(),
                children: Vec::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn execution_prompt_renders_the_architectural_concept() {
        let prompt = format_plan_step_execution_prompt(&leaf_plan(), "1", "").expect("prompt");

        assert!(prompt.contains("### Architectural Concept (System Invariants & Focus):"));
        assert!(prompt.contains("Decision Immutability"));
        assert!(prompt.contains("The step owns only how the decision is implemented."));
        assert!(prompt.contains("Introduce a bounded emit window."));
        assert!(prompt.contains("Reuse the existing scanner."));
        assert!(prompt.contains("**Addressed Concept Nodes**: c-1"));
        assert!(prompt.contains("**Reference Files (read-only context)**: crates/core/src/api.rs"));
    }

    #[test]
    fn execution_prompt_forbids_architectural_drift() {
        let prompt = format_plan_step_execution_prompt(&leaf_plan(), "1", "").expect("prompt");

        // The directive owns the implementation only and refuses every
        // substitution of the decision the plan already fixed.
        assert!(prompt.contains("never the requirement (`What`)"));
        assert!(prompt.contains("Replacing the designed data structures"));
        assert!(prompt.contains("substituting the selected components or libraries"));
        assert!(prompt.contains("do NOT improvise an alternative design"));
    }

    #[test]
    fn execution_prompt_routes_control_tags_through_the_marker() {
        let prompt = format_plan_step_execution_prompt(&leaf_plan(), "1", "_M7").expect("prompt");

        for tag in [
            "<success_M7 summary=",
            "<blocked_M7 reason=",
            "<error_M7 message=",
            "<context_request_M7 path=",
        ] {
            assert!(prompt.contains(tag), "missing {tag}");
        }
        assert!(!prompt.contains("<success summary="));
    }

    #[test]
    fn execution_prompt_rejects_a_container_and_an_unknown_target() {
        let mut plan = leaf_plan();
        // A container is a conceptual milestone and never an execution target.
        let step = plan.items.first_mut().expect("the single step");
        step.children.push(PlanItem {
            id: "1.1".to_string(),
            title: "Nested implementation step".to_string(),
            ..Default::default()
        });

        assert!(format_plan_step_execution_prompt(&plan, "1", "").is_err());
        assert!(format_plan_step_execution_prompt(&plan, "99", "").is_err());
    }

    #[test]
    fn retry_prompt_demands_a_protocol_operation() {
        let prompt = format_plan_step_retry_prompt("finished <edit> nothing", Some("no edits"));

        assert!(prompt.starts_with(PLAN_STEP_RETRY_PREFIX));
        assert!(prompt.contains("&lt;edit&gt;"));
        assert!(prompt.contains("no edits"));
        assert!(prompt.contains("terminal success tag"));
    }
}
