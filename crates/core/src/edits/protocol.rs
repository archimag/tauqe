use workbench_protocol::{EditOperation, ModelResult};

use crate::model::gateway::{FunctionDefinition, ToolCall, ToolDefinition};

pub trait EditProtocol: Send + Sync {
    /// Identifier name of the edit protocol (e.g. "xml", "whole_file", "tool_call")
    fn name(&self) -> &'static str;

    /// Returns system prompt instructions tailored for this edit protocol.
    fn system_instructions(&self, editable_paths: &[String]) -> String;

    /// Parses the model text output into a ModelResult.
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult;

    /// Returns tool definitions if this protocol operates via native LLM tool calling.
    fn tools(&self, _editable_paths: &[String]) -> Option<Vec<ToolDefinition>> {
        None
    }

    /// Parses tool calls returned by the model into a ModelResult.
    fn parse_tool_calls(
        &self,
        _tool_calls: &[ToolCall],
        _editable_paths: &[String],
    ) -> Option<ModelResult> {
        None
    }
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol names.
    pub fn available_protocols() -> Vec<String> {
        vec![
            "xml".to_string(),
            "whole_file".to_string(),
            "tool_call".to_string(),
        ]
    }

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol name.
    pub fn canonical_name(name: &str) -> Option<String> {
        match name.trim().to_lowercase().as_str() {
            "xml" => Some("xml".to_string()),
            "whole_file" => Some("whole_file".to_string()),
            "tool_call" | "tool_calling" | "tools" | "tool" | "function_calling" | "functions" => {
                Some("tool_call".to_string())
            }
            _ => None,
        }
    }

    /// Creates an EditProtocol instance by name or alias.
    pub fn create_protocol(name: &str) -> Result<Box<dyn EditProtocol>, String> {
        match name.trim().to_lowercase().as_str() {
            "tool_call" | "tool_calling" | "tools" | "tool" | "function_calling" | "functions" => {
                Ok(Box::new(ToolCallEditProtocol))
            }
            "whole_file" => Ok(Box::new(WholeFileEditProtocol)),
            "xml" => Ok(Box::new(XmlEditProtocol)),
            other => Err(format!(
                "Unknown edit protocol '{}'. Available protocols: {}",
                other,
                Self::available_protocols().join(", ")
            )),
        }
    }
}

/// Factory function to create an EditProtocol by identifier name.
pub fn create_edit_protocol(name: &str) -> Box<dyn EditProtocol> {
    EditProtocolFactory::create_protocol(name).unwrap_or_else(|_| Box::new(XmlEditProtocol))
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
                applied: false,
                error: Some(err_msg),
                changed_files: Vec::new(),
                commit_hash: None,
            },
        }
    }
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
    fn name(&self) -> &'static str {
        "whole_file"
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Whole File Replacement)\n");
        prompt.push_str("When proposing changes, provide the COMPLETE updated content of each modified or newly created file.\n");
        prompt.push_str("Existing files must be listed in <editable_files>. You may also create new files by specifying their relative path.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No existing files are currently marked as editable. Answer questions or create new files if requested.\n\n");
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

        prompt.push_str("Format for specifying file replacements or new files:\n");
        prompt.push_str(&format!("FILE: {}\n", sample_path));
        prompt.push_str("```\n");
        prompt.push_str("// Complete file contents from first line to last line\n");
        prompt.push_str("```\n\n");

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Specify 'FILE: <exact_relative_path>' before the code block.\n");
        prompt.push_str("2. For existing files, target path MUST match a file listed under <editable_files>.\n");
        prompt.push_str("3. Inside the fenced block, include the ENTIRE file content. Never truncate with comments like '... rest of code ...'.\n");
        prompt.push_str("4. If no code changes are needed, reply with normal conversational text.\n\n");

        prompt
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
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
                commit_hash: None,
            }
        }
    }
}

/// Native LLM Tool Calling (Function Calling) edit protocol
#[derive(Debug, Default, Clone)]
pub struct ToolCallEditProtocol;

pub type FunctionCallingEditProtocol = ToolCallEditProtocol;

