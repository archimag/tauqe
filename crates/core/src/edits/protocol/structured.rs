use workbench_protocol::{
    EditOperation, ModelResult, ModelResultProposal,
};

use crate::model::gateway::{JsonSchemaDefinition, ResponseFormat};

use super::utils::{normalize_content, resolve_target_path};
use super::EditProtocol;

/// Structured Output JSON-schema edit protocol
#[derive(Debug, Default, Clone)]
pub struct StructuredEditProtocol;

impl StructuredEditProtocol {
    pub fn schema(_editable_paths: &[String]) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Conversational explanation or response to the user"
                },
                "changes": {
                    "type": "array",
                    "description": "List of structured code modification operations",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["replace", "create", "delete"],
                                "description": "Operation type: 'replace' for modifying existing files, 'create' for new files, 'delete' for deleting files"
                            },
                            "path": {
                                "type": "string",
                                "description": "Relative repository path to the target file"
                            },
                            "old_text": {
                                "type": "string",
                                "description": "Exact text to replace in the file (required for 'replace', empty string otherwise)"
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text (for 'replace', empty string otherwise)"
                            },
                            "content": {
                                "type": "string",
                                "description": "Complete file content (required for 'create', empty string otherwise)"
                            }
                        },
                        "required": ["op", "path", "old_text", "new_text", "content"],
                        "additionalProperties": false
                    }
                },
                "context_requests": {
                    "type": "array",
                    "description": "Optional list of file paths or glob patterns to request adding to context for subsequent turns",
                    "items": {
                        "type": "string"
                    }
                },
                "suggested_actions": {
                    "type": "array",
                    "description": "Optional list of suggested follow-up options for the user",
                    "items": {
                        "type": "string"
                    }
                }
            },
            "required": ["message", "changes", "context_requests", "suggested_actions"],
            "additionalProperties": false
        })
    }

    fn proposal_to_model_result(
        &self,
        proposal: ModelResultProposal,
        editable_paths: &[String],
    ) -> ModelResult {
        if proposal.changes.is_empty() {
            return ModelResult::Answer {
                text: proposal.message,
            };
        }

        let mut edits = Vec::new();

        for change in &proposal.changes {
            let target_path = resolve_target_path(&change.path, editable_paths);

            if !editable_paths.is_empty()
                && change.op != "create"
                && !editable_paths.contains(&target_path)
            {
                return ModelResult::Edit {
                    summary: format!("File '{}' is not editable", target_path),
                    edits: Vec::new(),
                    proposal: Some(proposal),
                    applied: false,
                    error: Some(format!(
                        "Permission error: file '{}' is not marked as editable in context. Permitted files: {}",
                        target_path,
                        editable_paths.join(", ")
                    )),
                    changed_files: Vec::new(),
                    commit_hash: None,
                };
            }

            match change.op.as_str() {
                "create" => {
                    edits.push(EditOperation::Create {
                        path: target_path,
                        content: normalize_content(change.content.as_deref().unwrap_or_default()),
                    });
                    // Flat-string schema: empty values mean "not applicable".
                }
                "delete" => {
                    edits.push(EditOperation::Delete { path: target_path });
                }
                _ => {
                    edits.push(EditOperation::Replace {
                        path: target_path,
                        old_text: normalize_content(change.old_text.as_deref().unwrap_or_default()),
                        new_text: normalize_content(
                            change
                                .new_text
                                .as_deref()
                                .filter(|s| !s.is_empty())
                                .or(change.content.as_deref())
                                .unwrap_or_default(),
                        ),
                    });
                }
            }
        }

        let summary = if !proposal.message.trim().is_empty() {
            let first_line = proposal
                .message
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Applied changes")
                .trim();
            if first_line.chars().count() > 100 {
                let truncated: String = first_line.chars().take(97).collect();
                format!("{}...", truncated)
            } else {
                first_line.to_string()
            }
        } else if edits.len() == 1 {
            match &edits[0] {
                EditOperation::Replace { path, .. } => format!("Update {}", path),
                EditOperation::Create { path, .. } => format!("Create {}", path),
                EditOperation::Delete { path } => format!("Delete {}", path),
            }
        } else {
            format!("Apply {} structured edits", edits.len())
        };

        ModelResult::Edit {
            summary,
            edits,
            proposal: Some(proposal),
            applied: false,
            error: None,
            changed_files: Vec::new(),
            commit_hash: None,
        }
    }
}

