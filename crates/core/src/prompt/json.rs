//! JSON Schema (Structured Output) prompt protocol.
//!
//! [`JsonPromptProtocol`] decodes Structured Output (JSON schema) model output
//! into the normalized [`LlmCommand`] vocabulary consumed by the execution
//! pipeline. It isolates the JSON Schema wire syntax from the core business
//! logic and delegates concrete parsing to the structured edit protocol so the
//! schema keeps a single source of truth.

use std::borrow::Cow;

use crate::capabilities::{
    capability_expr_schema, compose_structured_protocol_instructions, decode_capability_expr,
    PromptRenderContext,
};
use crate::edits::protocol::structured::{
    resolve_decoded_edits, StructuredEditProtocol, StructuredPayload,
};
use crate::edits::stream::discussion::DiscussionStreamFilter;
use crate::edits::stream::JsonStreamFilter;
use tauqe_protocol::{
    CapabilityExpr, EditProtocolKind, LlmCapability, LlmCommand, PatchStrategy, ProjectConfig,
    ShortDescriptionCommand, TextCommand,
};

use super::protocol::PromptProtocol;
use crate::control::repl::dialogue_capability_expr;
use crate::control::turn::turn_capability_expr;

/// Operating mode of the structured transport.
///
/// The mode is derived from the capability surface: a surface that permits file
/// edits runs the edit transport, while a surface that edits nothing runs the
/// conversational transport and streams the single field it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPromptMode {
    /// Edit transport, selected by a surface that permits file modifications.
    Edits,
    /// Conversational transport streaming `stream_field` to the interface.
    Discussion { stream_field: String },
}

/// Structured Output (JSON Schema) prompt protocol.
pub struct JsonPromptProtocol {
    mode: JsonPromptMode,
    capabilities: CapabilityExpr,
}

impl Default for JsonPromptProtocol {
    fn default() -> Self {
        Self::with_capabilities(dialogue_capability_expr(false, false))
    }
}

impl JsonPromptProtocol {
    /// Protocol of an isolated step turn, whose base surface is the turn contour.
    pub fn edits() -> Self {
        Self::with_capabilities(turn_capability_expr())
    }

    /// Protocol of an interactive dialogue that authors and amends plans.
    pub fn discussion() -> Self {
        Self::with_capabilities(dialogue_capability_expr(true, false))
    }

    /// Dialogue protocol that streams a custom response field.
    pub fn with_stream_field(stream_field: &str) -> Self {
        let capabilities = dialogue_capability_expr(true, false);
        let mode = Self::mode_for_field(&capabilities, stream_field);
        Self { mode, capabilities }
    }

    /// Protocol carrying `capabilities`, with the mode derived from them.
    pub fn with_capabilities(capabilities: CapabilityExpr) -> Self {
        let mode = Self::mode_for(&capabilities);
        Self { mode, capabilities }
    }

    /// Active operating mode of this transport.
    pub fn mode(&self) -> &JsonPromptMode {
        &self.mode
    }

    /// Derives the mode of `capabilities` for the default `message` channel.
    fn mode_for(capabilities: &CapabilityExpr) -> JsonPromptMode {
        Self::mode_for_field(capabilities, "message")
    }

    /// Derives the mode of `capabilities` for the given streamed field.
    fn mode_for_field(capabilities: &CapabilityExpr, stream_field: &str) -> JsonPromptMode {
        if has_any_edit(capabilities) {
            JsonPromptMode::Edits
        } else {
            JsonPromptMode::Discussion {
                stream_field: stream_field.to_string(),
            }
        }
    }
}

