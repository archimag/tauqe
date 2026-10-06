use workbench_protocol::{EditOperation, ModelResult};

use crate::model::gateway::{FunctionDefinition, ToolCall, ToolDefinition};

use super::utils::{normalize_content, resolve_target_path};
use super::xml::{has_xml_edit_tags, XmlEditProtocol};
use super::EditProtocol;

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
        prompt.push_str("You have access to tools for modifying files: `edit_file`, `create_file`, `delete_file`, and `batch_edits`.\n");
        prompt.push_str("When proposing changes, invoke the appropriate tool calls:\n");
        prompt.push_str("- `edit_file`: Replace an exact block of text in an existing file. Specify `path`, `old_text` (exact match), and `new_text`.\n");
        prompt.push_str("- `create_file`: Create a new file with `path` and `content`.\n");
        prompt.push_str("- `delete_file`: Delete an obsolete file with `path`.\n");
        prompt.push_str("- `batch_edits`: Apply multiple edits across multiple files in a single operation.\n\n");

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
        prompt.push_str("1. SINGLE-TURN EXECUTION: All changes across ALL affected files MUST be proposed in this SINGLE response using parallel tool calls. There is no multi-turn tool review loop — do NOT emit edits for one file and wait for feedback before editing the next.\n");
        prompt.push_str("2. PARALLEL TOOL CALLS: You can and MUST emit multiple tool calls (`edit_file`, `create_file`, `delete_file`) in a single turn whenever the task touches multiple files or multiple locations.\n");
        prompt.push_str("3. When refactoring or splitting files, call `create_file` for each new module and `edit_file` on the source file in the same turn.\n");
        prompt.push_str("4. For `edit_file`, `path` MUST EXACTLY match one of the paths listed in permitted files.\n");
        prompt.push_str("5. For `edit_file`, `old_text` must match EXACTLY ONE location in the target file, including indentation and whitespace.\n");
        prompt.push_str("6. Keep `old_text` as concise as possible while remaining unique.\n");
        prompt.push_str("7. For `create_file`, specify the complete file content.\n");
        prompt.push_str("8. If no code changes are needed, answer conversationally without invoking tool calls.\n\n");

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
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: "batch_edits".to_string(),
                    description: "Apply a batch of edits across multiple files atomically in a single operation.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "summary": {
                                "type": "string",
                                "description": "Concise summary of the batch changes"
                            },
                            "edits": {
                                "type": "array",
                                "description": "List of edit operations to apply",
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
                                            "description": "Relative file path"
                                        },
                                        "old_text": {
                                            "type": "string",
                                            "description": "Exact text to replace (required for replace)"
                                        },
                                        "new_text": {
                                            "type": "string",
                                            "description": "Replacement text (required for replace)"
                                        },
                                        "content": {
                                            "type": "string",
                                            "description": "Full file content (required for create)"
                                        }
                                    },
                                    "required": ["op", "path"]
                                }
                            }
                        },
                        "required": ["edits"],
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
                        proposal: None,
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
                                proposal: None,
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
                                proposal: None,
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
                                proposal: None,
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
                            proposal: None,
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
                                proposal: None,
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
                                proposal: None,
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
                                proposal: None,
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
                        proposal: None,
                        applied: false,
                        error: Some(format!(
                            "Unknown tool '{}'. Expected 'edit_file', 'create_file', 'delete_file', or 'batch_edits'",
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
            proposal: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_call_protocol_edit_file() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec!["crates/core/src/main.rs".to_string()];

        let tools = proto.tools(&editable).expect("Expected tool definitions");
        assert_eq!(tools.len(), 4);
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
    fn test_tool_call_protocol_batch_edits() {
        use crate::model::gateway::FunctionCall;

        let proto = ToolCallEditProtocol;
        let editable = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        let tool_calls = vec![ToolCall {
            id: "call_batch".to_string(),
            tool_type: "function".to_string(),
            function: FunctionCall {
                name: "batch_edits".to_string(),
                arguments: serde_json::json!({
                    "summary": "Refactor a and b",
                    "edits": [
                        {
                            "op": "replace",
                            "path": "src/a.rs",
                            "old_text": "old_a",
                            "new_text": "new_a"
                        },
                        {
                            "op": "replace",
                            "path": "src/b.rs",
                            "old_text": "old_b",
                            "new_text": "new_b"
                        }
                    ]
                })
                .to_string(),
            },
        }];

        let res = proto
            .parse_tool_calls(&tool_calls, &editable)
            .expect("Expected ModelResult");
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Refactor a and b");
                assert_eq!(edits.len(), 2);
                assert!(matches!(&edits[0], EditOperation::Replace { path, .. } if path == "src/a.rs"));
                assert!(matches!(&edits[1], EditOperation::Replace { path, .. } if path == "src/b.rs"));
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
}
