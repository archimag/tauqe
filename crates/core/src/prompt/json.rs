//! JSON Schema (Structured Output) prompt protocol.
//!
//! [`JsonPromptProtocol`] decodes Structured Output (JSON schema) model output
//! into the normalized [`LlmCommand`] vocabulary consumed by the execution
//! pipeline. It isolates the JSON Schema wire syntax from the core business
//! logic and delegates concrete parsing to the structured edit protocol so the
//! schema keeps a single source of truth.

use crate::edits::protocol::structured::StructuredEditProtocol;
use crate::edits::protocol::EditProtocol;
use crate::edits::stream::discussion::DiscussionStreamFilter;
use tauqe_protocol::{LlmCapability, LlmCommand, ModelResult};

use super::protocol::PromptProtocol;
use super::xml::{map_verify_on_success, map_verify_target};

/// Operating mode for [`JsonPromptProtocol`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum JsonPromptMode {
    /// Atomic file editing mode with structured change proposals.
    #[default]
    Edits,
    /// Discussion mode streaming a target text field (by default `"message"`)
    /// and decoding structured plan updates, context requests, and user language.
    Discussion { stream_field: String },
}

/// JSON Schema (Structured Output) wire-format implementation of [`PromptProtocol`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JsonPromptProtocol {
    mode: JsonPromptMode,
}

impl JsonPromptProtocol {
    /// Creates a default structured protocol in edit mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a structured protocol configured for file editing.
    pub fn edits() -> Self {
        Self {
            mode: JsonPromptMode::Edits,
        }
    }

    /// Creates a structured protocol configured for discussion mode with the
    /// standard `"message"` streaming field.
    pub fn discussion() -> Self {
        Self {
            mode: JsonPromptMode::Discussion {
                stream_field: "message".to_string(),
            },
        }
    }

    /// Creates a structured protocol configured for discussion mode streaming
    /// an arbitrary top-level JSON string field.
    pub fn with_stream_field(stream_field: impl Into<String>) -> Self {
        Self {
            mode: JsonPromptMode::Discussion {
                stream_field: stream_field.into(),
            },
        }
    }

    /// Returns the active operating mode.
    pub fn mode(&self) -> &JsonPromptMode {
        &self.mode
    }
}