impl EditProtocol for StructuredEditProtocol {
    fn name(&self) -> &'static str {
        "structured"
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Structured Output JSON)\n");
        prompt.push_str("You must respond with a JSON object conforming to the structured output schema with the following fields:\n");
        prompt.push_str("- `message`: Your conversational explanation or answer to the user.\n");
        prompt.push_str("- `changes`: Array of code edit operations (`op`: 'replace'|'create'|'delete', `path`, `old_text`, `new_text`, `content`).\n");
        prompt.push_str("- `context_requests`: Optional list of file paths to request adding to context for subsequent turns.\n");
        prompt.push_str("- `suggested_actions`: Optional list of suggested follow-up options for the user.\n");
        prompt.push_str("Every field of a change is required: use an empty string \"\" for fields that do not apply to the operation (e.g. `old_text`/`new_text` for 'create'/'delete', `content` for 'replace').\n\n");

        if editable_paths.is_empty() {
            prompt.push_str("NOTE: No existing files are currently marked as editable. Answer questions in `message` or create new files with `op: 'create'` in `changes`.\n\n");
        } else {
            prompt.push_str("Currently permitted existing target files for modification:\n");
            for path in editable_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push_str("\n");
        }

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Output MUST be valid JSON adhering to the specified schema.\n");
        prompt.push_str("2. For 'replace', `path` MUST match one of the permitted files in <editable_files>.\n");
        prompt.push_str("3. For 'replace', `old_text` must match EXACTLY ONE location in the target file, including indentation and whitespace.\n");
        prompt.push_str("4. For 'create', `content` must contain the complete file content from first to last line.\n");
        prompt.push_str("5. If no code changes are needed, set `changes: []` and provide your answer in `message`.\n\n");

        prompt
    }

    fn response_format(&self, editable_paths: &[String]) -> Option<ResponseFormat> {
        Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "workbench_edit_proposal".to_string(),
                description: Some("Structured code modification proposal and response".to_string()),
                schema: Self::schema(editable_paths),
                strict: Some(true),
            },
        })
    }

    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        if let Some(proposal) = parse_proposal(raw_text) {
            return self.proposal_to_model_result(proposal, editable_paths);
        }

        if let Some(json_res) = crate::edits::parse_workbench_edit_json(raw_text) {
            return json_res;
        }

        if super::xml::has_xml_edit_tags(raw_text) {
            return super::xml::XmlEditProtocol.parse_output(raw_text, editable_paths);
        }

        ModelResult::Answer {
            text: raw_text.to_string(),
        }
    }
}

/// Robustly deserializes a proposal regardless of key order or surrounding text.
fn parse_proposal(raw: &str) -> Option<ModelResultProposal> {
    let clean = extract_json_str(raw);
    if let Ok(p) = serde_json::from_str::<ModelResultProposal>(clean) {
        return Some(p);
    }

    let mut from = 0;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        if let Some(end) = balanced_object_end(raw, start) {
            if let Ok(p) = serde_json::from_str::<ModelResultProposal>(&raw[start..end]) {
                return Some(p);
            }
        }
        from = start + 1;
    }
    None
}

/// Returns the exclusive end offset of the JSON object starting at `start`, if complete.
fn balanced_object_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn extract_json_str(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(idx) = trimmed.find("```json") {
        let after = &trimmed[idx + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    } else if let Some(idx) = trimmed.find("```") {
        let after = &trimmed[idx + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_structured_protocol_parse_replace() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let json = r#"{
            "message": "Update greeting function",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/lib.rs",
                    "old_text": "fn old() {}",
                    "new_text": "fn new() {}"
                }
            ],
            "context_requests": [],
            "suggested_actions": ["Run cargo test"]
        }"#;

        let res = proto.parse_output(json, &editable);
        match res {
            ModelResult::Edit {
                summary,
                edits,
                proposal,
                ..
            } => {
                assert_eq!(summary, "Update greeting function");
                assert_eq!(edits.len(), 1);
                assert!(proposal.is_some());
                match &edits[0] {
                    EditOperation::Replace {
                        path,
                        old_text,
                        new_text,
                    } => {
                        assert_eq!(path, "src/lib.rs");
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
    fn test_structured_protocol_utf8_long_summary_truncation() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        // A long Cyrillic string exceeding 100 characters where byte 97 falls inside a multibyte char
        let cyrillic_msg = "Я обновлю реализацию функции валидации пути и добавлю расширенную обработку возможных ошибок для обеспечения безопасности";
        let json = serde_json::json!({
            "message": cyrillic_msg,
            "changes": [
                {
                    "op": "replace",
                    "path": "src/lib.rs",
                    "old_text": "a",
                    "new_text": "b",
                    "content": null
                }
            ],
            "context_requests": [],
            "suggested_actions": []
        }).to_string();

        let res = proto.parse_output(&json, &editable);
        match res {
            ModelResult::Edit { summary, .. } => {
                assert!(summary.ends_with("..."));
                assert!(summary.chars().count() <= 100);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_schema_has_no_nullable_types() {
        let schema = StructuredEditProtocol::schema(&[]);
        let text = schema.to_string();
        assert!(!text.contains("\"null\""));
        let required = schema["properties"]["changes"]["items"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(required.len(), 5);
    }

    #[test]
    fn test_structured_protocol_empty_string_fields() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Create file",
            "changes": [
                {"op": "create", "path": "src/new.rs", "old_text": "", "new_text": "", "content": "fn x() {}\n"}
            ],
            "context_requests": [],
            "suggested_actions": []
        }"#;
        match proto.parse_output(json, &editable) {
            ModelResult::Edit { edits, .. } => match &edits[0] {
                EditOperation::Create { path, content } => {
                    assert_eq!(path, "src/new.rs");
                    assert_eq!(content, "fn x() {}\n");
                }
                _ => panic!("Expected Create"),
            },
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_changes_before_message_with_wrapper_text() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let raw = "Here you go:\n{\"changes\":[{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"old_text\":\"a {\",\"new_text\":\"b {\",\"content\":\"\"}],\"message\":\"Swap a for b\",\"context_requests\":[],\"suggested_actions\":[]}\nThanks!";
        match proto.parse_output(raw, &editable) {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Swap a for b");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_parse_answer() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Hello there! How can I help you?",
            "changes": [],
            "context_requests": [],
            "suggested_actions": []
        }"#;

        let res = proto.parse_output(json, &editable);
        match res {
            ModelResult::Answer { text } => {
                assert_eq!(text, "Hello there! How can I help you?");
            }
            _ => panic!("Expected ModelResult::Answer"),
        }
    }
}
