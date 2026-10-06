use workbench_protocol::{EditOperation, ModelResult};

use super::EditProtocol;

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
        prompt.push_str("When refactoring or splitting files, provide a FILE: block for each modified or created file in the same response.\n");
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
                proposal: None,
                applied: false,
                error: None,
                changed_files: Vec::new(),
                commit_hash: None,
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
    fn test_whole_file_protocol_replace() {
        let proto = WholeFileEditProtocol;
        let editable = vec!["crates/core/src/app.rs".to_string()];

        let output = r#"Here is the updated file:

FILE: crates/core/src/app.rs
```rust
fn run() {
    println!("hello");
}
```
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Create { path, content } => {
                        assert_eq!(path, "crates/core/src/app.rs");
                        assert!(content.contains("fn run()"));
                    }
                    _ => panic!("Expected Create operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }
}
