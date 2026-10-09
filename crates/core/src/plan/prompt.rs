use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

/// Formats the plan and its items into a prompt block for LLM context.
pub fn format_active_plan_context(plan: &Plan) -> Option<String> {
    if plan.items.is_empty() {
        return None;
    }

    let (total, completed) = plan.count_stats();
    let mut out = String::new();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        plan.id, plan.title, completed, total
    ));

    if let Some(desc) = &plan.description {
        out.push_str(&format!("  <description>{}</description>\n", desc.trim()));
    }

    fn render_items(out: &mut String, items: &[PlanItem], depth: usize) {
        let indent = "  ".repeat(depth + 1);
        for item in items {
            let status_mark = match item.status {
                PlanItemStatus::Discussion => "DISCUSSION",
                PlanItemStatus::Todo => "TODO",
                PlanItemStatus::InProgress => "IN_PROGRESS",
                PlanItemStatus::Done => "DONE",
                PlanItemStatus::Cancelled => "CANCELLED",
            };

            out.push_str(&format!(
                "{}- {} (id: \"{}\", status: {})",
                indent, item.title, item.id, status_mark
            ));

            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", trimmed));
                }
            }
            out.push('\n');

            if !item.children.is_empty() {
                render_items(out, &item.children, depth + 1);
            }
        }
    }

    render_items(&mut out, &plan.items, 0);
    out.push_str("</active_plan>");

    Some(out)
}

pub const PLAN_STEP_EXECUTION_PREFIX: &str = "# Target Plan Step Execution:";

pub use tauqe_protocol::plan::{
    format_plan_discussion_prompt, is_plan_discussion_prompt, PLAN_DISCUSSION_PREFIX,
};

/// Target plan and step identifiers parsed from an isolated plan step execution prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStepTarget {
    pub plan_id: Option<String>,
    pub step_id: String,
}

/// Returns true if the prompt represents an isolated plan step execution turn.
pub fn is_plan_step_execution_prompt(prompt: &str) -> bool {
    prompt.starts_with(PLAN_STEP_EXECUTION_PREFIX)
}

/// Extracts the target plan ID and step ID from an isolated plan step execution prompt.
pub fn extract_plan_step_execution_target(prompt: &str) -> Option<PlanStepTarget> {
    if !prompt.starts_with(PLAN_STEP_EXECUTION_PREFIX) {
        return None;
    }
    let rest = prompt.strip_prefix(PLAN_STEP_EXECUTION_PREFIX)?.trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some((plan_id, step_id)) = spec.split_once(':') {
            let plan_id = plan_id.trim();
            let step_id = step_id.trim();
            if !step_id.is_empty() {
                return Some(PlanStepTarget {
                    plan_id: if plan_id.is_empty() { None } else { Some(plan_id.to_string()) },
                    step_id: step_id.to_string(),
                });
            }
        } else {
            return Some(PlanStepTarget {
                plan_id: None,
                step_id: spec.to_string(),
            });
        }
    }
    None
}

/// Extracts the target step ID from an isolated plan step execution prompt.
pub fn extract_plan_step_execution_id(prompt: &str) -> Option<String> {
    extract_plan_step_execution_target(prompt).map(|t| t.step_id)
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
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        PLAN_STEP_EXECUTION_PREFIX, plan.id, step.id, step.title
    ));

    out.push_str(&format!("## Plan: {}\n", plan.title));
    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", trimmed));
        }
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
            out.push_str(&format!("- [x] #{} {}\n", id, title));
        }
        out.push('\n');
    }

    out.push_str("### Target Step to Implement:\n");
    out.push_str(&format!("- **ID**: `{}`\n", step.id));
    out.push_str(&format!("- **Title**: {}\n", step.title));
    if let Some(details) = &step.details {
        let trimmed = details.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Details**:\n{}\n", trimmed));
        }
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on implementing step `{}`.\n",
        step.id
    ));
    out.push_str("2. Do NOT implement any upcoming or subsequent steps from the plan.\n");
    out.push_str("3. Keep changes minimal, coherent, and verified with toolchain checks.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <plan_step_done{0} id=\"{1}\" />.\n",
        marker_suffix, step.id
    ));
    out.push_str(&format!(
        "5. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Request all necessary files together in a single turn to avoid exhausting discovery rounds. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));
    out.push_str("6. If you encounter an unsolvable blocking issue or need developer architectural decisions, explain it in text and DO NOT emit the completion tag.\n");

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_active_plan_context() {
        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Implement secure tokens".to_string()),
            created_at: 100,
            updated_at: 200,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: Some("Claims struct".to_string()),
                    status: PlanItemStatus::Done,
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    children: vec![],
                },
            ],
        };

        let formatted = format_active_plan_context(&plan).unwrap();
        assert!(formatted.contains("<active_plan id=\"jwt-auth\""));
        assert!(formatted.contains("progress=\"1/2 completed\""));
        assert!(formatted.contains("Endpoint (id: \"2\", status: IN_PROGRESS): Write handler"));
    }

    #[test]
    fn test_format_plan_step_execution_prompt() {
        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Implement secure tokens".to_string()),
            created_at: 100,
            updated_at: 200,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: Some("Claims struct".to_string()),
                    status: PlanItemStatus::Done,
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    children: vec![],
                },
            ],
        };

        let prompt = format_plan_step_execution_prompt(&plan, "2", "_XYZ").unwrap();
        assert!(prompt.contains("Target Plan Step Execution: [jwt-auth:2] Endpoint"));
        assert!(prompt.contains("Already Completed Prerequisites:"));

        let target = extract_plan_step_execution_target(&prompt).unwrap();
        assert_eq!(target.plan_id.as_deref(), Some("jwt-auth"));
        assert_eq!(target.step_id, "2");
        assert!(prompt.contains("- [x] #1 Models"));
        assert!(prompt.contains("- **ID**: `2`"));
        assert!(prompt.contains("<plan_step_done_XYZ id=\"2\" />"));
    }

    #[test]
    fn test_format_plan_discussion_prompt() {
        let plan = Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Implement secure tokens".to_string()),
            created_at: 100,
            updated_at: 200,
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: Some("Claims struct".to_string()),
                    status: PlanItemStatus::Done,
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    children: vec![],
                },
            ],
        };

        let prompt = format_plan_discussion_prompt(&plan, &["2".to_string()], "Should we use EdDSA?");
        assert!(prompt.contains("# Plan Discussion: [jwt-auth] JWT Authentication"));
        assert!(prompt.contains("Implement secure tokens"));
        assert!(prompt.contains("<active_plan id=\"jwt-auth\""));
        assert!(prompt.contains("Items Focused by User for Discussion:"));
        assert!(prompt.contains("- **#2 Endpoint**"));
        assert!(prompt.contains("Should we use EdDSA?"));
        assert!(prompt.contains("Instructions for Model:"));
    }
}
