use super::item::{ConceptItem, PlanItem, PlanItemStatus};
use super::node::Plan;

/// Joins plan metadata entries into a single markdown line body.
///
/// Every metadata key must occupy exactly one indented line. Entries that come
/// from hand-edited or imported plan JSON may still carry surrounding
/// whitespace, embedded line breaks, or be empty, and a raw `join` would either
/// split the line or leave a dangling separator. Trimming, collapsing line
/// breaks into spaces, and dropping empty entries keeps the rendered document
/// structurally stable.
fn inline_list(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| entry.replace(['\r', '\n'], " "))
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<String>>()
        .join(", ")
}

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
                // A key whose entries are all empty renders no line at all, so the
                // emptiness check runs on the normalized body, not the raw list.
                let refs = inline_list(&item.concept_refs);
                if !refs.is_empty() {
                    out.push_str(&format!("{indent}    ref: {refs}\n"));
                }
                let files = inline_list(&item.files);
                if !files.is_empty() {
                    out.push_str(&format!("{indent}    files: {files}\n"));
                }
                let readonly_files = inline_list(&item.readonly_files);
                if !readonly_files.is_empty() {
                    out.push_str(&format!("{indent}    readonly_files: {readonly_files}\n"));
                }
                let depends_on = inline_list(&item.depends_on);
                if !depends_on.is_empty() {
                    out.push_str(&format!("{indent}    depends_on: {depends_on}\n"));
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
