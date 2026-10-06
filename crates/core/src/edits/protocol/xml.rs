use workbench_protocol::{EditOperation, ModelResult};

use super::utils::{normalize_content, resolve_target_path};
use super::EditProtocol;

/// XML-based code edit protocol
#[derive(Debug, Default, Clone)]
pub struct XmlEditProtocol;

impl EditProtocol for XmlEditProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (XML Edits)\n");
        prompt.push_str("When proposing changes, use structured XML blocks:\n");
        prompt.push_str("- Existing files: modify ONLY files listed in <editable_files> via <edit path=\"...\">.\n");
        prompt.push_str("- New files: create new files using <create path=\"...\"> with a valid repository-relative path. Created files are automatically added to context.\n");
        prompt.push_str("- File deletions: delete obsolete files using <delete path=\"...\" />.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No existing files are currently marked as editable. You may answer questions, explain code, or create new files with <create path=\"...\"> if requested.\n\n");
        } else {
            prompt.push_str("Currently permitted existing target files for modification:\n");
            for path in editable_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push_str("\n");
        }

        let sample_path = editable_paths
            .first()
            .map(|s| s.as_str())
            .unwrap_or("example_file.rs");

        prompt.push_str("Format for proposing changes:\n");
        prompt.push_str("<workbench_edits summary=\"Concise commit message in imperative mood, e.g. Add validation helper and update context manager\">\n");
        prompt.push_str("  <!-- To modify an existing file: -->\n");
        prompt.push_str(&format!("  <edit path=\"{}\">\n", sample_path));
        prompt.push_str("    <search>\n");
        prompt.push_str("exact lines from target file to be replaced\n");
        prompt.push_str("    </search>\n");
        prompt.push_str("    <replace>\n");
        prompt.push_str("new replacement lines\n");
        prompt.push_str("    </replace>\n");
        prompt.push_str("  </edit>\n\n");
        prompt.push_str("  <!-- To create a new file: -->\n");
        prompt.push_str("  <create path=\"path/to/new_file.rs\">\n");
        prompt.push_str("// Complete file content\n");
        prompt.push_str("  </create>\n\n");
        prompt.push_str("  <!-- To delete a file: -->\n");
        prompt.push_str("  <delete path=\"path/to/file_to_delete.rs\" />\n");
        prompt.push_str("</workbench_edits>\n\n");

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Specify 'summary=\"...\"' on <workbench_edits> with a high-quality Git commit message.\n");
        prompt.push_str("2. For <edit>, the 'path' attribute MUST EXACTLY match one of the paths listed in <editable_files>.\n");
        prompt.push_str("3. For <create>, specify a valid relative path within the repository.\n");
        prompt.push_str("4. The <search> block must match EXACTLY ONE location in the target file, including whitespace and line breaks.\n");
        prompt.push_str("5. Keep <search> blocks as concise as possible while maintaining uniqueness.\n");
        prompt.push_str("6. You may include multiple <edit>, <create>, or <delete> blocks inside <workbench_edits>.\n");
        prompt.push_str("7. If no code changes are needed (e.g. conversational answer or explanation), output plain text without any XML edit tags.\n\n");

        prompt
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        // Fallback check for workbench_edit JSON block
        if let Some(json_res) = crate::edits::parse_workbench_edit_json(raw_text) {
            return json_res;
        }

        if !has_xml_edit_tags(raw_text) {
            return ModelResult::Answer {
                text: raw_text.to_string(),
            };
        }

        let extracted_summary = extract_summary_from_xml(raw_text);

        match parse_xml_edits(raw_text, editable_paths) {
            Ok(edits) => {
                if edits.is_empty() {
                    ModelResult::Answer {
                        text: raw_text.to_string(),
                    }
                } else {
                    let summary = extracted_summary
                        .unwrap_or_else(|| "Apply AI code changes".to_string());
                    ModelResult::Edit {
                        summary,
                        edits,
                        proposal: None,
                        applied: false,
                        error: None,
                        changed_files: Vec::new(),
                        commit_hash: None,
                    }
                }
            }
            Err(err_msg) => ModelResult::Edit {
                summary: "Malformed XML edit block".to_string(),
                edits: Vec::new(),
                proposal: None,
                applied: false,
                error: Some(err_msg),
                changed_files: Vec::new(),
                commit_hash: None,
            },
        }
    }
}

