use tauqe_protocol::{Plan, PlanItem, PlanItemStatus};

/// Formats the active plan and its focused (checked) items into a prompt block.
///
/// If any items are checked, it highlights them as targeted items for immediate work.
/// Otherwise, it provides the general plan structure so the model maintains context.
pub fn format_active_plan_context(plan: &Plan) -> Option<String> {
    if plan.items.is_empty() {
        return None;
    }

    let (total, completed, checked) = plan.count_stats();
    let mut out = String::new();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        plan.id, plan.title, completed, total
    ));

    if let Some(desc) = &plan.description {
        out.push_str(&format!("  <description>{}</description>\n", desc.trim()));
    }

    let has_checked = checked > 0;
    if has_checked {
        out.push_str("  <!-- The user has specifically focused the following checked items [x] -->\n");
    }

    fn render_items(out: &mut String, items: &[PlanItem], depth: usize, has_checked: bool) {
        let indent = "  ".repeat(depth + 1);
        for item in items {
            let check_mark = if item.checked { "[x]" } else { "[ ]" };
            let status_mark = match item.status {
                PlanItemStatus::Todo => "TODO",
                PlanItemStatus::InProgress => "IN_PROGRESS",
                PlanItemStatus::Done => "DONE",
                PlanItemStatus::Cancelled => "CANCELLED",
            };

            let should_render_details = !has_checked || item.checked;
            out.push_str(&format!(
                "{}- {} {} (id: \"{}\", status: {})",
                indent, check_mark, item.title, item.id, status_mark
            ));

            if should_render_details {
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
            }
            out.push('\n');

            if !item.children.is_empty() {
                render_items(out, &item.children, depth + 1, has_checked);
            }
        }
    }

    render_items(&mut out, &plan.items, 0, has_checked);
    out.push_str("</active_plan>");

    Some(out)
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
                    checked: false,
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    checked: true,
                    children: vec![],
                },
            ],
        };

        let formatted = format_active_plan_context(&plan).unwrap();
        assert!(formatted.contains("<active_plan id=\"jwt-auth\""));
        assert!(formatted.contains("progress=\"1/2 completed\""));
        assert!(formatted.contains("[x] Endpoint (id: \"2\", status: IN_PROGRESS): Write handler"));
    }
}
