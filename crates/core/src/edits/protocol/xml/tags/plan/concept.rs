//! Parsing of `<concept>` sections and their `<concept_item>` entries.

use super::extract_and_strip_inner_tag;
use crate::edits::protocol::xml::extract_attribute;
use crate::edits::protocol::xml::tags::strip_tags;

fn next_concept_item_bounds(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while from < text.len() {
        let next_c = text[from..].find("<concept_item");
        let next_i = text[from..].find("<item");
        let (rel, open_len) = match (next_c, next_i) {
            (Some(c), Some(i)) if c <= i => (c, "<concept_item".len()),
            (Some(_), Some(i)) => (i, "<item".len()),
            (Some(c), None) => (c, "<concept_item".len()),
            (None, Some(i)) => (i, "<item".len()),
            (None, None) => break,
        };

        let start = from + rel;
        let after = start + open_len;
        let bytes = text.as_bytes();
        let mut idx = after;
        while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
        }
        let rest = &text[idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            from = after;
            continue;
        }
        let gt = rest.find('>')?;
        let header_end = idx + gt + 1;
        if rest[..gt].trim_end().ends_with('/') {
            return Some((start, header_end));
        }

        let tag_name = &text[start + 1..idx];
        let open_needle = format!("<{tag_name}");
        let close_needle = format!("</{tag_name}");
        let mut depth = 1;
        let mut search_pos = header_end;
        while search_pos < text.len() {
            let next_open = text[search_pos..].find(&open_needle);
            let next_close = text[search_pos..].find(&close_needle);
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
                        let inside = &close_rest[close_needle.len()..close_gt];
                        if inside.trim().is_empty() {
                            depth -= 1;
                            if depth == 0 {
                                return Some((start, close_start + close_gt + 1));
                            }
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

pub(super) fn parse_concept_items_with_diagnostics(
    content: &str,
) -> (Vec<tauqe_protocol::ConceptItem>, Vec<String>) {
    let (items, errors) = parse_concept_items_inner(content);
    if items.is_empty() && !content.trim().is_empty() {
        let fallback_items = parse_concept_items_fallback(content);
        if !fallback_items.is_empty() {
            return (fallback_items, errors);
        }
    }
    (items, errors)
}

/// Parses `<concept_item>` entries within a `<concept>` section.
///
/// This is the recursion-safe core: it parses recognized tags and descends
/// into their bodies, but it never runs the unstructured-text fallback. The
/// fallback belongs to the section root only (see
/// [`parse_concept_items_with_diagnostics`]), so `<details>` prose inside a
/// recognized `<concept_item>` cannot spawn phantom child entries.
fn parse_concept_items_inner(content: &str) -> (Vec<tauqe_protocol::ConceptItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    let needle_concept = "<concept_item";
    let needle_item = "<item";

    while pos < content.len() {
        let next_c = content[pos..].find(needle_concept);
        let next_i = content[pos..].find(needle_item);
        let (rel, open_len) = match (next_c, next_i) {
            (Some(c), Some(i)) if c <= i => (c, needle_concept.len()),
            (Some(_), Some(i)) => (i, needle_item.len()),
            (Some(c), None) => (c, needle_concept.len()),
            (None, Some(i)) => (i, needle_item.len()),
            (None, None) => break,
        };

        let start = pos + rel;
        let after = start + open_len;
        let bytes = &content.as_bytes()[after..];
        let mut idx = 0;
        while idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-' || bytes[idx].is_ascii_alphanumeric()) {
            idx += 1;
        }
        let rest = &content[after + idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push("Malformed concept item tag: unclosed opening tag".to_string());
            break;
        };
        let header_end = after + idx + gt + 1;
        let header = &content[start..header_end];
        let id = extract_attribute(header, "id").unwrap_or_default().trim().to_string();
        let raw_title = extract_attribute(header, "title").unwrap_or_default().trim().to_string();
        let attr_details = extract_attribute(header, "details")
            .or_else(|| extract_attribute(header, "description"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if rest[..gt].trim_end().ends_with('/') {
            if !id.is_empty() || !raw_title.is_empty() {
                items.push(tauqe_protocol::ConceptItem {
                    id: if id.is_empty() { format!("c-{}", items.len() + 1) } else { id },
                    title: raw_title,
                    details: attr_details,
                    children: Vec::new(),
                });
            }
            pos = header_end;
        } else {
            let tag_name = &content[start + 1..after + idx];
            let open_needle = format!("<{tag_name}");
            let close_needle = format!("</{tag_name}");
            let mut depth = 1;
            let mut search_pos = header_end;
            let mut close_start = None;
            let mut close_end = None;

            while search_pos < content.len() {
                let next_open = content[search_pos..].find(&open_needle);
                let next_close = content[search_pos..].find(&close_needle);

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
                            let inside = &close_rest[close_needle.len()..cgt];
                            if inside.trim().is_empty() {
                                depth -= 1;
                                if depth == 0 {
                                    close_start = Some(close_tag_start);
                                    close_end = Some(close_tag_start + cgt + 1);
                                    break;
                                }
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
                let (children, child_errors) = parse_concept_items_inner(body);
                errors.extend(child_errors);
                let text_only = strip_tags(body, next_concept_item_bounds);

                let (inner_title, text_without_title) = extract_and_strip_inner_tag(&text_only, "title");
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
                    items.push(tauqe_protocol::ConceptItem {
                        id: if id.is_empty() { format!("c-{}", items.len() + 1) } else { id.clone() },
                        title: if title.is_empty() { id } else { title },
                        details,
                        children,
                    });
                }
                pos = cend_full;
            } else {
                errors.push(format!("Malformed concept item tag: missing closing tag </{tag_name}>"));
                pos = header_end;
            }
        }
    }

    (items, errors)
}

fn parse_fallback_item_header(text: &str, fallback_index: usize) -> (String, String, Option<String>) {
    let raw = text.trim();
    let mut words = raw.split_whitespace();
    let first_word = words.next().unwrap_or("");
    let clean_first = first_word.trim_matches(['[', ']', '(', ')', ':', ',']);

    let (id, remainder) = if clean_first.starts_with("#c-") || clean_first.starts_with("#c_") {
        let id_val = clean_first.trim_start_matches('#');
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        (id_val.to_string(), rem)
    } else if clean_first.starts_with("c-") || clean_first.starts_with("c_") {
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        (clean_first.to_string(), rem)
    } else if clean_first.starts_with('#')
        && clean_first.strip_prefix('#').unwrap_or("").chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && clean_first.len() > 1
    {
        let id_val = clean_first.trim_start_matches('#');
        let rem = raw.strip_prefix(first_word).unwrap_or(raw).trim().trim_start_matches(':').trim();
        let formatted_id = if id_val.starts_with("c-") {
            id_val.to_string()
        } else {
            format!("c-{}", id_val)
        };
        (formatted_id, rem)
    } else {
        (format!("c-{}", fallback_index), raw)
    };

    let (title, details) = if let Some((t, d)) = remainder.split_once(": ") {
        (t.trim(), Some(d.trim().to_string()))
    } else if let Some((t, d)) = remainder.split_once(" - ") {
        (t.trim(), Some(d.trim().to_string()))
    } else {
        (remainder.trim(), None)
    };

    let title_clean = title.trim_matches(['*', '_', '`']).trim();
    let final_title = if title_clean.is_empty() {
        id.clone()
    } else {
        title_clean.to_string()
    };

    (id, final_title, details.filter(|s| !s.is_empty()))
}

fn parse_concept_items_fallback(content: &str) -> Vec<tauqe_protocol::ConceptItem> {
    let mut items = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut current_item: Option<tauqe_protocol::ConceptItem> = None;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_new_item = if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            Some(rest.trim())
        } else if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            after_digits.strip_prefix(". ").map(|after_dot| after_dot.trim())
        } else if trimmed.starts_with("#c-")
            || trimmed.starts_with("c-")
            || trimmed.starts_with("#c_")
            || trimmed.starts_with("c_")
        {
            Some(trimmed)
        } else {
            None
        };

        if let Some(item_text) = is_new_item {
            if let Some(prev) = current_item.take() {
                items.push(prev);
            }

            let (id, title, inline_details) =
                parse_fallback_item_header(item_text, items.len() + 1);
            current_item = Some(tauqe_protocol::ConceptItem {
                id,
                title,
                details: inline_details,
                children: Vec::new(),
            });
        } else if let Some(ref mut cur) = current_item {
            if let Some(ref mut d) = cur.details {
                d.push('\n');
                d.push_str(trimmed);
            } else {
                cur.details = Some(trimmed.to_string());
            }
        }
    }

    if let Some(last) = current_item {
        items.push(last);
    }

    if items.is_empty() && !content.trim().is_empty() {
        let trimmed = content.trim();
        let (first_line, rest) = match trimmed.split_once('\n') {
            Some((f, r)) => (f.trim(), Some(r.trim())),
            None => (trimmed, None),
        };
        let title = if first_line.chars().count() > 80 {
            let truncated: String = first_line.chars().take(77).collect();
            format!("{}...", truncated)
        } else {
            first_line.to_string()
        };
        let details = match rest {
            Some(r) if !r.is_empty() => Some(r.to_string()),
            _ => {
                if first_line != title {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            }
        };
        items.push(tauqe_protocol::ConceptItem {
            id: "c-1".to_string(),
            title,
            details,
            children: Vec::new(),
        });
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_concept_item_with_details_has_no_phantom_children() {
        let content = r#"
<concept_item id="c-1" title="Token Security">
  <details>
    Authentication must use short-lived, rotating tokens.
    Refresh must never leak the signing key.
  </details>
</concept_item>
"#;
        let (items, errors) = parse_concept_items_with_diagnostics(content);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "c-1");
        assert!(
            items[0].children.is_empty(),
            "details prose must not spawn phantom children"
        );
    }

    #[test]
    fn fallback_runs_once_at_the_section_root() {
        let content = "- #c-1 Verification Symmetry\n  Preflight runs the same checks.";
        let (items, errors) = parse_concept_items_with_diagnostics(content);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "c-1");
        assert_eq!(items[0].title, "Verification Symmetry");
        assert_eq!(
            items[0].details.as_deref(),
            Some("Preflight runs the same checks.")
        );
    }

    #[test]
    fn nested_concept_items_are_parsed_without_fallback_children() {
        let content = r#"
<concept_item id="c-1" title="Parent">
  <details>Parent details stay attached to the parent.</details>
  <concept_item id="c-1.1" title="Child" />
</concept_item>
"#;
        let (items, errors) = parse_concept_items_with_diagnostics(content);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].id, "c-1.1");
        assert_eq!(
            items[0].details.as_deref(),
            Some("Parent details stay attached to the parent.")
        );
    }
}
