//! Parsing of `<item>` entries within plan blocks.

use super::extract_and_strip_inner_tag;
use crate::edits::protocol::xml::extract_attribute;
use crate::edits::protocol::xml::tags::strip_tags;

pub(super) fn next_item_tag_bounds(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = text[from..].find("<item") {
        let start = from + rel;
        let after = start + "<item".len();
        let bytes = text.as_bytes();
        let mut idx = after;
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
        {
            idx += 1;
        }
        let rest = &text[idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            from = after;
            continue;
        }
        let gt = rest.find('>')?;
        let header_end = idx + gt + 1;
        if rest[..gt].trim_end().ends_with('/') {
            return Some((start, header_end));
        }
        let close_tag = "</item";
        let mut depth = 1;
        let mut search_pos = header_end;
        while search_pos < text.len() {
            let next_open = text[search_pos..].find("<item");
            let next_close = text[search_pos..].find(close_tag);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    let tag_header = &text[search_pos + o..];
                    if let Some(hgt) = tag_header.find('>') {
                        if !tag_header[..hgt].trim_end().ends_with('/') {
                            depth += 1;
                        }
                        search_pos += o + hgt + 1;
                    } else {
                        break;
                    }
                }
                (_, Some(c)) => {
                    let close_start = search_pos + c;
                    let close_rest = &text[close_start..];
                    if let Some(close_gt) = close_rest.find('>') {
                        depth -= 1;
                        if depth == 0 {
                            return Some((start, close_start + close_gt + 1));
                        }
                        search_pos = close_start + close_gt + 1;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        return Some((start, header_end));
    }
    None
}

/// Parsed representation of an `<item>` tag within a plan block.
///
/// Preserves whether `status` was explicitly provided or omitted, enabling
/// partial updates to retain existing item status without regressing to `Todo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlanItem {
    pub id: String,
    pub title: String,
    pub details: Option<String>,
    pub status: Option<tauqe_protocol::PlanItemStatus>,
    pub files: Vec<String>,
    pub readonly_files: Vec<String>,
    pub concept_refs: Vec<String>,
    pub depends_on: Vec<String>,
    pub children: Vec<ParsedPlanItem>,
}

impl ParsedPlanItem {
    /// Converts into a standard [`PlanItem`], defaulting omitted status to [`PlanItemStatus::Todo`].
    pub fn into_plan_item(self) -> tauqe_protocol::PlanItem {
        tauqe_protocol::PlanItem {
            id: self.id,
            title: self.title,
            details: self.details,
            status: self.status.unwrap_or(tauqe_protocol::PlanItemStatus::Todo),
            files: self.files,
            readonly_files: self.readonly_files,
            concept_refs: self.concept_refs,
            depends_on: self.depends_on,
            children: self
                .children
                .into_iter()
                .map(ParsedPlanItem::into_plan_item)
                .collect(),
        }
    }
}

impl From<tauqe_protocol::PlanItem> for ParsedPlanItem {
    fn from(item: tauqe_protocol::PlanItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            details: item.details,
            status: Some(item.status),
            files: item.files,
            readonly_files: item.readonly_files,
            concept_refs: item.concept_refs,
            depends_on: item.depends_on,
            children: item.children.into_iter().map(Into::into).collect(),
        }
    }
}