pub fn has_xml_edit_tags(text: &str) -> bool {
    text.contains("<edit")
        || text.contains("<replace")
        || text.contains("<create")
        || text.contains("<delete")
        || text.contains("<workbench_edits")
}

fn extract_summary_from_xml(text: &str) -> Option<String> {
    // 1. Try finding attribute summary="..." in <workbench_edits ...>
    if let Some(idx) = text.find("<workbench_edits") {
        if let Some(end) = text[idx..].find('>') {
            let header = &text[idx..idx + end];
            if let Some(s) = extract_attribute(header, "summary") {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
        }
    }

    // 2. Try finding <summary>...</summary> tag
    if let Some((content, _)) = find_tag(text, "summary") {
        let clean = content.trim();
        if !clean.is_empty() {
            return Some(clean.to_string());
        }
    }

    None
}

fn parse_xml_edits(text: &str, editable_paths: &[String]) -> Result<Vec<EditOperation>, String> {
    let mut edits = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let remaining = &text[pos..];

        let next_tag = find_next_edit_tag(remaining);
        let (tag_type, offset) = match next_tag {
            Some(res) => res,
            None => break,
        };

        let tag_start = pos + offset;
        let tag_slice = &text[tag_start..];

        match tag_type {
            TagType::Edit | TagType::Replace => {
                let open_tag_end = tag_slice.find('>').ok_or_else(|| {
                    format!("Unterminated opening tag at offset {}", tag_start)
                })?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in tag: {}", header)
                })?;
                let target_path = resolve_target_path(&path_attr, editable_paths);

                let closing_tag = if tag_type == TagType::Edit {
                    "</edit>"
                } else {
                    "</replace>"
                };

                let close_idx = tag_slice.find(closing_tag).ok_or_else(|| {
                    format!("Missing closing tag '{}' for file '{}'", closing_tag, target_path)
                })?;

                let inner = &tag_slice[open_tag_end + 1..close_idx];

                let mut inner_pos = 0;
                let mut found_any = false;

                while let Some((search_content, after_search)) = find_tag(&inner[inner_pos..], "search") {
                    let search_abs_end = inner_pos + after_search;
                    let remaining_inner = &inner[search_abs_end..];

                    let (replace_content, after_replace) = if let Some(res) = find_tag(remaining_inner, "replace") {
                        res
                    } else if let Some(res) = find_tag(remaining_inner, "with") {
                        res
                    } else {
                        return Err(format!(
                            "Missing <replace> or <with> tag after <search> in file '{}'",
                            target_path
                        ));
                    };

                    edits.push(EditOperation::Replace {
                        path: target_path.clone(),
                        old_text: normalize_content(search_content),
                        new_text: normalize_content(replace_content),
                    });

                    found_any = true;
                    inner_pos = search_abs_end + after_replace;
                }

                if !found_any {
                    return Err(format!(
                        "Tag for file '{}' must contain <search> and <replace> blocks",
                        target_path
                    ));
                }

                pos = tag_start + close_idx + closing_tag.len();
            }
            TagType::Create => {
                let open_tag_end = tag_slice.find('>').ok_or_else(|| {
                    format!("Unterminated <create> tag at offset {}", tag_start)
                })?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <create> tag: {}", header)
                })?;
                let target_path = resolve_target_path(&path_attr, editable_paths);

                let close_idx = tag_slice.find("</create>").ok_or_else(|| {
                    format!("Missing </create> tag for file '{}'", target_path)
                })?;

                let content = &tag_slice[open_tag_end + 1..close_idx];
                edits.push(EditOperation::Create {
                    path: target_path,
                    content: normalize_content(content),
                });

                pos = tag_start + close_idx + "</create>".len();
            }
            TagType::Delete => {
                let open_tag_end = tag_slice.find('>').ok_or_else(|| {
                    format!("Unterminated <delete> tag at offset {}", tag_start)
                })?;
                let header = &tag_slice[..open_tag_end];
                let path_attr = extract_attribute(header, "path").ok_or_else(|| {
                    format!("Missing 'path' attribute in <delete> tag: {}", header)
                })?;
                let target_path = resolve_target_path(&path_attr, editable_paths);

                if header.ends_with('/') {
                    pos = tag_start + open_tag_end + 1;
                } else if let Some(close_idx) = tag_slice.find("</delete>") {
                    pos = tag_start + close_idx + "</delete>".len();
                } else {
                    pos = tag_start + open_tag_end + 1;
                }

                edits.push(EditOperation::Delete { path: target_path });
            }
        }
    }

    Ok(edits)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TagType {
    Edit,
    Replace,
    Create,
    Delete,
}

