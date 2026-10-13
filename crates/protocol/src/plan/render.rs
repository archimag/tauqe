use super::item::{ConceptItem, PlanItem, PlanItemStatus};
use super::node::Plan;

impl Plan {
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# Plan: {}\n", self.title);
        if let Some(desc) = &self.description {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                out.push('\n');
                out.push_str(trimmed);
                out.push('\n');
            }
        }

        if !self.concept.is_empty() {
            out.push_str("\n## Concept\n\n");
            fn write_concept(items: &[ConceptItem], depth: usize, out: &mut String) {
                for item in items {
                    let indent = "  ".repeat(depth);
                    let clean_title = item.title.replace(['\r', '\n'], " ").trim().to_string();
                    out.push_str(&format!("{}- #{}: {}\n", indent, item.id, clean_title));
                    if let Some(details) = &item.details {
                        let trimmed = details.trim();
                        if !trimmed.is_empty() {
                            for line in trimmed.lines() {
                                out.push_str(&format!("{}    {}\n", indent, line));
                            }
                        }
                    }
                    write_concept(&item.children, depth + 1, out);
                }
            }
            write_concept(&self.concept, 0, &mut out);
        }

        if !self.concept.is_empty() && !self.items.is_empty() {
            out.push_str("\n## Implementation\n");
        }
        out.push('\n');

        fn write_items(items: &[PlanItem], depth: usize, out: &mut String) {
            for item in items {
                let indent = "  ".repeat(depth);
                let check = match item.status {
                    PlanItemStatus::Done => "[x]",
                    PlanItemStatus::InProgress => "[>]",
                    PlanItemStatus::Blocked => "[!]",
                    PlanItemStatus::Cancelled => "[-]",
                    PlanItemStatus::Discussion => "[?]",
                    PlanItemStatus::Todo => "[ ]",
                };
                let clean_title = item.title.replace(['\r', '\n'], " ").trim().to_string();
                out.push_str(&format!(
                    "{}- {} {} #{}: {}\n",
                    indent, check, item.status, item.id, clean_title
                ));
                if !item.concept_refs.is_empty() {
                    out.push_str(&format!("{}    ref: {}\n", indent, item.concept_refs.join(", ")));
                }
                if !item.files.is_empty() {
                    out.push_str(&format!("{}    files: {}\n", indent, item.files.join(", ")));
                }
                if !item.readonly_files.is_empty() {
                    out.push_str(&format!("{}    readonly_files: {}\n", indent, item.readonly_files.join(", ")));
                }
                if !item.depends_on.is_empty() {
                    out.push_str(&format!("{}    depends_on: {}\n", indent, item.depends_on.join(", ")));
                }
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        for line in trimmed.lines() {
                            out.push_str(&format!("{}    {}\n", indent, line));
                        }
                    }
                }
                write_items(&item.children, depth + 1, out);
            }
        }

        write_items(&self.items, 0, &mut out);
        out
    }
}
