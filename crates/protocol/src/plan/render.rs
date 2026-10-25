use super::item::{ConceptItem, PlanItem, PlanItemStatus};
use super::node::Plan;

/// Collapses a rendered value onto a single line.
///
/// Hand-edited or imported plan JSON can carry line breaks and surrounding
/// whitespace in any interpolated field: the plan title, a concept or step id,
/// a step title, or a metadata entry. A raw interpolation would split the
/// document — breaking the `# Plan:` heading into several headings and shifting
/// a `#id` marker off its line. Folding `\r` and `\n` into spaces and trimming
/// keeps exactly one heading line and one line per concept or step entry.
fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

/// Joins plan metadata entries into a single markdown line body.
///
/// Every metadata key must occupy exactly one indented line. Entries that come
/// from hand-edited or imported plan JSON may still carry surrounding
/// whitespace, embedded line breaks, or be empty, and a raw `join` would either
/// split the line or leave a dangling separator. Collapsing each entry onto a
/// single line and dropping empty ones keeps the rendered document
/// structurally stable.
fn inline_list(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| single_line(entry))
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<String>>()
        .join(", ")
}

impl Plan {
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# Plan: {}\n", single_line(&self.title));
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
                    let clean_title = single_line(&item.title);
                    out.push_str(&format!(
                        "{}- #{}: {}\n",
                        indent,
                        single_line(&item.id),
                        clean_title
                    ));
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
                let clean_title = single_line(&item.title);
                out.push_str(&format!(
                    "{}- {} {} #{}: {}\n",
                    indent,
                    check,
                    item.status,
                    single_line(&item.id),
                    clean_title
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