impl PromptProtocol for JsonPromptProtocol {
    fn name(&self) -> &'static str {
        "structured"
    }

    fn capabilities(&self) -> &[LlmCapability] {
        match &self.mode {
            JsonPromptMode::Edits => &[
                LlmCapability::TextMessage,
                LlmCapability::FileEdits,
                LlmCapability::Discovery,
                LlmCapability::ToolchainVerify,
                LlmCapability::UserLanguage,
            ],
            JsonPromptMode::Discussion { .. } => &[
                LlmCapability::TextMessage,
                LlmCapability::PlanCreation,
                LlmCapability::PlanUpdate,
                LlmCapability::Discovery,
                LlmCapability::UserLanguage,
            ],
        }
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        match &self.mode {
            JsonPromptMode::Edits => {
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
            JsonPromptMode::Discussion { .. } => {
                let resp = DiscussionStreamFilter::parse_response(raw_text);
                let mut commands = Vec::new();

                if !resp.message.trim().is_empty() {
                    commands.push(LlmCommand::Text {
                        content: resp.message,
                    });
                }

                if let Some(update) = resp.plan_update {
                    commands.push(LlmCommand::Plan { update });
                }

                for request in resp.context_requests {
                    commands.push(LlmCommand::ContextRequest {
                        path: request.path,
                        access: request.access,
                    });
                }

                if let Some(language) = resp.user_language {
                    commands.push(LlmCommand::UserLanguage { language });
                }

                commands
            }
        }
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        match &self.mode {
            JsonPromptMode::Edits => StructuredEditProtocol.system_instructions(editable_paths),
            JsonPromptMode::Discussion { stream_field } => {
                if stream_field == "message" {
                    "You are operating in Structured Discussion mode. Formulate all responses as a valid JSON object matching the discussion_response schema:\n\
                    - message: Conversational Markdown explanation, reasoning, and answers. This field is streamed directly to the user interface.\n\
                    - plan_update: null (unless the user explicitly instructed to create or modify a plan)\n\
                    - context_requests: list of { path, access } files to add to context\n\
                    - user_language: detected primary natural language of user prompt (e.g. Russian, English)\n\
                    Output ONLY raw valid JSON matching this schema, without any outer markdown fences or markdown wrapping."
                        .to_string()
                } else {
                    format!(
                        "You are operating in Structured Output mode. Formulate all responses as a valid JSON object.\n\
                        - {stream_field}: Conversational Markdown response streamed directly to the user interface.\n\
                        Output ONLY raw valid JSON matching the schema, without any outer markdown fences or markdown wrapping."
                    )
                }
            }
        }
    }

    fn turn_marker(&self) -> Option<&str> {
        match &self.mode {
            JsonPromptMode::Edits => StructuredEditProtocol.turn_marker(),
            JsonPromptMode::Discussion { .. } => None,
        }
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
        match &self.mode {
            JsonPromptMode::Edits => StructuredEditProtocol.response_format(editable_paths),
            JsonPromptMode::Discussion { .. } => {
                Some(crate::providers::ResponseFormat::JsonSchema {
                    json_schema: crate::providers::JsonSchemaDefinition {
                        name: "discussion_response".to_string(),
                        description: Some(
                            "Structured response for discussion and planning modes".to_string(),
                        ),
                        strict: Some(true),
                        schema: tauqe_protocol::discussion_response_schema(),
                    },
                })
            }
        }
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        match &self.mode {
            JsonPromptMode::Edits => {
                StructuredEditProtocol.create_stream_filter(editable_paths, repo_root, staged_contents)
            }
            JsonPromptMode::Discussion { stream_field } => {
                Box::new(DiscussionStreamFilter::with_target_field(stream_field.clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, EditOperation};

    #[test]
    fn test_name_and_capabilities() {
        let protocol = JsonPromptProtocol::default();
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
        let protocol = JsonPromptProtocol::default();
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
        let protocol = JsonPromptProtocol::default();
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
        let protocol = JsonPromptProtocol::default();
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
        let protocol = JsonPromptProtocol::default();
        let output = concat!(
            "{\"message\":\"Done.\",\"changes\":[],\"context_requests\":[],\"context_drops\":[],\"suggested_actions\":[]}\n",
            "<user_language>Russian</user_language>\n",
        );
        let commands = protocol.decode(output);
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
    }

    #[test]
    fn test_discussion_mode_capabilities() {
        let protocol = JsonPromptProtocol::discussion();
        assert_eq!(protocol.name(), "structured");
        assert!(protocol.supports(LlmCapability::TextMessage));
        assert!(protocol.supports(LlmCapability::PlanCreation));
        assert!(protocol.supports(LlmCapability::PlanUpdate));
        assert!(protocol.supports(LlmCapability::Discovery));
        assert!(protocol.supports(LlmCapability::UserLanguage));
        assert!(!protocol.supports(LlmCapability::FileEdits));
        assert!(!protocol.supports(LlmCapability::ToolchainVerify));
    }

    #[test]
    fn test_decode_discussion_response() {
        let protocol = JsonPromptProtocol::discussion();
        let output = r#"{
            "message": "Let us refine the concept.",
            "plan_update": {
                "action": "update",
                "id": "p-1",
                "title": "Refined Plan",
                "description": "Updated",
                "concept": [],
                "items": []
            },
            "context_requests": [
                {
                    "path": "crates/core/src/lib.rs",
                    "access": "read_only"
                }
            ],
            "user_language": "Russian"
        }"#;

        let commands = protocol.decode(output);
        assert!(commands.contains(&LlmCommand::Text {
            content: "Let us refine the concept.".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "crates/core/src/lib.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }));
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
        let has_plan = commands.iter().any(|c| {
            matches!(c, LlmCommand::Plan { update } if update.id.as_deref() == Some("p-1"))
        });
        assert!(has_plan);
    }

    #[test]
    fn test_discussion_streaming_filter_integration() {
        let protocol = JsonPromptProtocol::discussion();
        let mut filter = protocol.create_stream_filter(
            vec![],
            std::path::PathBuf::from("/tmp"),
            std::collections::HashMap::new(),
        );

        let chunk1 = r#"{"message":"Hello developer"#;
        let events1 = filter.push_chunk(chunk1);
        let text1: String = events1
            .into_iter()
            .filter_map(|e| match e {
                crate::providers::StreamEvent::TextDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(text1, "Hello developer");

        let chunk2 = r#"!","plan_update":null,"context_requests":[],"user_language":"English"}"#;
        let events2 = filter.push_chunk(chunk2);
        let text2: String = events2
            .into_iter()
            .filter_map(|e| match e {
                crate::providers::StreamEvent::TextDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(text2, "!");

        let final_events = filter.finish();
        assert!(final_events.is_empty());
    }
}
