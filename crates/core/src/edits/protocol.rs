use workbench_protocol::{EditOperation, ModelResult};

pub trait EditProtocol: Send + Sync {
    /// Returns system prompt instructions tailored for this edit protocol.
    fn system_instructions(&self, editable_paths: &[String]) -> String;

    /// Parses the model text output into a ModelResult.
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult;
}

/// Backward compatibility shim for any external references
#[derive(Debug, Clone, Default)]
pub struct SearchReplaceMarkers;

/// Backward compatibility alias
pub type CustomSearchReplaceEditProtocol = XmlEditProtocol;

/// XML-based code edit protocol
#[derive(Debug, Default, Clone)]
pub struct XmlEditProtocol;

impl EditProtocol for XmlEditProtocol {
    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (XML Edits)\n");
        prompt.push_str("When proposing changes, modify ONLY files listed in <editable_files> using structured XML blocks.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No files are currently marked as editable. You must answer questions or explain code without proposing edits.\n\n");
        } else {
            prompt.push_str("Currently permitted target files for modification:\n");
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
        prompt.push_str("<workbench_edits>\n");
        prompt.push_str(&format!("  <!-- To modify an existing file: -->\n"));
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
        prompt.push_str("1. The 'path' attribute MUST EXACTLY match one of the paths listed in <editable_files>.\n");
        prompt.push_str("2. The <search> block must match EXACTLY ONE location in the target file, including whitespace and line breaks.\n");
        prompt.push_str("3. Keep <search> blocks as concise as possible while maintaining uniqueness.\n");
        prompt.push_str("4. You may include multiple <edit>, <create>, or <delete> blocks inside <workbench_edits>.\n");
        prompt.push_str("5. If no code changes are needed (e.g. conversational answer or explanation), output plain text without any XML edit tags.\n\n");

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

        match parse_xml_edits(raw_text, editable_paths) {
            Ok(edits) => {
                if edits.is_empty() {
                    ModelResult::Answer {
                        text: raw_text.to_string(),
                    }
                } else {
                    ModelResult::Edit {
                        summary: "Applied XML code edits".to_string(),
                        edits,
                        applied: false,
                        error: None,
                        changed_files: Vec::new(),
                    }
                }
            }
            Err(err_msg) => ModelResult::Edit {
                summary: "Malformed XML edit block".to_string(),
                edits: Vec::new(),
                applied: false,
                error: Some(err_msg),
                changed_files: Vec::new(),
            },
        }
    }
}

fn has_xml_edit_tags(text: &str) -> bool {
    text.contains("<edit")
        || text.contains("<replace")
        || text.contains("<create")
        || text.contains("<delete")
        || text.contains("<workbench_edits")
}

fn parse_xml_edits(text: &str, editable_paths: &[String]) -> Result<Vec<EditOperation>, String> {
    let mut edits = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let remaining = &text[pos..];

        // Find the next tag candidate
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

                // Parse search/replace pairs inside inner
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
        // Only treat as top-level if it has a 'path' attribute
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

fn resolve_target_path(candidate: &str, editable_paths: &[String]) -> String {
    let cleaned = candidate.trim().trim_matches('`').trim();
    for ed in editable_paths {
        if cleaned == ed || cleaned.ends_with(ed) {
            return ed.clone();
        }
    }
    cleaned.to_string()
}

fn normalize_content(raw: &str) -> String {
    let s = if let Some(stripped) = raw.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = raw.strip_prefix('\n') {
        stripped
    } else {
        raw
    };

    if let Some(last_nl) = s.rfind('\n') {
        let trailing = &s[last_nl + 1..];
        if trailing.chars().all(|c| c == ' ' || c == '\t') {
            return s[..=last_nl].to_string();
        }
    }
    s.to_string()
}

/// Whole file replacement edit protocol
#[derive(Debug, Default, Clone)]
pub struct WholeFileEditProtocol;

impl EditProtocol for WholeFileEditProtocol {
    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Whole File Replacement)\n");
        prompt.push_str("When proposing changes, provide the COMPLETE updated content of each modified file.\n");
        prompt.push_str("Only files listed in <editable_files> may be modified.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No files are currently marked as editable. Answer questions or explain code without proposing edits.\n\n");
        } else {
            prompt.push_str("Permitted target files:\n");
            for path in editable_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push_str("\n");
        }

        let sample_path = editable_paths
            .first()
            .map(|s| s.as_str())
            .unwrap_or("example_file.rs");

        prompt.push_str("Format for specifying file replacements:\n");
        prompt.push_str(&format!("FILE: {}\n", sample_path));
        prompt.push_str("```\n");
        prompt.push_str("// Complete file contents from first line to last line\n");
        prompt.push_str("```\n\n");

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Specify 'FILE: <exact_relative_path>' before the code block.\n");
        prompt.push_str("2. Target path MUST match a file listed under <editable_files>.\n");
        prompt.push_str("3. Inside the fenced block, include the ENTIRE file content. Never truncate with comments like '... rest of code ...'.\n");
        prompt.push_str("4. If no code changes are needed, reply with normal conversational text.\n\n");

        prompt
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        // Fallback check for workbench_edit JSON block
        if let Some(json_res) = crate::edits::parse_workbench_edit_json(raw_text) {
            return json_res;
        }

        let lines: Vec<&str> = raw_text.lines().collect();
        let mut edits = Vec::new();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i].trim();

            if let Some(target_path) = extract_whole_file_header(line, editable_paths) {
                i += 1;
                while i < lines.len() && !lines[i].trim().starts_with("```") {
                    i += 1;
                }
                if i >= lines.len() {
                    break;
                }
                i += 1;

                let mut content_lines = Vec::new();
                let mut found_end = false;
                while i < lines.len() {
                    let curr = lines[i];
                    if curr.trim().starts_with("```") {
                        found_end = true;
                        i += 1;
                        break;
                    }
                    content_lines.push(curr);
                    i += 1;
                }

                if found_end {
                    let mut full_content = content_lines.join("\n");
                    if !content_lines.is_empty() {
                        full_content.push('\n');
                    }

                    edits.push(EditOperation::Create {
                        path: target_path,
                        content: full_content,
                    });
                    continue;
                }
            }

            i += 1;
        }

        if edits.is_empty() {
            ModelResult::Answer {
                text: raw_text.to_string(),
            }
        } else {
            ModelResult::Edit {
                summary: "Applied whole file replacements".to_string(),
                edits,
                applied: false,
                error: None,
                changed_files: Vec::new(),
            }
        }
    }
}