impl PromptProtocol for JsonPromptProtocol {
    fn name(&self) -> &'static str {
        "structured"
    }

    fn kind(&self) -> EditProtocolKind {
        EditProtocolKind::Structured
    }

    fn capability_expr(&self) -> CapabilityExpr {
        self.capabilities.clone()
    }

    fn with_capability_expr(&self, expr: CapabilityExpr) -> Box<dyn PromptProtocol> {
        let mode = Self::mode_for(&expr);
        Box::new(Self {
            mode,
            capabilities: expr,
        })
    }

    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand> {
        // A structured round is one JSON object, and every command below is
        // read from this single decoding instead of re-parsing the response
        // once per field.
        let payload = StructuredPayload::parse(raw_text);
        let mut commands = Vec::new();

        // Conversational text with structured JSON markup removed. The channel
        // is declared inside the operational branch a round selects, so a
        // dialogue answers with the full text message while a step, which owns
        // no `TextMessage` leaf, reports through its short description. One
        // response therefore yields exactly one conversational command.
        let text = payload.clean_text();
        if !text.trim().is_empty() {
            if self.capabilities.contains(LlmCapability::TextMessage) {
                commands.push(LlmCommand::Text(TextCommand { content: text }));
            } else if self.capabilities.contains(LlmCapability::ShortDescription) {
                commands.push(LlmCommand::ShortDescription(ShortDescriptionCommand {
                    content: text,
                }));
            }
        }

        // Every operational command is decoded by walking the same
        // capability tree that composed the response schema: the ambient
        // leaves and the leaves of the branch the round selected, so a
        // capability the tree does not declare is never extracted from a
        // response.
        if let Some(root) = payload.root() {
            let ctx = PromptRenderContext::new(context_paths, None);
            let mut edits = Vec::new();
            for command in decode_capability_expr(&self.capabilities, root, &ctx) {
                match command {
                    // The module decodes paths but holds no working
                    // context, so the edit batch is resolved against the
                    // context paths of this round once the walk collected it.
                    cmd if is_edit_capability(cmd.capability()) => edits.push(cmd),
                    // channel was already emitted above.
                    LlmCommand::Text(_) | LlmCommand::ShortDescription(_) => {}
                    other => commands.push(other),
                }
            }
            match resolve_decoded_edits(edits, context_paths) {
                Ok(resolved) => commands.extend(resolved),
                Err(message) => commands.push(LlmCommand::ProtocolError { message }),
            }
        }

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
            JsonPromptMode::Discussion { .. } => {
                // A discussion turn edits no file, so the editable paths still
                // reach the render context that composes the operational rules
                // of the active capability tree.
                let render = PromptRenderContext::new(editable_paths, None)
                    .with_eager_repo_map(_has_eager_repo_map)
                    .with_patch_strategy(patch_strategy);
                // The schema declares the response structure, so the section
                // carries only the operational rules of the active capability
                // tree: the very call that renders an edit surface names exactly
                // the mechanics that surface cannot express through a field, and
                // a narrowed tree never advertises a capability it dropped. No
                // streamed field earns a bullet of its own either, because the
                // schema already declares the property that carries it.
                let operational =
                    compose_structured_protocol_instructions(&self.capabilities, &render);
                let mut instructions = String::from(
                    "## Mode: Structured Discussion\nYou are operating in Structured Discussion mode. Formulate all responses as a valid JSON object matching the discussion_response schema:\n",
                );
                let schema = capability_expr_schema(&self.capabilities, &render);
                if let Ok(schema_str) = serde_json::to_string_pretty(&schema) {
                    instructions.push_str("```json\n");
                    instructions.push_str(&schema_str);
                    instructions.push_str("\n```\n\n");
                }
                if !operational.is_empty() {
                    instructions.push_str(&operational);
                    instructions.push('\n');
                }
                instructions.push_str(
                    "Output ONLY raw valid JSON matching this schema, without any outer markdown fences or markdown wrapping.\n",
                );
                instructions
            }
        }
    }

    fn response_format(
        &self,
        editable_paths: &[String],
        patch_strategy: PatchStrategy,
        project: Option<&ProjectConfig>,
    ) -> Option<crate::providers::ResponseFormat> {
        // The edit schema is a projection of the enabled capability set, the
        // declared patch strategy, and the project command declaration, never of
        // the files a turn may address: the field names are fixed before any
        // file enters the context. The permitted paths still seed the render
        // context, so one projection serves the schema and the instructions a
        // turn renders from the very same declaration.
        match &self.mode {
            JsonPromptMode::Edits => {
                let render = PromptRenderContext::new(editable_paths, None)
                    .with_project(project)
                    .with_patch_strategy(patch_strategy);
                StructuredEditProtocol.response_format_for(&self.capabilities, &render)
            }
            JsonPromptMode::Discussion { .. } => {
                let render = PromptRenderContext::new(editable_paths, None)
                    .with_project(project)
                    .with_patch_strategy(patch_strategy);
                Some(crate::providers::ResponseFormat::JsonSchema {
                    json_schema: crate::providers::JsonSchemaDefinition {
                        name: "discussion_response".to_string(),
                        description: Some(
                            "Structured response for discussion and planning modes".to_string(),
                        ),
                        strict: Some(true),
                        schema: Cow::Owned(capability_expr_schema(&self.capabilities, &render)),
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

fn is_edit_capability(capability: Option<LlmCapability>) -> bool {
    matches!(
        capability,
        Some(
            LlmCapability::EditReplace
                | LlmCapability::EditCreate
                | LlmCapability::EditDelete
                | LlmCapability::EditMove
                | LlmCapability::EditMoveDir
                | LlmCapability::EditOverwrite
        )
    )
}

fn has_any_edit(capabilities: &CapabilityExpr) -> bool {
    capabilities.contains(LlmCapability::EditReplace)
        || capabilities.contains(LlmCapability::EditCreate)
        || capabilities.contains(LlmCapability::EditDelete)
        || capabilities.contains(LlmCapability::EditMove)
        || capabilities.contains(LlmCapability::EditMoveDir)
        || capabilities.contains(LlmCapability::EditOverwrite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{
        AbortCommand, CommitMessageCommand, ContextAccess, ContextRequestCommand,
        CustomToolCommand, DirListCommand, DocRequestCommand, EditOperation, EditReplace,
        OuterDocRequestCommand, RepoDirsCommand, RepoMapCommand, RepoSearchCommand,
        ScratchpadCommand, TurnErrorCommand, TurnSkipCommand, VerifyCommand,
    };

    /// Capability tree of a dialogue contour, as the control loops declare it.
    fn dialogue(plan_update: bool, review_decision: bool) -> CapabilityExpr {
        crate::control::repl::dialogue_capability_expr(plan_update, review_decision)
    }

    /// Lifts the operational branch envelope of a composed schema into one surface.
    ///
    /// The wire keeps the operational branches mutually exclusive, so a field of
    /// a branch is addressed through the envelope that owns it. A test that reads
    /// the property of a capability reads the schema after this lift, so it names
    /// the field rather than the branch that carries it.
    fn flattened_surface(schema: &serde_json::Value) -> serde_json::Value {
        let mut objects = vec![schema];
        if let Some(branches) = schema["properties"]["action"]["anyOf"].as_array() {
            objects.extend(branches.iter());
        }
        let mut properties = serde_json::Map::new();
        let mut required: Vec<serde_json::Value> = Vec::new();
        for object in objects {
            let Some(fields) = object["properties"].as_object() else {
                continue;
            };
            for (name, field) in fields {
                if name.as_str() == "action" || name.as_str() == "kind" {
                    continue;
                }
                if properties.contains_key(name) {
                    continue;
                }
                properties.insert(name.clone(), field.clone());
                required.push(serde_json::Value::String(name.clone()));
            }
        }
        let mut flat = serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": required,
            "additionalProperties": false
        });
        if let Some(definitions) = schema.get("$defs") {
            flat["$defs"] = definitions.clone();
        }
        flat
    }

    #[test]
    fn test_name_and_capabilities() {
        let protocol = JsonPromptProtocol::default();
        assert_eq!(protocol.name(), "structured");
        for capability in [
            LlmCapability::TextMessage,
            LlmCapability::EditReplace,
            LlmCapability::ContextRequest,
            LlmCapability::DocRequest,
            LlmCapability::ToolchainVerify,
            LlmCapability::PlanCreation,
            LlmCapability::Scratchpad,
            LlmCapability::CommitMessage,
            LlmCapability::TurnError,
            LlmCapability::TurnSkip,
            LlmCapability::Blocked,
            LlmCapability::Abort,
            LlmCapability::CustomTools,
        ] {
            assert!(protocol.supports(capability));
        }
        // Plan mutation stays a matter of the plan focus, never of the
        // transport, so it is absent exactly as in the XML profile.
        assert!(!protocol.supports(LlmCapability::PlanUpdate));
        assert_eq!(protocol.capability_expr(), dialogue(false, false));
    }

    #[test]
    fn test_decode_answer_without_changes() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{"message":"Hello there! How can I help you?","action":{"kind":"answer"}}"#;
        let commands = protocol.decode(output, &[]);
        assert_eq!(
            commands,
            vec![LlmCommand::Text(TextCommand {
                content: "Hello there! How can I help you?".to_string(),
            })]
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
    fn test_decode_malformed_change_set_reports_a_protocol_error() {
        // A `content` of a wrong type deserializes no change entry, so the
        // whole structured payload fails to decode. Reading such a round as
        // an answer without edits would silently discard the change set the
        // model promised, so the defect travels through the protocol-error
        // channel where the bounded turn can retry it.
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Editing",
            "changes": [
                {
                    "op": "create",
                    "path": "src/new.rs",
                    "to": "",
                    "start": "",
                    "end": "",
                    "content": 5
                }
            ]
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(
            commands
                .iter()
                .any(|cmd| matches!(cmd, LlmCommand::ProtocolError { .. })),
            "a change the decoder cannot read must be a protocol error: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|cmd| is_edit_capability(cmd.capability())),
            "no edit reaches staging from a payload the decoder rejected"
        );
    }

    #[test]
    fn test_decode_file_edit() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message": "Fix the bug",
            "action": {
                "kind": "edit",
                "changes": [
                    {
                        "op": "replace",
                        "path": "src/lib.rs",
                        "to": "",
                        "old_text": "old();",
                        "new_text": "new();",
                        "content": ""
                    }
                ]
            }
        }"#;
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::Text(TextCommand {
            content: "Fix the bug".to_string(),
        })));

        let operation = commands
            .into_iter()
            .find_map(LlmCommand::into_edit_operation)
            .expect("edit command");
        match operation {
            EditOperation::Replace(EditReplace {
                path,
                old_text,
                new_text,
            }) => {
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
            "action":{
                "kind":"research",
                "repo_dirs":true,
                "repo_maps":["./crates/core/","."]
            }
        }"#;
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::RepoDirs(RepoDirsCommand {})));
        assert!(commands.contains(&LlmCommand::RepoMap(RepoMapCommand {
            path: "crates/core".to_string(),
        })));
        assert!(commands.contains(&LlmCommand::RepoMap(RepoMapCommand {
            path: String::new(),
        })));

        // An idle outline request decodes into no outline command at all.
        let idle =
            r#"{"message":"x","action":{"kind":"research","repo_dirs":false,"repo_maps":[]}}"#;
        let commands = protocol.decode(idle, &[]);
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::RepoDirs(_) | LlmCommand::RepoMap(_))));
    }

    #[test]
    fn test_decode_dir_list_requests() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message":"Listing",
            "action":{
                "kind":"research",
                "dir_lists":[{"path":"./crates/core/","recursive":false},{"path":".","recursive":true}]
            }
        }"#;
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::DirList(DirListCommand {
            path: "crates/core".to_string(),
            recursive: false,
        })));
        assert!(commands.contains(&LlmCommand::DirList(DirListCommand {
            path: String::new(),
            recursive: true,
        })));

        // An idle listing request decodes into no directory command at all.
        let idle = r#"{"message":"x","action":{"kind":"research","dir_lists":[]}}"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::DirList(_))));
    }

    #[test]
    fn test_decode_research_requests() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message":"Need more context",
            "action":{
                "kind":"research",
                "context_requests":["src/a.rs","editable:src/b.rs"],
                "repo_searches":[{"path":"crates/core","regex":"fn execute"}]
            }
        }"#;
        let commands = protocol.decode(output, &[]);

        assert!(
            commands.contains(&LlmCommand::ContextRequest(ContextRequestCommand {
                path: "src/a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }))
        );
        assert!(
            commands.contains(&LlmCommand::ContextRequest(ContextRequestCommand {
                path: "src/b.rs".to_string(),
                access: ContextAccess::Editable,
            }))
        );
        assert!(
            commands.contains(&LlmCommand::RepoSearch(RepoSearchCommand {
                path: "crates/core".to_string(),
                regex: "fn execute".to_string(),
            }))
        );
    }

    #[test]
    fn test_decode_outer_doc_requests_from_structured_json_and_xml_fallback() {
        let protocol = JsonPromptProtocol::default();

        // A request names the mount point of one project declaration, and a
        // blank identifier grants nothing.
        let output = r#"{
            "message": "Checking the upstream contract",
            "action": {
                "kind": "research",
                "outer_doc_requests": ["vendor/api.md", "   ", "mylib"]
            }
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(
            commands.contains(&LlmCommand::OuterDocRequest(OuterDocRequestCommand {
                id: "vendor/api.md".to_string(),
            }))
        );
        assert!(
            commands.contains(&LlmCommand::OuterDocRequest(OuterDocRequestCommand {
                id: "mylib".to_string(),
            }))
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, LlmCommand::OuterDocRequest(_)))
                .count(),
            2
        );

        // An idle array requests no document at all.
        let idle = r#"{"message":"x","action":{"kind":"research","outer_doc_requests":[]}}"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::OuterDocRequest(_))));
    }

    #[test]
    fn test_narrowed_capability_set_filters_decoded_commands() {
        let protocol = JsonPromptProtocol::edits()
            .with_capability_expr(CapabilityExpr::from_leaves([LlmCapability::TextMessage]));
        assert_eq!(
            protocol.capability_expr().leaves(),
            vec![LlmCapability::TextMessage]
        );

        let output = r#"{"message":"Need context","changes":[{"op":"create","path":"src/a.rs","to":"","old_text":"","new_text":"","content":"x"}],"context_requests":["src/b.rs"],"suggested_actions":[]}"#;
        let commands = protocol.decode(output, &[]);
        assert!(!commands.is_empty());
        assert!(commands
            .iter()
            .all(|command| matches!(command, LlmCommand::Text(_))));
    }

    #[test]
    fn test_discussion_mode_capabilities() {
        let protocol = JsonPromptProtocol::discussion();
        assert_eq!(protocol.name(), "structured");
        assert!(protocol.supports(LlmCapability::TextMessage));
        assert!(protocol.supports(LlmCapability::PlanCreation));
        assert!(protocol.supports(LlmCapability::PlanUpdate));
        assert!(protocol.supports(LlmCapability::ContextRequest));
        // A plan discussion carries the full dialogue surface, so its transport
        // derives the edit mode and composes the composite registry schema.
        assert!(protocol.supports(LlmCapability::EditReplace));
        assert_eq!(protocol.mode(), &JsonPromptMode::Edits);
    }

    #[test]
    fn test_mode_is_derived_from_the_capability_surface() {
        // A surface that permits file edits always runs in edit mode, so widening
        // a discussion surface switches the transport instead of keeping a
        // conversational schema that would reject the change set.
        let edits = dialogue(false, false);
        assert_eq!(JsonPromptProtocol::mode_for(&edits), JsonPromptMode::Edits);
        assert_eq!(
            JsonPromptProtocol::mode_for_field(&edits, "summary"),
            JsonPromptMode::Edits
        );

        // A surface without file edits keeps the conversational channel, and a
        // custom field name is preserved for it.
        let text_only =
            CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::PlanUpdate]);
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
        assert_eq!(discussion.capability_expr(), dialogue(true, false));
        assert_eq!(
            discussion.mode(),
            &JsonPromptProtocol::mode_for(&dialogue(true, false))
        );

        let custom = JsonPromptProtocol::with_stream_field("summary");
        assert_eq!(custom.capability_expr(), dialogue(true, false));
        assert_eq!(
            custom.mode(),
            &JsonPromptProtocol::mode_for_field(&dialogue(true, false), "summary")
        );
    }

    #[test]
    fn test_file_edits_surface_composes_the_registry_edit_schema() {
        // The mode is a projection of the surface: a surface that permits file
        // edits runs in edit mode, so its response schema is the registry
        // composition, where `plan_update` is already declared and the change
        // composition, where `plan_create` is already declared and the change
        // single source of truth across both discussions.
        let widened = JsonPromptProtocol::with_capabilities(dialogue(false, false));
        let format = widened
            .response_format(&[], PatchStrategy::SearchReplace, None)
            .expect("edit mode always carries a response schema");
        let crate::providers::ResponseFormat::JsonSchema { json_schema } = format else {
            panic!("edit mode composes a JSON schema");
        };
        let schema = flattened_surface(&json_schema.schema);
        assert!(schema["properties"].get("changes").is_some());
        assert!(schema["properties"].get("plan_create").is_some());
        assert!(schema["properties"].get("context_requests").is_some());
        assert!(schema["properties"].get("repo_searches").is_some());
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_conversational_surface_composes_the_discussion_schema() {
        // The complementary branch: without file edits the derived mode streams
        // the `message` field and answers with the discussion schema.
        let text_only = CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::ContextRequest,
        ]);
        let protocol = JsonPromptProtocol::with_capabilities(text_only);
        let format = protocol
            .response_format(&[], PatchStrategy::SearchReplace, None)
            .expect("discussion mode composes a response schema");
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
        let conversational = CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::ContextRequest,
        ]);
        let protocol = JsonPromptProtocol::with_capabilities(conversational);
        let instructions = protocol.system_instructions(&[], false, PatchStrategy::SearchReplace);

        // The protocol renders the discussion section itself, with the same
        // `##`-section structure the edit reference uses.
        assert!(instructions.starts_with("## Mode: Structured Discussion\n"));
        assert!(instructions.contains("matching the discussion_response schema"));
        // The response structure belongs to the schema alone, so the section
        // names no field of the surface it describes.
        for field in ["message", "plan_create", "plan_update", "context_requests"] {
            assert!(
                !instructions.contains(&format!("- `{field}`:")),
                "the discussion section restates the schema field `{field}`"
            );
        }
        assert!(instructions.ends_with("markdown wrapping.\n"));
        assert!(!instructions.contains("Code Modification Protocol"));
    }

    #[test]
    fn test_discussion_instructions_follow_the_active_capability_tree() {
        // Narrowing the surface removes a property from the schema, so the
        // rendered reference must stop naming it as well: the text and the wire
        // stay one projection of the same tree.
        let without_research = CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
        ]);
        let narrowed = JsonPromptProtocol::with_capabilities(without_research).system_instructions(
            &[],
            false,
            PatchStrategy::SearchReplace,
        );
        assert!(narrowed.starts_with("## Mode: Structured Discussion\n"));
        assert!(!narrowed.contains("`plan_create`:"));
        assert!(!narrowed.contains("`plan_update`:"));
        assert!(!narrowed.contains("`context_requests`"));
        assert!(!narrowed.contains("`repo_searches`"));

        // A surface that edits no file states no change-set mechanics either.
        let text_only = CapabilityExpr::from_leaves([LlmCapability::TextMessage]);
        let bare = JsonPromptProtocol::with_capabilities(text_only).system_instructions(
            &[],
            false,
            PatchStrategy::SearchReplace,
        );
        assert!(bare.starts_with("## Mode: Structured Discussion\n"));
        assert!(!bare.contains("`changes`"));
        assert!(!bare.contains("`plan_update`"));
    }

    #[test]
    fn test_decode_discussion_response() {
        let protocol = JsonPromptProtocol::discussion();
        let plan_output = r#"{
            "message": "Let us refine the concept.",
            "action": {
                "kind": "plan",
                "plan_action": {
                    "kind": "modify",
                    "plan_update": {
                        "action": "update",
                        "id": "p-1",
                        "title": "Refined Plan",
                        "description": "Updated",
                        "concept": [],
                        "items": []
                    }
                }
            }
        }"#;

        let commands = protocol.decode(plan_output, &[]);
        assert!(commands.contains(&LlmCommand::Text(TextCommand {
            content: "Let us refine the concept.".to_string(),
        })));
        let has_plan = commands.iter().any(
            |c| matches!(c, LlmCommand::Plan { update } if update.id.as_deref() == Some("p-1")),
        );
        assert!(has_plan);

        let research_output = r#"{
            "message": "Checking context.",
            "action": {
                "kind": "research",
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
                ]
            }
        }"#;
        let research_commands = protocol.decode(research_output, &[]);
        assert!(
            research_commands.contains(&LlmCommand::ContextRequest(ContextRequestCommand {
                path: "crates/core/src/lib.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }))
        );
        assert!(
            research_commands.contains(&LlmCommand::RepoSearch(RepoSearchCommand {
                path: "crates/core".to_string(),
                regex: "fn prompt".to_string(),
            }))
        );
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

        let chunk2 = r#"!","plan_update":null,"context_requests":[]}"#;
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
        let protocol = JsonPromptProtocol::default();
        let research_output = r#"{
            "message": "Need the git chapter",
            "scratchpad": "Verified: json decoder fixed.",
            "action": {
                "kind": "research",
                "tauqe_doc_requests": ["git"]
            }
        }"#;
        let research_commands = protocol.decode(research_output, &[]);
        assert!(
            research_commands.contains(&LlmCommand::DocRequest(DocRequestCommand {
                topic: tauqe_protocol::DocTopic::Git,
            }))
        );
        assert!(
            research_commands.contains(&LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "Verified: json decoder fixed.".to_string(),
            }))
        );

        // The canonical field is what the strict schema advertises, and the alias
        // stays a permanent spelling of the same command.
        let aliased = research_output.replace("tauqe_doc_requests", "doc_requests");
        assert!(protocol
            .decode(&aliased, &[])
            .contains(&LlmCommand::DocRequest(DocRequestCommand {
                topic: tauqe_protocol::DocTopic::Git,
            })));

        let edit_output = r#"{
            "message": "Done",
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "All checks passed",
                "reason": ""
            }
        }"#;
        assert!(protocol
            .decode(edit_output, &[])
            .contains(&LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "All checks passed".to_string(),
            })));

        let blocked_idle = r#"{
            "message": "Done",
            "action": {
                "kind": "blocked",
                "blocked": {"is_blocked": false, "reason": "", "details": ""}
            }
        }"#;
        assert!(!protocol
            .decode(blocked_idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::Blocked(_))));
    }

    #[test]
    fn test_decode_abort_from_structured_json_and_xml_fallback() {
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "The native build needs approval.",
            "action": {
                "kind": "abort",
                "abort": {
                    "is_aborted": true,
                    "reason": "Native build approval required",
                    "recommendation": "pnpm approve-builds"
                }
            }
        }"#;
        assert!(protocol
            .decode(output, &[])
            .contains(&LlmCommand::Abort(AbortCommand {
                reason: "Native build approval required".to_string(),
                recommendation: "pnpm approve-builds".to_string(),
            })));

        // An idle abort field reports nothing at all.
        let idle = r#"{
            "message": "x",
            "action": {
                "kind": "abort",
                "abort": {"is_aborted": false, "reason": "", "recommendation": ""}
            }
        }"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::Abort(_))));
    }

    #[test]
    fn test_decode_custom_tool_from_structured_json_and_xml_fallback() {
        let text_only =
            CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::CustomTools]);
        let protocol = JsonPromptProtocol::edits().with_capability_expr(text_only);
        let output = r#"{
            "message": "Running the declared probe.",
            "custom_tools": [{"name": "probe", "args": {"n": "10"}}]
        }"#;
        assert!(protocol
            .decode(output, &[])
            .contains(&LlmCommand::CustomTool(CustomToolCommand {
                name: "probe".to_string(),
                args: serde_json::json!({ "n": "10" }),
            })));

        // An empty array requests no execution.
        let idle = r#"{"message": "x", "custom_tools": []}"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::CustomTool(_))));
    }

    #[test]
    fn test_narrowed_capability_set_blocks_control_field_decoding() {
        let protocol = JsonPromptProtocol::edits()
            .with_capability_expr(CapabilityExpr::from_leaves([LlmCapability::TextMessage]));
        let output = r#"{
            "message": "Only text",
            "changes": [],
            "tauqe_doc_requests": ["git"],
            "scratchpad": "notes",
            "commit_message": "done",
            "reason": "",
            "blocked": {"is_blocked": true, "reason": "blocked", "details": ""}
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(!commands.is_empty());
        assert!(commands
            .iter()
            .all(|command| matches!(command, LlmCommand::Text(_))));
    }

    #[test]
    fn test_with_capabilities_derives_mode_from_file_edits() {
        let with_edits = JsonPromptProtocol::with_capabilities(dialogue(false, false));
        assert_eq!(with_edits.name(), "structured");
        assert!(with_edits.supports(LlmCapability::EditReplace));
        assert_eq!(with_edits.capability_expr(), dialogue(false, false));

        let no_edits = JsonPromptProtocol::with_capabilities(CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanUpdate,
        ]));
        assert!(!no_edits.supports(LlmCapability::EditReplace));
        assert_eq!(
            no_edits.capability_expr(),
            CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::PlanUpdate,])
        );
    }

    #[test]
    fn test_decode_verify_from_structured_json() {
        let protocol = JsonPromptProtocol::default();
        let output = r#"{
            "message": "Checking",
            "action": {
                "kind": "edit",
                "changes": [],
                "verify": {"target": "test", "on_success": "silent"}
            }
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(commands.contains(&LlmCommand::Verify(VerifyCommand {
            target: tauqe_protocol::VerifyTarget::Test,
            on_success: tauqe_protocol::VerifyOnSuccess::Silent,
        })));
    }

    #[test]
    fn test_decode_uses_the_context_paths_for_resolution_and_membership() {
        let protocol = JsonPromptProtocol::edits();
        let context_paths = vec!["crates/core/src/lib.rs".to_string()];

        // A unique suffix of a context file resolves to the context path.
        let suffix = r#"{
            "message": "Touch lib",
            "action": {
                "kind": "edit",
                "changes": [{"op": "replace", "path": "lib.rs", "to": "", "old_text": "old();", "new_text": "new();", "content": ""}]
            }
        }"#;
        let operation = protocol
            .decode(suffix, &context_paths)
            .into_iter()
            .find_map(LlmCommand::into_edit_operation)
            .expect("decoded edit");
        match operation {
            EditOperation::Replace(EditReplace { path, .. }) => {
                assert_eq!(path, "crates/core/src/lib.rs");
            }
            other => panic!("unexpected operation: {other:?}"),
        }

        // A path outside the context is rejected at the decode boundary instead
        // of being deferred to staging.
        let outside = r#"{
            "message": "Touch other",
            "action": {
                "kind": "edit",
                "changes": [{"op": "replace", "path": "crates/core/src/other.rs", "to": "", "old_text": "a", "new_text": "b", "content": ""}]
            }
        }"#;
        let commands = protocol.decode(outside, &context_paths);
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
        assert!(!commands
            .iter()
            .any(|command| is_edit_capability(command.capability())));
    }

    #[test]
    fn test_decode_plan_update_from_structured_json() {
        let protocol = JsonPromptProtocol::discussion();
        let output = r#"{
            "message": "Plan refined",
            "action": {
                "kind": "plan",
                "plan_action": {
                    "kind": "create",
                    "plan_create": {"action": "save", "id": "p-9", "title": "P9"}
                }
            }
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
            "action": {
                "kind": "plan",
                "plan_create": {"action": "save", "items": "not-a-list"}
            }
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
        let toolchain = CapabilityExpr::from_leaves([LlmCapability::ToolchainVerify]);
        let protocol = JsonPromptProtocol::with_capabilities(toolchain);
        let output = r#"{
            "message": "x",
            "changes": [{"op": "create", "path": "a.rs", "to": "", "old_text": "", "new_text": "", "content": "fn x() {}"}],
            "verify": {"target": "check", "on_success": "report"}
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(commands.iter().any(|c| matches!(c, LlmCommand::Verify(_))));
        assert!(!commands.iter().any(|c| is_edit_capability(c.capability())));
    }

    #[test]
    fn test_decode_silent_structured_round_emits_no_text() {
        // A round whose only conversational channel is empty must not surface
        // its JSON scaffolding as text: the REPL streamed nothing for it, and
        // session history must record the same nothing (plus phase banners).
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{"message":"","action":{"kind":"edit","changes":[],"verify":{"target":"none","on_success":"silent"}}}"#;
        let commands = protocol.decode(output, &[]);
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, LlmCommand::Text(_))),
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
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "  ",
                "reason": ""
            }
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(
            commands.is_empty(),
            "a blank commit message must decode into no command: {commands:?}"
        );
    }

    #[test]
    fn test_decode_empty_object_emits_no_commands() {
        // An empty object is the smallest silent structured round: it carries no
        // conversational channel and no actionable field, so the decoded surface
        // stays empty and never degrades into raw JSON text.
        let protocol = JsonPromptProtocol::with_capabilities(CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::EditReplace,
        ]));
        let commands = protocol.decode("{}", &[]);
        assert!(
            commands.is_empty(),
            "an empty structured round must decode into no command at all: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, LlmCommand::Text(_))),
            "an empty structured round must never become raw JSON text"
        );
    }

    #[test]
    fn test_decode_terminal_outcomes_always_carry_their_payload() {
        // A terminal status is a claim, and the decoding keeps exactly the
        // payload the round stated: an omitted field becomes the empty string
        // its atomic command carries, so the bounded turn decides how to
        // complete a claim nobody stated.
        let protocol = JsonPromptProtocol::default();

        let empty_terminal = r#"{
            "message": "",
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "",
                "message": "",
                "reason": "  "
            }
        }"#;
        assert!(protocol.decode(empty_terminal, &[]).is_empty());

        let structured_success = r#"{
            "message": "",
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "feat: add module",
                "message": "",
                "reason": ""
            }
        }"#;
        assert_eq!(
            protocol.decode(structured_success, &[]),
            vec![LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "feat: add module".to_string(),
            })]
        );

        let structured_error = r#"{
            "message": "",
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "",
                "message": "Missing table",
                "error_message": "Missing table",
                "reason": ""
            }
        }"#;
        assert_eq!(
            protocol.decode(structured_error, &[]),
            vec![
                LlmCommand::Text(TextCommand {
                    content: "Missing table".to_string(),
                }),
                LlmCommand::TurnError(TurnErrorCommand {
                    error_message: "Missing table".to_string(),
                }),
            ]
        );

        let structured_skip = r#"{
            "message": "",
            "action": {
                "kind": "edit",
                "changes": [],
                "commit_message": "",
                "message": "",
                "reason": "Already done"
            }
        }"#;
        assert_eq!(
            protocol.decode(structured_skip, &[]),
            vec![LlmCommand::TurnSkip(TurnSkipCommand {
                reason: "Already done".to_string(),
            })]
        );
    }
    #[test]
    fn test_decode_review_update_from_structured_json() {
        // A review discussion records the decision the engineer reached as an
        // atomic payload, so the decision travels as a typed command instead of
        // being folded into the conversational text. The decision belongs to the
        // review discussion regime alone, so the transport runs on that surface:
        // the shared dialogue head never advertises the field.
        let protocol = JsonPromptProtocol::with_capabilities(dialogue(false, true));
        let output = r#"{
            "message": "Recording the decision.",
            "action": {
                "kind": "review",
                "review_update": {
                    "item_id": 7,
                    "status": "rejected",
                    "note": "The guard is released after the await",
                    "recommendations": ["Cover the retry path"]
                }
            }
        }"#;
        let decision = protocol
            .decode(output, &[])
            .into_iter()
            .find_map(|command| match command {
                LlmCommand::ReviewUpdate { update } => Some(update),
                _ => None,
            })
            .expect("a review decision command");
        assert_eq!(decision.item_id, 7);
        assert_eq!(
            decision.status,
            tauqe_protocol::review::ReviewDecisionStatus::Rejected
        );
        assert_eq!(decision.note, "The guard is released after the await");

        // An idle field states no decision at all.
        let idle =
            r#"{"message":"Nothing to record","action":{"kind":"review","review_update":null}}"#;
        assert!(!protocol
            .decode(idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::ReviewUpdate { .. })));

        // A payload the DTO cannot hold is reported as a protocol error, so the
        // bounded turn can repeat generation with the diagnostics attached.
        let malformed =
            r#"{"message":"Bad","action":{"kind":"review","review_update":{"item_id":"seven"}}}"#;
        assert!(protocol
            .decode(malformed, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
    }

    #[test]
    fn test_decode_selects_the_operational_branch_the_round_declares() {
        // The tree scopes decoding: a round that declares the edit branch
        // yields its change set and no discovery command, even though the
        // response carries fields of the branches it did not select.
        let protocol = JsonPromptProtocol::edits();
        let output = r#"{
            "message": "Editing",
            "action": {
                "kind": "edit",
                "changes": [
                    {
                        "op": "create",
                        "path": "src/new.rs",
                        "to": "",
                        "content": "fn n() {}\n"
                    }
                ],
                "commit_message": "feat: add the module",
                "message": "",
                "reason": ""
            },
            "context_requests": ["src/hostile.rs"],
            "plan_update": {"action": "save", "id": "p-1", "title": "P"}
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::EditCreate(_))));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest(_))));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Plan { .. })));
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::CommitMessage(_))));
    }
    #[test]
    fn discussion_instructions_compose_the_active_capability_tree() {
        // The section is the composition of the active tree, so a property is
        // named only by the capability that owns an operational rule for it.
        let plan_tree = CapabilityExpr::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::ContextRequest,
        ]);
        let text_only = CapabilityExpr::from_leaves([LlmCapability::TextMessage]);

        let authored = JsonPromptProtocol::with_capabilities(plan_tree.clone())
            .system_instructions(&[], false, PatchStrategy::SearchReplace);
        let plain = JsonPromptProtocol::with_capabilities(text_only).system_instructions(
            &[],
            false,
            PatchStrategy::SearchReplace,
        );
        for text in [&authored, &plain] {
            for property in [
                "plan_create",
                "plan_update",
                "context_requests",
                "repo_searches",
            ] {
                assert!(
                    !text.contains(&format!("`{property}`")),
                    "the discussion section names the schema property `{property}`: {text}"
                );
            }
            assert!(text.starts_with("## Mode: Structured Discussion\n"));
            assert!(text.contains("matching the discussion_response schema"));
        }

        // The composition is live: a capability that carries an operational rule
        // contributes it, while an authorship capability carries none, so the
        // plan properties never reach text whose tree declares them beside it.
        let render = PromptRenderContext::new(&[], None);
        let with_edit =
            CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]);
        assert!(
            compose_structured_protocol_instructions(&with_edit, &render).contains("`changes`"),
            "the change-set mechanics follow the capability that owns them"
        );
        assert!(
            compose_structured_protocol_instructions(&plan_tree, &render).is_empty(),
            "a discussion tree names no plan property, because the schema owns it"
        );
    }

    #[test]
    fn a_custom_streamed_field_earns_no_hand_written_bullet() {
        // The discussion section is the composition of the active tree, so a
        // streamed field never earns a bullet the schema does not declare.
        let protocol = JsonPromptProtocol {
            mode: JsonPromptMode::Discussion {
                stream_field: "summary".to_string(),
            },
            capabilities: CapabilityExpr::from_leaves([
                LlmCapability::TextMessage,
                LlmCapability::PlanCreation,
            ]),
        };
        let instructions = protocol.system_instructions(&[], false, PatchStrategy::SearchReplace);
        assert!(!instructions.contains("`summary`"));
        assert!(!instructions.contains("Conversational Markdown response"));
        assert!(instructions.starts_with("## Mode: Structured Discussion\n"));
        assert!(instructions.contains("matching the discussion_response schema"));
        assert!(instructions.ends_with("markdown wrapping.\n"));
    }
}
