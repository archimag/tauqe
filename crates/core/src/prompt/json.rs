//! JSON Schema (Structured Output) prompt protocol.
//!
//! [`JsonPromptProtocol`] decodes Structured Output (JSON schema) model output
//! into the normalized [`LlmCommand`] vocabulary consumed by the execution
//! pipeline. It isolates the JSON Schema wire syntax from the core business
//! logic and delegates concrete parsing to the structured edit protocol so the
//! schema keeps a single source of truth.

use crate::edits::protocol::structured::StructuredEditProtocol;
use crate::edits::protocol::TerminalOutcome;
use crate::edits::stream::discussion::DiscussionStreamFilter;
use crate::edits::stream::JsonStreamFilter;
use tauqe_protocol::{CapabilitySet, LlmCapability, LlmCommand, LlmOutcome, ModelResult};

use crate::capabilities::{map_verify_on_success, map_verify_target};

use super::protocol::PromptProtocol;

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
///
/// The protocol carries the capability set of its operating mode, so prompt
/// composition and command decoding are both derived from one declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPromptProtocol {
    mode: JsonPromptMode,
    capabilities: CapabilitySet,
}

impl Default for JsonPromptProtocol {
    fn default() -> Self {
        Self::edits()
    }
}

impl JsonPromptProtocol {
    /// Creates a default structured protocol in edit mode.
    pub fn new() -> Self {
        Self::edits()
    }

    /// Creates a structured protocol configured for file editing.
    pub fn edits() -> Self {
        Self {
            mode: JsonPromptMode::Edits,
            capabilities: CapabilitySet::develop_structured(),
        }
    }

    /// Creates a structured protocol configured for discussion mode with the
    /// standard `"message"` streaming field.
    pub fn discussion() -> Self {
        Self {
            mode: JsonPromptMode::Discussion {
                stream_field: "message".to_string(),
            },
            capabilities: CapabilitySet::discussion(),
        }
    }

    /// Creates a structured protocol configured for discussion mode streaming
    /// an arbitrary top-level JSON string field.
    pub fn with_stream_field(stream_field: impl Into<String>) -> Self {
        Self {
            mode: JsonPromptMode::Discussion {
                stream_field: stream_field.into(),
            },
            capabilities: CapabilitySet::discussion(),
        }
    }

    /// Creates a structured protocol narrowed to an explicit capability set.
    ///
    /// The operating mode is derived from the capability set so the wire
    /// schema and the decoded surface can never disagree with what the protocol
    /// advertises.
    pub fn with_capabilities(capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        let mode = Self::mode_for(&capabilities);
        Box::new(Self { mode, capabilities })
    }