fn extract_whole_file_header(line: &str, editable_paths: &[String]) -> Option<String> {
    let s = line.trim();
    let path_candidate = if let Some(stripped) = s.strip_prefix("FILE:") {
        stripped.trim()
    } else if let Some(stripped) = s.strip_prefix("file:") {
        stripped.trim()
    } else {
        return None;
    };

    let cleaned = path_candidate.trim_matches('`').trim();
    for ed in editable_paths {
        if cleaned == ed || cleaned.ends_with(ed) {
            return Some(ed.clone());
        }
    }

    if !cleaned.is_empty() && (cleaned.contains('/') || cleaned.contains('.')) {
        return Some(cleaned.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_protocol_edit_single_block() {
        let proto = XmlEditProtocol;
        let editable = vec!["crates/core/src/lib.rs".to_string()];

        let output = r#"I have updated the code:

<workbench_edits>
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
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace { path, old_text, new_text } => {
                        assert_eq!(path, "crates/core/src/lib.rs");
                        assert_eq!(old_text, "pub fn old_fn() {}\n");
                        assert_eq!(new_text, "pub fn new_fn() {}\n");
                    }
                    _ => panic!("Expected Replace"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_xml_protocol_multiple_operations() {
        let proto = XmlEditProtocol;
        let editable = vec![
            "src/main.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/old.rs".to_string(),
        ];

        let output = r#"Here are all the requested changes:

<workbench_edits>
  <edit path="src/main.rs">
    <search>
    let x = 1;
    </search>
    <replace>
    let x = 42;
    </replace>
  </edit>

  <create path="src/utils.rs">
pub fn helper() -> bool { true }
  </create>

  <delete path="src/old.rs" />
</workbench_edits>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 3);
                assert_eq!(
                    edits[0],
                    EditOperation::Replace {
                        path: "src/main.rs".to_string(),
                        old_text: "    let x = 1;\n".to_string(),
                        new_text: "    let x = 42;\n".to_string(),
                    }
                );
                assert_eq!(
                    edits[1],
                    EditOperation::Create {
                        path: "src/utils.rs".to_string(),
                        content: "pub fn helper() -> bool { true }\n".to_string(),
                    }
                );
                assert_eq!(
                    edits[2],
                    EditOperation::Delete {
                        path: "src/old.rs".to_string(),
                    }
                );
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_xml_protocol_code_with_generics_does_not_break() {
        let proto = XmlEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];

        let output = r#"
<edit path="src/lib.rs">
  <search>
fn parse<T: Clone>(item: Option<T>) -> Result<Vec<T>, Error> {
  </search>
  <replace>
fn parse<T: Clone + Send>(item: Option<T>) -> Result<Vec<T>, Error> {
  </replace>
</edit>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace { old_text, new_text, .. } => {
                        assert!(old_text.contains("<T: Clone>"));
                        assert!(new_text.contains("<T: Clone + Send>"));
                    }
                    _ => panic!("Expected Replace"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_xml_protocol_plain_conversational_answer() {
        let proto = XmlEditProtocol;
        let editable = vec!["src/main.rs".to_string()];

        let output = "Sure, you can implement this by creating a struct and implementing the Display trait.";
        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Answer { text } => {
                assert_eq!(text, output);
            }
            _ => panic!("Expected ModelResult::Answer"),
        }
    }
}
