//! Shared, factual rendering of a review group's saved remediation horizon.

use crate::escape::escape_xml_text;
use crate::plan::{PlanItem, PlanItemStatus};
use crate::review::ReviewGroup;

/// Render the group's blocker, recorded decision, and complete saved horizon.
/// Flow uses this same diagnostic projection when reporting why a group needs refinement.
pub fn render_group_implementation(group: &ReviewGroup) -> String {
    render_group_implementation_with_focus(group, &[])
}

pub(super) fn render_group_implementation_with_focus(
    group: &ReviewGroup,
    focused_steps: &[String],
) -> String {
    let mut out = String::from("## Saved Group Implementation and Diagnostics:\n");
    match group.blocked_reason.as_deref() {
        Some(reason) if !reason.trim().is_empty() => out.push_str(&format!(
            "- **Group Blocker**: {}\n",
            escape_xml_text(reason.trim())
        )),
        _ => out.push_str("- **Group Blocker**: none recorded for the group (individual steps may still be blocked)\n"),
    }
    match group.resolution_notes.as_deref() {
        Some(note) if !note.trim().is_empty() => out.push_str(&format!(
            "- **Recorded Group Decision**: {}\n",
            escape_xml_text(note.trim())
        )),
        _ => out.push_str("- **Recorded Group Decision**: none recorded\n"),
    }
    out.push_str("### Saved Horizon (in stored order):\n");
    if group.implementation.items.is_empty() {
        out.push_str("No implementation steps saved.\n");
    } else {
        render_items(&mut out, &group.implementation.items, 0, focused_steps);
    }
    out.push('\n');
    out
}

fn render_items(out: &mut String, items: &[PlanItem], depth: usize, focused_steps: &[String]) {
    for item in items {
        let indent = "  ".repeat(depth);
        out.push_str(&format!(
            "{indent}- **Step `#{}`**: {} (status: {}){}\n",
            escape_xml_text(&item.id),
            escape_xml_text(&item.title),
            item.status,
            if focused_steps.contains(&item.id) {
                " [FOCUSED]"
            } else {
                ""
            }
        ));
        if let Some(details) = &item.details {
            out.push_str(&format!(
                "{indent}  Details: {}\n",
                escape_xml_text(details)
            ));
        }
        for (label, values) in [
            ("Depends on", &item.depends_on),
            ("Files", &item.files),
            ("Readonly files", &item.readonly_files),
            ("Outer docs", &item.outer_docs),
            ("Concept refs", &item.concept_refs),
        ] {
            if !values.is_empty() {
                let escaped = values
                    .iter()
                    .map(|value| format!("`{}`", escape_xml_text(value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{indent}  {label}: {escaped}\n"));
            }
        }
        if let Some(commit) = &item.commit {
            out.push_str(&format!("{indent}  Commit: {}\n", escape_xml_text(commit)));
        }
        if item.status == PlanItemStatus::Blocked {
            match item.blocked_reason.as_deref() {
                Some(reason) if !reason.trim().is_empty() => out.push_str(&format!(
                    "{indent}  Step Blocker: {}\n",
                    escape_xml_text(reason.trim())
                )),
                _ => out.push_str(&format!(
                    "{indent}  Step Blocker: no saved reason for this blocked step\n"
                )),
            }
        } else if let Some(reason) = &item.blocked_reason {
            // Display stored data as-is even if an older document carries a
            // stale reason on a step whose status is no longer Blocked.
            out.push_str(&format!(
                "{indent}  Saved blocked_reason: {}\n",
                escape_xml_text(reason)
            ));
        }
        render_items(out, &item.children, depth + 1, focused_steps);
    }
}