pub(super) fn parse_plan_items_with_diagnostics(
    content: &str,
) -> (Vec<ParsedPlanItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    while let Some(rel) = content[pos..].find("<item") {
        let start = pos + rel;
        let after = start + "<item".len();
        let bytes = &content.as_bytes()[after..];
        let mut idx = 0;
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
        {
            idx += 1;
        }
        let rest = &content[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push("Malformed <item> tag: unclosed opening tag".to_string());
            break;
        };
        let header_end = after + idx + gt + 1;
        let header = &content[start..header_end];
        let id = extract_attribute(header, "id")
            .unwrap_or_default()
            .trim()
            .to_string();
        let raw_title = extract_attribute(header, "title")
            .unwrap_or_default()
            .trim()
            .to_string();
        let status_str = extract_attribute(header, "status");
        let status = status_str.map(|s| match s.trim().to_lowercase().as_str() {
            "discussion" | "discuss" | "question" | "?" => {
                tauqe_protocol::PlanItemStatus::Discussion
            }
            "in_progress" | "inprogress" | "doing" => tauqe_protocol::PlanItemStatus::InProgress,
            "done" | "completed" | "finished" => tauqe_protocol::PlanItemStatus::Done,
            "blocked" | "block" => tauqe_protocol::PlanItemStatus::Blocked,
            "cancelled" | "canceled" => tauqe_protocol::PlanItemStatus::Cancelled,
            "todo" | "pending" | "" => tauqe_protocol::PlanItemStatus::Todo,
            other => {
                errors.push(format!(
                    "Unrecognized status '{other}' for item '{}', defaulting to 'todo'",
                    if id.is_empty() { &raw_title } else { &id }
                ));
                tauqe_protocol::PlanItemStatus::Todo
            }
        });

        let attr_details = extract_attribute(header, "details")
            .or_else(|| extract_attribute(header, "description"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let files = extract_attribute(header, "files")
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim().trim_start_matches("./").trim_start_matches('/');
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let readonly_files = extract_attribute(header, "readonly")
            .or_else(|| extract_attribute(header, "readonly_files"))
            .or_else(|| extract_attribute(header, "ref"))
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim().trim_start_matches("./").trim_start_matches('/');
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let concept_refs = extract_attribute(header, "concept_refs")
            .or_else(|| extract_attribute(header, "concept"))
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let depends_on = extract_attribute(header, "depends_on")
            .or_else(|| extract_attribute(header, "deps"))
            .map(|s| {
                s.split([',', ';', '\n'])
                    .filter_map(|p| {
                        let trimmed = p.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if rest[..gt].trim_end().ends_with('/') {
            // Self-closing item
            if !id.is_empty() || !raw_title.is_empty() {
                items.push(ParsedPlanItem {
                    id: if id.is_empty() {
                        format!("{}", items.len() + 1)
                    } else {
                        id.clone()
                    },
                    title: raw_title,
                    details: attr_details,
                    status,
                    files,
                    readonly_files,
                    concept_refs,
                    depends_on,
                    children: Vec::new(),
                });
            }
            pos = header_end;
        } else {
            // Paired tag: find matching </item> (supporting optional turn marker suffix)
            let mut depth = 1;
            let mut search_pos = header_end;
            let mut close_start = None;
            let mut close_end = None;

            while search_pos < content.len() {
                let next_open = content[search_pos..].find("<item");
                let next_close = content[search_pos..].find("</item");

                match (next_open, next_close) {
                    (Some(o), Some(c)) if o < c => {
                        let tag_header = &content[search_pos + o..];
                        if let Some(hgt) = tag_header.find('>') {
                            if !tag_header[..hgt].trim_end().ends_with('/') {
                                depth += 1;
                            }
                            search_pos += o + hgt + 1;
                        } else {
                            break;
                        }
                    }
                    (_, Some(c)) => {
                        let close_tag_start = search_pos + c;
                        let close_rest = &content[close_tag_start..];
                        if let Some(cgt) = close_rest.find('>') {
                            depth -= 1;
                            if depth == 0 {
                                close_start = Some(close_tag_start);
                                close_end = Some(close_tag_start + cgt + 1);
                                break;
                            }
                            search_pos = close_tag_start + cgt + 1;
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }

            if let (Some(cend), Some(cend_full)) = (close_start, close_end) {
                let body = &content[header_end..cend];
                let (children, child_errors) = parse_plan_items_with_diagnostics(body);
                errors.extend(child_errors);
                let text_only = strip_tags(body, next_item_tag_bounds);

                let (inner_title, text_without_title) =
                    extract_and_strip_inner_tag(&text_only, "title");
                let mut title = raw_title;
                if title.is_empty() {
                    if let Some(it) = inner_title {
                        title = it;
                    }
                }

                let (inner_details, text_without_details) = {
                    let (d, rem) = extract_and_strip_inner_tag(&text_without_title, "details");
                    if d.is_some() {
                        (d, rem)
                    } else {
                        extract_and_strip_inner_tag(&text_without_title, "description")
                    }
                };

                let mut details = inner_details.or(attr_details).or_else(|| {
                    let trimmed = text_without_details.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });

                if title.is_empty() {
                    if let Some(d) = details.take() {
                        if let Some((first, rest)) = d.split_once('\n') {
                            title = first.trim().to_string();
                            let rest_trimmed = rest.trim();
                            if !rest_trimmed.is_empty() {
                                details = Some(rest_trimmed.to_string());
                            }
                        } else {
                            title = d;
                        }
                    }
                }

                if !id.is_empty() || !title.is_empty() {
                    items.push(ParsedPlanItem {
                        id: if id.is_empty() {
                            format!("{}", items.len() + 1)
                        } else {
                            id.clone()
                        },
                        title: if title.is_empty() { id } else { title },
                        details,
                        status,
                        files: if children.is_empty() {
                            files
                        } else {
                            Vec::new()
                        },
                        readonly_files: if children.is_empty() {
                            readonly_files
                        } else {
                            Vec::new()
                        },
                        concept_refs,
                        depends_on: if children.is_empty() {
                            depends_on
                        } else {
                            Vec::new()
                        },
                        children,
                    });
                }
                pos = cend_full;
            } else {
                errors.push(format!(
                    "Malformed <item> tag: missing closing tag </item> for item '{}'",
                    if id.is_empty() { &raw_title } else { &id }
                ));
                pos = header_end;
            }
        }
    }

    (items, errors)
}