    /// Derives the operating mode that matches the requested capabilities.
    ///
    /// A set that enables [`LlmCapability::FileEdits`] uses edit mode, while a
    /// set without it streams a conversational `message` field.
    fn mode_for(capabilities: &CapabilitySet) -> JsonPromptMode {
        if capabilities.contains(LlmCapability::FileEdits) {
            JsonPromptMode::Edits
        } else {
            JsonPromptMode::Discussion {
                stream_field: "message".to_string(),
            }
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
        self.capabilities.as_slice()
    }

    fn capability_set(&self) -> CapabilitySet {
        self.capabilities.clone()
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        let mode = Self::mode_for(&capabilities);
        Box::new(Self { mode, capabilities })
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        let structured = StructuredEditProtocol;
        let mut commands = Vec::new();

        // Conversational text with structured JSON markup removed. This path
        // serves every mode because each structured response carries `message`.
        let text = structured.clean_assistant_text(raw_text);
        if !text.trim().is_empty() {
            commands.push(LlmCommand::Text { content: text });
        }

        // File edit operations decoded from the structured `changes` array.
        match structured.parse_output(raw_text, &[]) {
            ModelResult::Edit { edits, outcome, .. } => {
                if let Some(message) = outcome.error {
                    commands.push(LlmCommand::ProtocolError { message });
                }
                for operation in edits {
                    commands.push(LlmCommand::Edit { operation });
                }
            }
            ModelResult::Answer { .. } => {}
        }

        // Discovery: context requests and repository searches.
        for request in structured.parse_context_requests(raw_text) {
            commands.push(LlmCommand::ContextRequest {
                path: request.path,
                access: request.access,
            });
        }
        for search in structured.parse_repo_searches(raw_text) {
            commands.push(LlmCommand::RepoSearch {
                path: search.path,
                regex: search.regex,
            });
        }

        // Repository outline: the compact directory tree and targeted symbol
        // maps decoded from the structured `repo_dirs`/`repo_maps` fields.
        if structured.parse_repo_dirs(raw_text) {
            commands.push(LlmCommand::RepoDirs);
        }
        for path in structured.parse_repo_maps(raw_text) {
            commands.push(LlmCommand::RepoMap { path });
        }

        // Directory content listings decoded from the structured `dir_lists`
        // field, falling back to `<dir_list>` control tags.
        for request in structured.parse_dir_lists(raw_text) {
            commands.push(LlmCommand::DirList {
                path: request.path,
                recursive: request.recursive,
            });
        }

        // Toolchain verification decoded from the structured `verify` field.
        if let Some(request) = structured.parse_verify_request(raw_text) {
            commands.push(LlmCommand::Verify {
                target: map_verify_target(request.target),
                on_success: map_verify_on_success(request.on_success),
            });
        }

        // Detected user language decoded from the structured `user_language` field.
        if let Some(language) = structured.parse_user_language(raw_text) {
            commands.push(LlmCommand::UserLanguage { language });
        }

        // Documentation topics decoded from the structured `doc_requests` field.
        for topic in structured.parse_doc_requests(raw_text) {
            commands.push(LlmCommand::DocRequest { topic });
        }

        // Ephemeral working memory decoded from the structured `scratchpad` field.
        if let Some(notes) = structured.parse_scratchpad(raw_text) {
            commands.push(LlmCommand::Scratchpad { notes });
        }

        // Terminal outcome decoded from the structured `outcome` field. A
        // blocker is decoded through `parse_blocked` below so it is never
        // reported twice.
        if let Some(outcome) = structured.parse_terminal_outcome(raw_text) {
            match outcome {
                TerminalOutcome::Success { summary } => {
                    commands.push(LlmCommand::Outcome {
                        outcome: LlmOutcome::Success { summary },
                    });
                }
                TerminalOutcome::Error { message } => {
                    commands.push(LlmCommand::Outcome {
                        outcome: LlmOutcome::Error { message },
                    });
                }
                TerminalOutcome::Skip { reason } => {
                    commands.push(LlmCommand::Outcome {
                        outcome: LlmOutcome::Skip { reason },
                    });
                }
                TerminalOutcome::Blocked { .. } => {}
            }
        }

        // Semantic blocker decoded from the structured `blocked` field.
        if let Some((reason, details)) = structured.parse_blocked(raw_text) {
            commands.push(LlmCommand::Blocked { reason, details });
        }

        // Plan creation or modification decoded from the structured `plan_update`
        // field, falling back to `<plan>` control tags.
        for update in structured.parse_plan_updates(raw_text) {
            commands.push(LlmCommand::Plan { update });
        }

        // Narrowing the capability set must also block decoding, so a disabled
        // capability can never reach the execution pipeline.
        commands.retain(|command| match command.capability() {
            Some(capability) => self.capabilities.contains(capability),
            None => true,
        });
        commands
    }

    fn system_instructions(&self, editable_paths: &[String], _has_eager_repo_map: bool) -> String {
        match &self.mode {
            JsonPromptMode::Edits => {
                // Structured Output always exposes the `repo_maps` schema field,
                // so its reference describes the field regardless of the outline.
                StructuredEditProtocol.system_instructions_for(&self.capabilities, editable_paths)
            }
            JsonPromptMode::Discussion { stream_field } => {
                if stream_field == "message" {
                    "You are operating in Structured Discussion mode. Formulate all responses as a valid JSON object matching the discussion_response schema:\n\
                    - message: Conversational Markdown explanation, reasoning, and answers. This field is streamed directly to the user interface.\n\
                    - plan_update: null (unless the user explicitly instructed to create or modify a plan)\n\
                    - context_requests: list of { path, access } files to add to context\n\
                    - repo_searches: list of { path, regex } search queries using Rust / ripgrep regex syntax (supports \\b, \\w, \\d, (?:...), (?i); no lookarounds or backreferences)\n\
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
        None
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
            JsonPromptMode::Edits => {
                StructuredEditProtocol.response_format_for(&self.capabilities, editable_paths)
            }
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
            JsonPromptMode::Edits => Box::new(
                JsonStreamFilter::new(editable_paths, repo_root)
                    .with_staged_contents(staged_contents),
            ),
            JsonPromptMode::Discussion { stream_field } => Box::new(
                DiscussionStreamFilter::with_target_field(stream_field.clone()),
            ),
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
            LlmCapability::DocRequest,
            LlmCapability::ToolchainVerify,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ] {
            assert!(protocol.supports(capability));
        }
        assert!(!protocol.supports(LlmCapability::PlanCreation));
        assert!(!protocol.supports(LlmCapability::PlanUpdate));
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
    fn test_decode_invalid_structured_change_emits_protocol_error() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Bad change",
            "changes": [
                {
                    "op": "unknown_op",
                    "path": "src/lib.rs",
                    "to": "",
                    "old_text": "",
                    "new_text": "",
                    "content": ""
                }
            ],
            "context_requests": [],
            "suggested_actions": []
        }"#;
        let commands = protocol.decode(output);
        let has_error = commands
            .iter()
            .any(|cmd| matches!(cmd, LlmCommand::ProtocolError { .. }));
        assert!(
            has_error,
            "Invalid structured change should produce ProtocolError command"
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
    fn test_decode_repo_outline_requests() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message":"Outlining",
            "changes":[],
            "repo_dirs":true,
            "repo_maps":["./crates/core/","."],
            "suggested_actions":[]
        }"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::RepoDirs));
        assert!(commands.contains(&LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::RepoMap {
            path: String::new(),
        }));

        // An idle outline request decodes into no outline command at all.
        let idle = r#"{"message":"x","changes":[],"repo_dirs":false,"repo_maps":[]}"#;
        let commands = protocol.decode(idle);
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::RepoDirs | LlmCommand::RepoMap { .. })));
    }

    #[test]
    fn test_decode_dir_list_requests() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message":"Listing",
            "changes":[],
            "dir_lists":[{"path":"./crates/core/","recursive":false},{"path":".","recursive":true}]
        }"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: false,
        }));
        assert!(commands.contains(&LlmCommand::DirList {
            path: String::new(),
            recursive: true,
        }));

        // An idle listing request decodes into no directory command at all.
        let idle = r#"{"message":"x","changes":[],"dir_lists":[]}"#;
        assert!(!protocol
            .decode(idle)
            .iter()
            .any(|command| matches!(command, LlmCommand::DirList { .. })));
    }

    #[test]
    fn test_decode_dir_list_from_xml_fallback() {
        let protocol = JsonPromptProtocol::default();
        let output = "{\"message\":\"Working\",\"changes\":[]}\n\
<dir_list path=\"crates/core\" recursive=\"true\" />";
        let commands = protocol.decode(output);
        assert!(commands.contains(&LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: true,
        }));
    }

    #[test]
    fn test_decode_discovery_requests() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message":"Need more context",
            "changes":[],
            "context_requests":["src/a.rs","editable:src/b.rs"],
            "repo_searches":[{"path":"crates/core","regex":"fn execute"}],
            "suggested_actions":[]
        }"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }));
        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "src/b.rs".to_string(),
            access: ContextAccess::Editable,
        }));
        assert!(commands.contains(&LlmCommand::RepoSearch {
            path: "crates/core".to_string(),
            regex: "fn execute".to_string(),
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
    fn test_decode_verify_control_tag() {
        let protocol = JsonPromptProtocol::default();
        let output = concat!(
            "{\"message\":\"Done.\",\"changes\":[],\"context_requests\":[],\"suggested_actions\":[]}\n",
            "<verify target=\"test\" on_success=\"silent\" />\n",
        );
        let commands = protocol.decode(output);
        assert!(commands.iter().any(|cmd| matches!(
            cmd,
            LlmCommand::Verify {
                target: tauqe_protocol::VerifyTarget::Test,
                on_success: tauqe_protocol::VerifyOnSuccess::Silent,
            }
        )));
    }

    #[test]
    fn test_narrowed_capability_set_filters_decoded_commands() {
        let protocol = JsonPromptProtocol::edits()
            .with_capability_set(CapabilitySet::from_slice(&[LlmCapability::TextMessage]));
        assert_eq!(
            protocol.capability_set().as_slice(),
            &[LlmCapability::TextMessage]
        );

        let output = r#"{"message":"Need context","changes":[{"op":"create","path":"src/a.rs","to":"","old_text":"","new_text":"","content":"x"}],"context_requests":["src/b.rs"],"suggested_actions":[]}"#;
        let commands = protocol.decode(output);
        assert!(!commands.is_empty());
        assert!(commands
            .iter()
            .all(|command| matches!(command, LlmCommand::Text { .. })));
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
            "repo_searches": [
                {
                    "path": "crates/core",
                    "regex": "fn prompt"
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
        assert!(commands.contains(&LlmCommand::RepoSearch {
            path: "crates/core".to_string(),
            regex: "fn prompt".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
        let has_plan = commands.iter().any(
            |c| matches!(c, LlmCommand::Plan { update } if update.id.as_deref() == Some("p-1")),
        );
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

    #[test]
    fn test_decode_control_fields_from_structured_json() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Need the git chapter",
            "changes": [],
            "doc_requests": ["git"],
            "scratchpad": "Verified: json decoder fixed.",
            "outcome": {"status": "success", "summary": "All checks passed", "message": ""},
            "blocked": {"is_blocked": false, "reason": "", "details": ""}
        }"#;
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::DocRequest {
            topic: tauqe_protocol::DocTopic::Git,
        }));
        assert!(commands.contains(&LlmCommand::Scratchpad {
            notes: "Verified: json decoder fixed.".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                summary: Some("All checks passed".to_string()),
            },
        }));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Blocked { .. })));
    }

    #[test]
    fn test_decode_control_fields_from_xml_fallback() {
        let protocol = JsonPromptProtocol::edits();
        let output = "{\"message\":\"Working\",\"changes\":[]}\n\
<doc_request topic=\"plan\" />\n\
<scratchpad>Inspect decoder</scratchpad>\n\
<blocked reason=\"Missing table\" details=\"users table absent\" />";
        let commands = protocol.decode(output);

        assert!(commands.contains(&LlmCommand::DocRequest {
            topic: tauqe_protocol::DocTopic::Plan,
        }));
        assert!(commands.contains(&LlmCommand::Scratchpad {
            notes: "Inspect decoder".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::Blocked {
            reason: "Missing table".to_string(),
            details: Some("users table absent".to_string()),
        }));
        // A control tag that is both a blocker and a terminal outcome must be
        // reported exactly once, as a semantic blocker.
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, LlmCommand::Blocked { .. }))
                .count(),
            1
        );
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Outcome { .. })));
    }

    #[test]
    fn test_narrowed_capability_set_blocks_control_field_decoding() {
        let protocol = JsonPromptProtocol::edits()
            .with_capability_set(CapabilitySet::from_slice(&[LlmCapability::TextMessage]));
        let output = r#"{
            "message": "Only text",
            "changes": [],
            "doc_requests": ["git"],
            "scratchpad": "notes",
            "outcome": {"status": "success", "summary": "done", "message": ""},
            "blocked": {"is_blocked": true, "reason": "blocked", "details": ""}
        }"#;
        let commands = protocol.decode(output);
        assert!(!commands.is_empty());
        assert!(commands
            .iter()
            .all(|command| matches!(command, LlmCommand::Text { .. })));
    }

    #[test]
    fn test_with_capabilities_derives_mode_from_file_edits() {
        let with_edits = JsonPromptProtocol::with_capabilities(CapabilitySet::develop_structured());
        assert_eq!(with_edits.name(), "structured");
        assert!(with_edits.supports(LlmCapability::FileEdits));
        assert_eq!(
            with_edits.capability_set(),
            CapabilitySet::develop_structured()
        );

        let no_edits = JsonPromptProtocol::with_capabilities(CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanUpdate,
            LlmCapability::UserLanguage,
        ]));
        assert!(!no_edits.supports(LlmCapability::FileEdits));
        assert_eq!(
            no_edits.capability_set(),
            CapabilitySet::from_slice(&[
                LlmCapability::TextMessage,
                LlmCapability::PlanUpdate,
                LlmCapability::UserLanguage,
            ])
        );
    }

    #[test]
    fn test_decode_verify_and_user_language_from_structured_json() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Checking",
            "changes": [],
            "verify": {"target": "test", "on_success": "silent"},
            "user_language": "Russian"
        }"#;
        let commands = protocol.decode(output);
        assert!(commands.contains(&LlmCommand::Verify {
            target: tauqe_protocol::VerifyTarget::Test,
            on_success: tauqe_protocol::VerifyOnSuccess::Silent,
        }));
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
    }

    #[test]
    fn test_decode_plan_update_from_structured_json() {
        let protocol = JsonPromptProtocol::discussion();
        let output = r#"{
            "message": "Plan refined",
            "plan_update": {"action": "save", "id": "p-9", "title": "P9"}
        }"#;
        let commands = protocol.decode(output);
        let plan = commands
            .iter()
            .find_map(|command| match command {
                LlmCommand::Plan { update } => Some(update),
                _ => None,
            })
            .expect("plan command");
        assert_eq!(plan.id.as_deref(), Some("p-9"));
        assert_eq!(plan.action, tauqe_protocol::DiscussionPlanAction::Save);
    }

    #[test]
    fn test_capability_switch_filters_unadvertised_commands() {
        let protocol =
            JsonPromptProtocol::edits().with_capability_set(CapabilitySet::from_slice(&[
                LlmCapability::TextMessage,
                LlmCapability::ToolchainVerify,
            ]));
        let output = r#"{
            "message": "x",
            "changes": [{"op": "create", "path": "a.rs", "to": "", "old_text": "", "new_text": "", "content": "fn x() {}"}],
            "verify": {"target": "check", "on_success": "report"}
        }"#;
        let commands = protocol.decode(output);
        assert!(commands
            .iter()
            .any(|c| matches!(c, LlmCommand::Verify { .. })));
        assert!(!commands
            .iter()
            .any(|c| matches!(c, LlmCommand::Edit { .. })));
    }
}