fn find_next_edit_tag(text: &str) -> Option<(TagType, usize)> {
    let mut candidates = Vec::new();

    if let Some(idx) = find_tag_start(text, "edit") {
        candidates.push((TagType::Edit, idx));
    }
    if let Some(idx) = find_tag_start(text, "replace") {
        if let Some(end_bracket) = text[idx..].find('>') {
            let header = &text[idx..idx + end_bracket];
            if header.contains("path") {
                candidates.push((TagType::Replace, idx));
            }
        }
    }
    if let Some(idx) = find_tag_start(text, "create") {
        candidates.push((TagType::Create, idx));
    }
    if let Some(idx) = find_tag_start(text, "delete") {
        candidates.push((TagType::Delete, idx));
    }

    candidates.into_iter().min_by_key(|(_, idx)| *idx)
}

fn find_tag_start(text: &str, tag_name: &str) -> Option<usize> {
    let prefix = format!("<{}", tag_name);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&prefix) {
        let abs_idx = search_from + idx;
        let after = &text[abs_idx + prefix.len()..];
        if let Some(c) = after.chars().next() {
            if c.is_whitespace() || c == '>' {
                return Some(abs_idx);
            }
        }
        search_from = abs_idx + prefix.len();
    }
    None
}

fn find_tag<'a>(text: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    let open_prefix = format!("<{}", tag);
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(&open_prefix) {
        let abs_start = search_from + idx;
        let after = &text[abs_start + open_prefix.len()..];
        if let Some(c) = after.chars().next() {
            if c == '>' || c.is_whitespace() {
                if let Some(close_bracket) = after.find('>') {
                    let content_start = abs_start + open_prefix.len() + close_bracket + 1;
                    let close_tag = format!("</{}>", tag);
                    if let Some(end_idx) = text[content_start..].find(&close_tag) {
                        let content = &text[content_start..content_start + end_idx];
                        let next_pos = content_start + end_idx + close_tag.len();
                        return Some((content, next_pos));
                    }
                }
            }
        }
        search_from = abs_start + open_prefix.len();
    }
    None
}

fn extract_attribute(tag_header: &str, attr: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(pos) = tag_header[search_from..].find(attr) {
        let abs_pos = search_from + pos;
        let before_ok = abs_pos == 0
            || tag_header[..abs_pos]
                .chars()
                .last()
                .map(|c| c.is_whitespace() || c == '<')
                .unwrap_or(false);
        let after_attr = &tag_header[abs_pos + attr.len()..];
        let trimmed = after_attr.trim_start();
        if before_ok && trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
            if after_eq.is_empty() {
                return None;
            }
            let quote_char = after_eq.chars().next()?;
            if quote_char == '"' || quote_char == '\'' {
                let rest = &after_eq[quote_char.len_utf8()..];
                let end_quote = rest.find(quote_char)?;
                let val = &rest[..end_quote];
                return Some(val.trim().trim_matches('`').to_string());
            } else {
                let val: String = after_eq
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
                    .collect();
                if !val.is_empty() {
                    return Some(val.trim_matches('`').to_string());
                }
            }
        }
        search_from = abs_pos + attr.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_protocol_edit_with_summary_attribute() {
        let proto = XmlEditProtocol;
        let editable = vec!["crates/core/src/lib.rs".to_string()];

        let output = r#"I have updated the code:

<workbench_edits summary="Refactor core initialization">
  <edit path="crates/core/src/lib.rs">
    <search>
pub fn old_fn() {}
    </search>
    <replace>
pub fn new_fn() {}
    </replace>
  </edit>
</workbench_edits>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Refactor core initialization");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_xml_protocol_create_new_file() {
        let proto = XmlEditProtocol;
        let editable = vec![];

        let output = r#"I will create a helper module:

<workbench_edits summary="Add helper module">
  <create path="crates/core/src/helper.rs">
pub fn help() -> bool { true }
  </create>
</workbench_edits>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Add helper module");
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Create { path, content } => {
                        assert_eq!(path, "crates/core/src/helper.rs");
                        assert!(content.contains("pub fn help()"));
                    }
                    _ => panic!("Expected Create operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }
}
