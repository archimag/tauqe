//! JSON Schema (Structured Output) prompt protocol.
//!
//! [`JsonPromptProtocol`] decodes Structured Output (JSON schema) model output
//! into the normalized [`LlmCommand`] vocabulary consumed by the execution
//! pipeline. It isolates the JSON Schema wire syntax from the core business
//! logic and delegates concrete parsing to the structured edit protocol so the
//! schema keeps a single source of truth.

use crate::edits::protocol::structured::StructuredEditProtocol;
use crate::edits::protocol::EditProtocol;
use tauqe_protocol::{LlmCapability, LlmCommand, ModelResult};

use super::protocol::PromptProtocol;
use super::xml::{map_verify_on_success, map_verify_target};

/// JSON Schema (Structured Output) wire-format implementation of [`PromptProtocol`].
#[derive(Debug, Default, Clone)]
pub struct JsonPromptProtocol;

impl PromptProtocol for JsonPromptProtocol {
    fn name(&self) -> &'static str {
        "structured"
    }

    fn capabilities(&self) -> &[LlmCapability] {
        &[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Discovery,
            LlmCapability::ToolchainVerify,
            LlmCapability::UserLanguage,
        ]
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        let structured = StructuredEditProtocol;
        let mut commands = Vec::new();

        // Conversational text with structured JSON markup removed.
        let text = structured.clean_assistant_text(raw_text);
        if !text.trim().is_empty() {
            commands.push(LlmCommand::Text { content: text });
        }

        // File edit operations decoded from the structured `changes` array.
        if let ModelResult::Edit { edits, .. } = structured.parse_output(raw_text, &[]) {
            for operation in edits {
                commands.push(LlmCommand::Edit { operation });
            }
        }

        // Discovery: context requests.
        for request in structured.parse_context_requests(raw_text) {
            commands.push(LlmCommand::ContextRequest {
                path: request.path,
                access: request.access,
            });
        }

        // Toolchain verification decoded from control tags when present.
        if let Some(request) = structured.parse_verify_request(raw_text) {
            commands.push(LlmCommand::Verify {
                target: map_verify_target(request.target),
                on_success: map_verify_on_success(request.on_success),
            });
        }

        // Detected user language decoded from control tags.
        if let Some(language) = structured.parse_user_language(raw_text) {
            commands.push(LlmCommand::UserLanguage { language });
        }

        commands
    }

    fn turn_marker(&self) -> Option<&str> {
        StructuredEditProtocol.turn_marker()
    }

    fn with_turn_marker(&self, _marker: &str) -> Option<Box<dyn PromptProtocol>> {
        // Structured Output encodes structure in JSON Schema rather than
        // marker-bound tags, so no marker variant is produced.
        None
    }

    fn response_format(
        &self,
        editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        StructuredEditProtocol.response_format(editable_paths)
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        StructuredEditProtocol.create_stream_filter(editable_paths, repo_root, staged_contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, EditOperation};

    #[test]
    fn test_name_and_capabilities() {
        let protocol = JsonPromptProtocol;
        assert_eq!(protocol.name(), "structured");
        for capability in [
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Discovery,
            LlmCapability::ToolchainVerify,
            LlmCapability::UserLanguage,
        ] {
            assert!(protocol.supports(capability));
        }
        assert!(!protocol.supports(LlmCapability::Scratchpad));
        assert!(!protocol.supports(LlmCapability::Outcome));
    }

    #[test]
    fn test_decode_answer_without_changes() {
        let protocol = JsonPromptProtocol;
        let output = r#"{"message":"Hello there! How can I help you?","changes":[],"context_requests":[],"context_drops":[],"suggested_actions":[]}"#;
        let commands = protocol.decode(output);
        assert_eq!(
            commands,
            vec![LlmCommand::Text {
                content: "Hello there! How can I help you?".to_string(),
            }]
        );
    }

    #[test]
    fn test_decode_file_edit() {
        let protocol = JsonPromptProtocol;
        let output = r#"{
            "message": "Fix the bug",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/lib.rs",
                    "to": "",
                    "old_text": "old();",
                    "new_text": "new();",
                    "content": ""
                }
            ],
            "context_requests": [],
            "context_drops": [],
            "suggested_actions": []
        }"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::Text {
            content: "Fix the bug".to_string(),
        }));

        let operation = commands
            .iter()
            .find_map(|command| match command {
                LlmCommand::Edit { operation } => Some(operation.clone()),
                _ => None,
            })
            .expect("edit command");
        match operation {
            EditOperation::Replace {
                path,
                old_text,
                new_text,
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(old_text, "old();");
                assert_eq!(new_text, "new();");
            }
            other => panic!("unexpected operation: {other:?}"),
        }
    }

    #[test]
    fn test_decode_discovery_requests() {
        let protocol = JsonPromptProtocol;
        let output = r#"{"message":"Need more context","changes":[],"context_requests":["src/a.rs","editable:src/b.rs"],"suggested_actions":[]}"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }));
        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "src/b.rs".to_string(),
            access: ContextAccess::Editable,
        }));
    }

    #[test]
    fn test_decode_user_language_control_tag() {
        let protocol = JsonPromptProtocol;
        let output = concat!(
            "{\"message\":\"Done.\",\"changes\":[],\"context_requests\":[],\"context_drops\":[],\"suggested_actions\":[]}\n",
            "<user_language>Russian</user_language>\n",
        );
        let commands = protocol.decode(output);
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
    }
}