impl EditProtocol for ToolCallEditProtocol {
    fn name(&self) -> &'static str {
        "tool_call"
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Native Tool Calling)\n");
        prompt.push_str("You have access to tools for modifying files: `edit_file`, `create_file`, and `delete_file`.\n");
        prompt.push_str("When proposing changes, invoke the appropriate tool calls:\n");
        prompt.push_str("- `edit_file`: Replace an exact block of text in an existing file. Specify `path`, `old_text` (exact match), and `new_text`.\n");
        prompt.push_str("- `create_file`: Create a new file with `path` and `content`.\n");
        prompt.push_str("- `delete_file`: Delete an obsolete file with `path`.\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No existing files are currently marked as editable. Answer questions, explain code, or create new files with `create_file` if requested.\n\n");
        } else {
            prompt.push_str("Currently permitted existing target files for modification:\n");
            for path in editable_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push_str("\n");
        }

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. For `edit_file`, `path` MUST EXACTLY match one of the paths listed in permitted files.\n");
        prompt.push_str("2. For `edit_file`, `old_text` must match EXACTLY ONE location in the target file, including indentation and whitespace.\n");
        prompt.push_str("3. Keep `old_text` as concise as possible while remaining unique.\n");
        prompt.push_str("4. For `create_file`, specify the complete file content.\n");
        prompt.push_str("5. If no code changes are needed, answer conversationally without invoking tool calls.\n\n");

        prompt
    }

    fn tools(&self, editable_paths: &[String]) -> Option<Vec<ToolDefinition>> {
        let mut path_schema = serde_json::json!({
            "type": "string",
            "description": "Relative path to the permitted file to modify"
        });

        if !editable_paths.is_empty() {
            path_schema["enum"] = serde_json::json!(editable_paths);
        }

        Some(vec![
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "edit_file".to_string(),
                    description: "Replace an exact block of code in an existing permitted file with new code.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": path_schema,
                            "old_text": {
                                "type": "string",
                                "description": "Exact lines or code block from the target file to be replaced. Indentation and whitespace must match uniquely (exactly 1 occurrence)."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "New replacement code"
                            },
                            "summary": {
                                "type": "string",
                                "description": "Optional concise commit-style summary of the change"
                            }
                        },
                        "required": ["path", "old_text", "new_text"],
                        "additionalProperties": false
                    }),
                },
            },
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "create_file".to_string(),
                    description: "Create a new file with the specified content.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Relative path for the new file to create"
                            },
                            "content": {
                                "type": "string",
                                "description": "Complete file content from first line to last line"
                            },
                            "summary": {
                                "type": "string",
                                "description": "Optional concise explanation of why the file was created"
                            }
                        },
                        "required": ["path", "content"],
                        "additionalProperties": false
                    }),
                },
            },
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "delete_file".to_string(),
                    description: "Delete an existing file from the repository.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Relative path to the file to delete"
                            },
                            "summary": {
                                "type": "string",
                                "description": "Optional concise explanation of why the file was deleted"
                            }
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }),
                },
            },
        ])
    }

    fn parse_tool_calls(
        &self,
        tool_calls: &[ToolCall],
        editable_paths: &[String],
    ) -> Option<ModelResult> {
        if tool_calls.is_empty() {
            return None;
        }

        let mut edits = Vec::new();
        let mut summaries = Vec::new();

        for call in tool_calls {
            let fn_name = call.function.name.as_str();
            let args_str = &call.function.arguments;

            let args: serde_json::Value = match serde_json::from_str(args_str) {
                Ok(v) => v,
                Err(err) => {
                    return Some(ModelResult::Edit {
                        summary: "Malformed tool call arguments".to_string(),
                        edits: Vec::new(),
                        applied: false,
                        error: Some(format!(
                            "Invalid JSON in arguments for tool '{}': {}",
                            fn_name, err
                        )),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    });
                }
            };

            if let Some(s) = args.get("summary").and_then(|v| v.as_str()) {
                let clean = s.trim();
                if !clean.is_empty() {
                    summaries.push(clean.to_string());
                }
            }

            match fn_name {
                "edit_file" | "replace_in_file" | "str_replace" | "replace" => {
                    let path_raw = match extract_path_arg(&args) {
                        Some(p) => p,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'path' argument in edit_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'path' property in edit_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let old_text = match extract_string_arg(
                        &args,
                        &["old_text", "search", "old_str", "target", "original"],
                    ) {
                        Some(t) => t,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'old_text' argument in edit_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'old_text' property in edit_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let new_text = match extract_string_arg(
                        &args,
                        &["new_text", "replace", "new_str", "replacement", "content"],
                    ) {
                        Some(t) => t,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'new_text' argument in edit_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'new_text' property in edit_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let target_path = resolve_target_path(&path_raw, editable_paths);

                    if !editable_paths.is_empty() && !editable_paths.contains(&target_path) {
                        return Some(ModelResult::Edit {
                            summary: format!("File '{}' is not editable", target_path),
                            edits: Vec::new(),
                            applied: false,
                            error: Some(format!(
                                "Permission error: file '{}' is not marked as editable in context. Permitted files: {}",
                                target_path,
                                editable_paths.join(", ")
                            )),
                            changed_files: Vec::new(),
                            commit_hash: None,
                        });
                    }

                    edits.push(EditOperation::Replace {
                        path: target_path,
                        old_text: normalize_content(&old_text),
                        new_text: normalize_content(&new_text),
                    });
                }
                "create_file" | "write_file" | "new_file" => {
                    let path_raw = match extract_path_arg(&args) {
                        Some(p) => p,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'path' argument in create_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'path' property in create_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let content = match extract_string_arg(&args, &["content", "text", "body", "file_text"]) {
                        Some(c) => c,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'content' argument in create_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'content' property in create_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let target_path = resolve_target_path(&path_raw, editable_paths);
                    edits.push(EditOperation::Create {
                        path: target_path,
                        content: normalize_content(&content),
                    });
                }
                "delete_file" | "remove_file" => {
                    let path_raw = match extract_path_arg(&args) {
                        Some(p) => p,
                        None => {
                            return Some(ModelResult::Edit {
                                summary: "Missing 'path' argument in delete_file".to_string(),
                                edits: Vec::new(),
                                applied: false,
                                error: Some(
                                    "Schema mismatch: missing required 'path' property in delete_file"
                                        .to_string(),
                                ),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            });
                        }
                    };

                    let target_path = resolve_target_path(&path_raw, editable_paths);
                    edits.push(EditOperation::Delete { path: target_path });
                }
                "apply_edits" | "workbench_edits" | "batch_edits" => {
                    if let Some(edits_arr) = args.get("edits").and_then(|v| v.as_array()) {
                        for edit_val in edits_arr {
                            if let Some(path_raw) = extract_path_arg(edit_val) {
                                let target_path = resolve_target_path(&path_raw, editable_paths);
                                let op_type = edit_val
                                    .get("type")
                                    .or_else(|| edit_val.get("op"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("replace");

                                match op_type {
                                    "create" => {
                                        let content = extract_string_arg(edit_val, &["content", "text"]).unwrap_or_default();
                                        edits.push(EditOperation::Create {
                                            path: target_path,
                                            content: normalize_content(&content),
                                        });
                                    }
                                    "delete" => {
                                        edits.push(EditOperation::Delete { path: target_path });
                                    }
                                    _ => {
                                        let old_text = extract_string_arg(edit_val, &["old_text", "search"]).unwrap_or_default();
                                        let new_text = extract_string_arg(edit_val, &["new_text", "replace"]).unwrap_or_default();
                                        edits.push(EditOperation::Replace {
                                            path: target_path,
                                            old_text: normalize_content(&old_text),
                                            new_text: normalize_content(&new_text),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                unknown => {
                    return Some(ModelResult::Edit {
                        summary: format!("Unknown tool '{}'", unknown),
                        edits: Vec::new(),
                        applied: false,
                        error: Some(format!(
                            "Unknown tool '{}'. Expected 'edit_file', 'create_file', or 'delete_file'",
                            unknown
                        )),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    });
                }
            }
        }

        if edits.is_empty() {
            return None;
        }

        let summary = if !summaries.is_empty() {
            summaries.join("; ")
        } else if edits.len() == 1 {
            match &edits[0] {
                EditOperation::Replace { path, .. } => format!("Update {}", path),
                EditOperation::Create { path, .. } => format!("Create {}", path),
                EditOperation::Delete { path } => format!("Delete {}", path),
            }
        } else {
            format!("Apply {} code edits via tool call", edits.len())
        };

        Some(ModelResult::Edit {
            summary,
            edits,
            applied: false,
            error: None,
            changed_files: Vec::new(),
            commit_hash: None,
        })
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        if let Some(json_res) = crate::edits::parse_workbench_edit_json(raw_text) {
            return json_res;
        }

        if has_xml_edit_tags(raw_text) {
            return XmlEditProtocol.parse_output(raw_text, editable_paths);
        }

        ModelResult::Answer {
            text: raw_text.to_string(),
        }
    }
}

fn extract_path_arg(args: &serde_json::Value) -> Option<String> {
    for key in &["path", "file", "filepath", "file_path", "filename"] {
        if let Some(val) = args.get(*key).and_then(|v| v.as_str()) {
            let trimmed = val.trim().trim_matches('`').trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn extract_string_arg(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = args.get(*key).and_then(|v| v.as_str()) {
            return Some(val.to_string());
        }
    }
    None
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
    fn test_tool_call_protocol_edit_file() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec!["crates/core/src/main.rs".to_string()];

        let tools = proto.tools(&editable).expect("Expected tool definitions");
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].function.name, "edit_file");

        let tool_calls = vec![ToolCall {
            id: "call_1".to_string(),
            tool_type: "function".to_string(),
            function: FunctionCall {
                name: "edit_file".to_string(),
                arguments: serde_json::json!({
                    "path": "crates/core/src/main.rs",
                    "old_text": "fn old() {}",
                    "new_text": "fn new() {}"
                })
                .to_string(),
            },
        }];

        let res = proto
            .parse_tool_calls(&tool_calls, &editable)
            .expect("Expected ModelResult");
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Update crates/core/src/main.rs");
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace {
                        path,
                        old_text,
                        new_text,
                    } => {
                        assert_eq!(path, "crates/core/src/main.rs");
                        assert_eq!(old_text, "fn old() {}");
                        assert_eq!(new_text, "fn new() {}");
                    }
                    _ => panic!("Expected Replace operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_tool_call_protocol_create_and_delete() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec![];

        let tool_calls = vec![
            ToolCall {
                id: "call_c".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "create_file".to_string(),
                    arguments: serde_json::json!({
                        "path": "crates/core/src/foo.rs",
                        "content": "pub struct Foo;\n"
                    })
                    .to_string(),
                },
            },
            ToolCall {
                id: "call_d".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "delete_file".to_string(),
                    arguments: serde_json::json!({
                        "path": "crates/core/src/bar.rs"
                    })
                    .to_string(),
                },
            },
        ];

        let res = proto
            .parse_tool_calls(&tool_calls, &editable)
            .expect("Expected ModelResult");
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 2);
                assert!(matches!(&edits[0], EditOperation::Create { path, .. } if path == "crates/core/src/foo.rs"));
                assert!(matches!(&edits[1], EditOperation::Delete { path } if path == "crates/core/src/bar.rs"));
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_tool_call_protocol_permission_validation() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec!["src/main.rs".to_string()];

        let tool_calls = vec![ToolCall {
            id: "call_err".to_string(),
            tool_type: "function".to_string(),
            function: FunctionCall {
                name: "edit_file".to_string(),
                arguments: serde_json::json!({
                    "path": "secret/config.rs",
                    "old_text": "old",
                    "new_text": "new"
                })
                .to_string(),
            },
        }];

        let res = proto.parse_tool_calls(&tool_calls, &editable).unwrap();
        match res {
            ModelResult::Edit { error, applied, .. } => {
                assert!(!applied);
                assert!(error.expect("expected error").contains("Permission error"));
            }
            _ => panic!("Expected ModelResult::Edit with error"),
        }
    }

    #[test]
    fn test_tool_call_protocol_schema_mismatch() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec!["src/main.rs".to_string()];

        // Missing required old_text
        let tool_calls = vec![ToolCall {
            id: "call_bad".to_string(),
            tool_type: "function".to_string(),
            function: FunctionCall {
                name: "edit_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/main.rs",
                    "new_text": "new"
                })
                .to_string(),
            },
        }];

        let res = proto.parse_tool_calls(&tool_calls, &editable).unwrap();
        match res {
            ModelResult::Edit { error, .. } => {
                assert!(error.expect("expected error").contains("Schema mismatch"));
            }
            _ => panic!("Expected ModelResult::Edit with error"),
        }
    }

    #[test]
    fn test_create_edit_protocol_factory() {
        let p1 = create_edit_protocol("tool_call");
        assert_eq!(p1.name(), "tool_call");

        let p2 = create_edit_protocol("tools");
        assert_eq!(p2.name(), "tool_call");

        let p3 = create_edit_protocol("function_calling");
        assert_eq!(p3.name(), "tool_call");

        let p4 = create_edit_protocol("xml");
        assert_eq!(p4.name(), "xml");
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
