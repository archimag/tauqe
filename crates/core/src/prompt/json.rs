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
use tauqe_protocol::{
    CapabilitySet, EditProtocolKind, LlmCapability, LlmCommand, LlmOutcome, ModelResult,
    PatchStrategy,
};

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
    ///
    /// The transport contributes the encoding only: the advertised surface is
    /// the intent-oriented interactive REPL profile, so a `structured` project
    /// and an `xml` project advertise the very same operational surface.
    pub fn edits() -> Self {
        Self {
            mode: JsonPromptMode::Edits,
            capabilities: CapabilitySet::interactive_repl(),
        }
    }

    /// Creates a structured protocol configured for discussion mode with the
    /// standard `"message"` streaming field.
    ///
    /// The operating mode is derived from the discussion capability surface
    /// rather than declared here, so the mode is a property of the surface the
    /// turn advertises: a surface that permits file edits runs in edit mode and
    /// composes the registry schema, where `plan_update` is already declared.
    pub fn discussion() -> Self {
        let capabilities = CapabilitySet::discussion();
        let mode = Self::mode_for(&capabilities);
        Self { mode, capabilities }
    }

    /// Creates a structured protocol configured for discussion mode streaming
    /// an arbitrary top-level JSON string field.
    ///
    /// The mode follows the same surface derivation as [`Self::discussion`]: the
    /// field names the conversational channel only while the surface carries no
    /// file edits.
    pub fn with_stream_field(stream_field: impl Into<String>) -> Self {
        let stream_field = stream_field.into();
        let capabilities = CapabilitySet::discussion();
        let mode = Self::mode_for_field(&capabilities, &stream_field);
        Self { mode, capabilities }
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
        Self::mode_for_field(capabilities, "message")
    }

    /// Derives the operating mode against a named conversational channel.
    ///
    /// The channel belongs to the conversational mode only: a surface that
    /// permits file edits composes the edit schema, so the requested field name
    /// never applies to it.
    fn mode_for_field(capabilities: &CapabilitySet, stream_field: &str) -> JsonPromptMode {
        if capabilities.contains(LlmCapability::FileEdits) {
            JsonPromptMode::Edits
        } else {
            JsonPromptMode::Discussion {
                stream_field: stream_field.to_string(),
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

    fn kind(&self) -> EditProtocolKind {
        EditProtocolKind::Structured
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

    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand> {
        let structured = StructuredEditProtocol;
        let mut commands = Vec::new();

        // Conversational text with structured JSON markup removed. This path
        // serves every mode because each structured response carries `message`.
        let text = structured.clean_assistant_text(raw_text);
        if !text.trim().is_empty() {
            commands.push(LlmCommand::Text { content: text });
        }

        // File edit operations decoded from the structured `changes` array.
        match structured.parse_output(raw_text, context_paths) {
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

        // Unconditional abort decoded from the structured `abort` field,
        // falling back to `<abort>` control tags.
        if let Some((reason, recommendation)) = structured.parse_abort(raw_text) {
            commands.push(LlmCommand::Abort {
                reason,
                recommendation,
            });
        }

        // Plan creation or modification decoded from the structured `plan_update`
        // field, falling back to `<plan>` control tags when the field is absent.
        // A payload the DTO cannot hold is reported as a protocol error instead
        // of being silently dropped, so the bounded turn can repeat generation
        // with the decoding diagnostics attached.
        match structured.parse_plan_updates(raw_text) {
            Ok(updates) => {
                for update in updates {
                    commands.push(LlmCommand::Plan { update });
                }
            }
            Err(message) => commands.push(LlmCommand::ProtocolError { message }),
        }

        // Typed project command invocations decoded from the structured
        // `custom_tools` field, falling back to `<custom_tool>` control tags.
        for (name, args) in structured.parse_custom_tools(raw_text) {
            commands.push(LlmCommand::CustomTool { name, args });
        }

        // Narrowing the capability set must also block decoding, so a disabled
        // capability can never reach the execution pipeline.
        commands.retain(|command| match command.capability() {
            Some(capability) => self.capabilities.contains(capability),
            None => true,
        });
        commands
    }

    fn system_instructions(
        &self,
        editable_paths: &[String],
        _has_eager_repo_map: bool,
        patch_strategy: PatchStrategy,
    ) -> String {
        // The editable paths travel through the protocol contract, so the
        // parameter is bound explicitly here.
        let _ = editable_paths;
        match &self.mode {
            JsonPromptMode::Edits => {
                // Structured Output always exposes the `repo_maps` schema field,
                // so its reference describes the field regardless of the outline.
                StructuredEditProtocol.system_instructions_for(
                    &self.capabilities,
                    editable_paths,
                    patch_strategy,
                )
            }
            JsonPromptMode::Discussion { stream_field } => {
                // A discussion turn edits no file, so the editable paths travel
                // through the protocol contract without reaching a change-set
                // reference: binding them here keeps the parameter consumed on
                // every operating mode of the transport.
                let _ = editable_paths;
                if stream_field == "message" {
                    "## Mode: Structured Discussion\nYou are operating in Structured Discussion mode. Formulate all responses as a valid JSON object matching the discussion_response schema:\n\
                    - message: Conversational Markdown explanation, reasoning, and answers. This field is streamed directly to the user interface.\n\
                    - plan_update: null (unless the user explicitly instructed to create or modify a plan)\n\
                    - context_requests: list of { path, access } files to add to context\n\
                    - repo_searches: list of { path, regex } search queries using Rust / ripgrep regex syntax (supports \\b, \\w, \\d, (?:...), (?i); no lookarounds or backreferences)\n\
                    - user_language: detected primary natural language of user prompt (e.g. Russian, English)\n\
                    Output ONLY raw valid JSON matching this schema, without any outer markdown fences or markdown wrapping.\n"
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
        patch_strategy: PatchStrategy,
    ) -> Option<crate::providers::ResponseFormat> {
        // The response schema is a projection of the enabled capability set and
        // the declared patch strategy, never of the files a turn may address:
        // the field names are fixed before any file enters the context, so the
        // permitted paths travel through the protocol contract without reaching
        // the schema or the discussion payload.
        let _ = editable_paths;
        match &self.mode {
            JsonPromptMode::Edits => {
                StructuredEditProtocol.response_format_for(&self.capabilities, patch_strategy)
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
            LlmCapability::PlanCreation,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::Abort,
            LlmCapability::UserLanguage,
            LlmCapability::CustomTools,
        ] {
            assert!(protocol.supports(capability));
        }
        // Plan mutation stays a matter of the plan focus, never of the
        // transport, so it is absent exactly as in the XML profile.
        assert!(!protocol.supports(LlmCapability::PlanUpdate));
        assert_eq!(protocol.capability_set(), CapabilitySet::interactive_repl());
    }

    #[test]
    fn test_decode_answer_without_changes() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{"message":"Hello there! How can I help you?","changes":[],"context_requests":[],"context_drops":[],"suggested_actions":[]}"#;
        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);

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
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::RepoDirs));
        assert!(commands.contains(&LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::RepoMap {
            path: String::new(),
        }));

        // An idle outline request decodes into no outline command at all.
        let idle = r#"{"message":"x","changes":[],"repo_dirs":false,"repo_maps":[]}"#;
        let commands = protocol.decode(idle, &[]);
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
        let commands = protocol.decode(output, &[]);

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
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::DirList { .. })));
    }

    #[test]
    fn test_decode_dir_list_from_xml_fallback() {
        let protocol = JsonPromptProtocol::default();
        let output = "{\"message\":\"Working\",\"changes\":[]}\n\
<dir_list path=\"crates/core\" recursive=\"true\" />";
        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);

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
        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);
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
        // A plan discussion carries the full dialogue surface, so its transport
        // derives the edit mode and composes the composite registry schema.
        assert!(protocol.supports(LlmCapability::FileEdits));
        assert!(protocol.supports(LlmCapability::ToolchainVerify));
        assert_eq!(protocol.mode(), &JsonPromptMode::Edits);
    }

    #[test]
    fn test_mode_is_derived_from_the_capability_surface() {
        // A surface that permits file edits always runs in edit mode, so widening
        // a discussion surface switches the transport instead of keeping a
        // conversational schema that would reject the change set.
        let edits = CapabilitySet::interactive_repl();
        assert!(edits.contains(LlmCapability::FileEdits));
        assert_eq!(JsonPromptProtocol::mode_for(&edits), JsonPromptMode::Edits);
        assert_eq!(
            JsonPromptProtocol::mode_for_field(&edits, "summary"),
            JsonPromptMode::Edits
        );

        // A surface without file edits keeps the conversational channel, and a
        // custom field name is preserved for it.
        let text_only = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanUpdate,
            LlmCapability::UserLanguage,
        ]);
        assert!(!text_only.contains(LlmCapability::FileEdits));
        assert_eq!(
            JsonPromptProtocol::mode_for(&text_only),
            JsonPromptMode::Discussion {
                stream_field: "message".to_string()
            }
        );
        assert_eq!(
            JsonPromptProtocol::mode_for_field(&text_only, "summary"),
            JsonPromptMode::Discussion {
                stream_field: "summary".to_string()
            }
        );
    }

    #[test]
    fn test_discussion_constructors_derive_the_mode_from_their_surface() {
        // The constructors no longer hardcode the conversational mode: they hand
        // their surface to the same derivation the capability-set path uses.
        let discussion = JsonPromptProtocol::discussion();
        assert_eq!(discussion.capability_set(), CapabilitySet::discussion());
        assert_eq!(
            discussion.mode(),
            &JsonPromptProtocol::mode_for(&CapabilitySet::discussion())
        );

        let custom = JsonPromptProtocol::with_stream_field("summary");
        assert_eq!(custom.capability_set(), CapabilitySet::discussion());
        assert_eq!(
            custom.mode(),
            &JsonPromptProtocol::mode_for_field(&CapabilitySet::discussion(), "summary")
        );
    }

    #[test]
    fn test_file_edits_surface_composes_the_registry_edit_schema() {
        // The mode is a projection of the surface: a surface that permits file
        // edits runs in edit mode, so its response schema is the registry
        // composition, where `plan_update` is already declared and the change
        // set carries its own property. The plan payload therefore keeps a
        // single source of truth across both discussions.
        let widened = JsonPromptProtocol::with_capabilities(CapabilitySet::interactive_repl());
        let format = widened
            .response_format(&[], PatchStrategy::SearchReplace)
            .expect("edit mode always carries a response schema");
        let crate::providers::ResponseFormat::JsonSchema { json_schema } = format else {
            panic!("edit mode composes a JSON schema");
        };
        let schema = &json_schema.schema;
        assert!(schema["properties"].get("changes").is_some());
        assert!(schema["properties"].get("plan_update").is_some());
        assert!(schema["properties"].get("context_requests").is_some());
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_conversational_surface_composes_the_discussion_schema() {
        // The complementary branch: without file edits the derived mode streams
        // the `message` field and answers with the discussion schema.
        let text_only = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Discovery,
            LlmCapability::UserLanguage,
        ]);
        let protocol = JsonPromptProtocol::with_capabilities(text_only);
        let format = protocol
            .response_format(&[], PatchStrategy::SearchReplace)
            .expect("discussion mode always carries a response schema");
        let crate::providers::ResponseFormat::JsonSchema { json_schema } = format else {
            panic!("discussion mode composes a JSON schema");
        };
        assert_eq!(json_schema.name, "discussion_response");
        assert!(json_schema.schema["properties"].get("changes").is_none());
    }

    #[test]
    fn test_discussion_instructions_own_the_mode_section() {
        // A structured discussion is a surface that edits no file, so it streams
        // the conversational channel and renders the discussion section itself.
        let conversational = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Discovery,
            LlmCapability::UserLanguage,
        ]);
        let protocol = JsonPromptProtocol::with_capabilities(conversational);
        let instructions = protocol.system_instructions(&[], false, PatchStrategy::SearchReplace);

        // The protocol renders the discussion section itself, with the same
        // `##`-section structure the edit reference uses.
        assert!(instructions.starts_with("## Mode: Structured Discussion\n"));
        assert!(instructions.contains("matching the discussion_response schema"));
        assert!(instructions.contains("- message: Conversational Markdown explanation"));
        assert!(instructions.contains("- plan_update: null"));
        assert!(instructions.ends_with("markdown wrapping.\n"));
        assert!(!instructions.contains("Code Modification Protocol"));
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

        let commands = protocol.decode(output, &[]);
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
        let commands = protocol.decode(output, &[]);

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
    fn test_decode_abort_from_structured_json_and_xml_fallback() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "The native build needs approval.",
            "changes": [],
            "abort": {
                "is_aborted": true,
                "reason": "Native build approval required",
                "recommendation": "pnpm approve-builds"
            }
        }"#;
        assert!(protocol.decode(output, &[]).contains(&LlmCommand::Abort {
            reason: "Native build approval required".to_string(),
            recommendation: "pnpm approve-builds".to_string(),
        }));

        // An idle abort field reports nothing at all.
        let idle = r#"{
            "message": "x",
            "changes": [],
            "abort": {"is_aborted": false, "reason": "", "recommendation": ""}
        }"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::Abort { .. })));

        // Control-tag fallback keeps both transports decoding one payload.
        let fallback = "{\"message\":\"Working\",\"changes\":[]}\n\
<abort reason=\"Toolchain missing\" recommendation=\"rustup component add clippy\" />";
        assert!(protocol.decode(fallback, &[]).contains(&LlmCommand::Abort {
            reason: "Toolchain missing".to_string(),
            recommendation: "rustup component add clippy".to_string(),
        }));
    }

    #[test]
    fn test_decode_custom_tool_from_structured_json_and_xml_fallback() {
        let protocol =
            JsonPromptProtocol::edits().with_capability_set(CapabilitySet::from_slice(&[
                LlmCapability::TextMessage,
                LlmCapability::CustomTools,
            ]));
        let output = r#"{
            "message": "Running the declared probe.",
            "custom_tools": [{"name": "probe", "args": {"n": "10"}}]
        }"#;
        assert!(protocol
            .decode(output, &[])
            .contains(&LlmCommand::CustomTool {
                name: "probe".to_string(),
                args: serde_json::json!({ "n": "10" }),
            }));

        // An empty array requests no execution.
        let idle = r#"{"message": "x", "custom_tools": []}"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::CustomTool { .. })));

        // Control-tag fallback decodes the same invocation shape.
        let fallback = "{\"message\":\"Working\"}\n<custom_tool name=\"probe\" n=\"10\" />";
        assert!(protocol
            .decode(fallback, &[])
            .contains(&LlmCommand::CustomTool {
                name: "probe".to_string(),
                args: serde_json::json!({ "n": "10" }),
            }));
    }

    #[test]
    fn test_decode_control_fields_from_xml_fallback() {
        let protocol = JsonPromptProtocol::edits();
        let output = "{\"message\":\"Working\",\"changes\":[]}\n\
<doc_request topic=\"plan\" />\n\
<scratchpad>Inspect decoder</scratchpad>\n\
<blocked reason=\"Missing table\" details=\"users table absent\" />";
        let commands = protocol.decode(output, &[]);

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
        let commands = protocol.decode(output, &[]);
        assert!(!commands.is_empty());
        assert!(commands
            .iter()
            .all(|command| matches!(command, LlmCommand::Text { .. })));
    }

    #[test]
    fn test_with_capabilities_derives_mode_from_file_edits() {
        let with_edits = JsonPromptProtocol::with_capabilities(CapabilitySet::interactive_repl());
        assert_eq!(with_edits.name(), "structured");
        assert!(with_edits.supports(LlmCapability::FileEdits));
        assert_eq!(
            with_edits.capability_set(),
            CapabilitySet::interactive_repl()
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
        let commands = protocol.decode(output, &[]);
        assert!(commands.contains(&LlmCommand::Verify {
            target: tauqe_protocol::VerifyTarget::Test,
            on_success: tauqe_protocol::VerifyOnSuccess::Silent,
        }));
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
    }

    #[test]
    fn test_decode_uses_the_context_paths_for_resolution_and_membership() {
        let protocol = JsonPromptProtocol::edits();
        let context_paths = vec!["crates/core/src/lib.rs".to_string()];

        // A unique suffix of a context file resolves to the context path.
        let suffix = r#"{
            "message": "Touch lib",
            "changes": [{"op": "replace", "path": "lib.rs", "to": "", "old_text": "old();", "new_text": "new();", "content": ""}]
        }"#;
        let operation = protocol
            .decode(suffix, &context_paths)
            .into_iter()
            .find_map(|command| match command {
                LlmCommand::Edit { operation } => Some(operation),
                _ => None,
            })
            .expect("decoded edit");
        match operation {
            EditOperation::Replace { path, .. } => {
                assert_eq!(path, "crates/core/src/lib.rs");
            }
            other => panic!("unexpected operation: {other:?}"),
        }

        // A path outside the context is rejected at the decode boundary instead
        // of being deferred to staging.
        let outside = r#"{
            "message": "Touch other",
            "changes": [{"op": "replace", "path": "crates/core/src/other.rs", "to": "", "old_text": "a", "new_text": "b", "content": ""}]
        }"#;
        let commands = protocol.decode(outside, &context_paths);
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Edit { .. })));
    }

    #[test]
    fn test_decode_plan_update_from_structured_json() {
        let protocol = JsonPromptProtocol::discussion();
        let output = r#"{
            "message": "Plan refined",
            "plan_update": {"action": "save", "id": "p-9", "title": "P9"}
        }"#;
        let commands = protocol.decode(output, &[]);
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
    fn test_decode_malformed_plan_update_emits_protocol_error() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Refining",
            "changes": [],
            "plan_update": {"action": "save", "items": "not-a-list"}
        }"#;
        let commands = protocol.decode(output, &[]);

        // The decoding boundary reports the defect as a protocol error, which is
        // what lets the bounded turn repeat generation instead of silently
        // recording an answer whose plan update vanished.
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Plan { .. })));
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
        let commands = protocol.decode(output, &[]);
        assert!(commands
            .iter()
            .any(|c| matches!(c, LlmCommand::Verify { .. })));
        assert!(!commands
            .iter()
            .any(|c| matches!(c, LlmCommand::Edit { .. })));
    }

    #[test]
    fn test_decode_silent_structured_round_emits_no_text() {
        // A round whose only conversational channel is empty must not surface
        // its JSON scaffolding as text: the REPL streamed nothing for it, and
        // session history must record the same nothing (plus phase banners).
        let protocol = JsonPromptProtocol::edits();
        let output =
            r#"{"message":"","changes":[],"verify":{"target":"none","on_success":"silent"}}"#;
        let commands = protocol.decode(output, &[]);
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, LlmCommand::Text { .. })),
            "a silent structured round must never become raw JSON text"
        );
    }
    #[test]
    fn test_decode_payload_less_success_reports_success_without_a_result() {
        // A terminal `success` states what the turn accomplished through its
        // summary and nothing else, so an empty summary decodes into a success
        // that carries no payload. The bounded turn can then re-ask the model
        // instead of recording a silent conclusion.
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "",
            "changes": [],
            "outcome": {"status": "success", "summary": "  ", "message": "", "reason": ""}
        }"#;
        let commands = protocol.decode(output, &[]);
        assert_eq!(
            commands,
            vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Success { summary: None },
            }],
            "a payload-free success must decode as a success without a result"
        );
    }

    #[test]
    fn test_decode_empty_object_emits_no_commands() {
        // An empty object is the smallest silent structured round: it carries no
        // conversational channel and no actionable field, so the decoded surface
        // stays empty and never degrades into raw JSON text.
        let protocol = JsonPromptProtocol::edits();
        let commands = protocol.decode("{}", &[]);
        assert!(
            commands.is_empty(),
            "an empty structured round must decode into no command at all: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, LlmCommand::Text { .. })),
            "an empty structured round must never become raw JSON text"
        );
    }

    /// Returns the terminal-outcome commands carried by a decoded round.
    ///
    /// A round may decode other commands besides its outcome: the `<skip>` tag
    /// deliberately survives conversational cleaning, because the review contour
    /// reads it back from the cleaned assistant text to record why a finding was
    /// skipped. Transport parity is therefore asserted on the decoded outcome
    /// payload, which is what the bounded turn inspects.
    fn decoded_outcomes(commands: &[LlmCommand]) -> Vec<LlmOutcome> {
        commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::Outcome { outcome } => Some(outcome.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_decode_terminal_outcomes_always_carry_their_payload() {
        // Nobody but the model can state what a turn accomplished, so `success`
        // without a summary has no deterministic completion: it stays a success
        // without a result. `error` and `skip` are completed deterministically,
        // and both contours decode one and the same outcome for every status.
        let protocol = JsonPromptProtocol::edits();

        let bare_success = LlmOutcome::Success { summary: None };
        let structured_success = r#"{
            "message": "",
            "changes": [],
            "outcome": {"status": "success", "summary": "", "message": "", "reason": ""}
        }"#;
        assert_eq!(
            decoded_outcomes(&protocol.decode(structured_success, &[])),
            vec![bare_success.clone()]
        );
        assert_eq!(
            decoded_outcomes(&protocol.decode("<success />\n", &[])),
            vec![bare_success]
        );

        let execution_error = LlmOutcome::Error {
            message: "Execution error".to_string(),
        };
        let structured_error = r#"{
            "message": "",
            "changes": [],
            "outcome": {"status": "error", "summary": "", "message": "", "reason": ""}
        }"#;
        assert_eq!(
            decoded_outcomes(&protocol.decode(structured_error, &[])),
            vec![execution_error.clone()]
        );
        assert_eq!(
            decoded_outcomes(&protocol.decode("<error />\n", &[])),
            vec![execution_error]
        );

        let unstated_skip = LlmOutcome::Skip {
            reason: "Step skipped without a stated reason".to_string(),
        };
        let structured_skip = r#"{
            "message": "",
            "changes": [],
            "outcome": {"status": "skip", "summary": "", "message": "", "reason": "  "}
        }"#;
        assert_eq!(
            decoded_outcomes(&protocol.decode(structured_skip, &[])),
            vec![unstated_skip.clone()]
        );
        assert_eq!(
            decoded_outcomes(&protocol.decode("<skip />\n", &[])),
            vec![unstated_skip]
        );
    }
}
