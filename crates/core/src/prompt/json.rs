//! JSON Schema (Structured Output) prompt protocol.
//!
//! [`JsonPromptProtocol`] decodes Structured Output (JSON schema) model output
//! into the normalized [`LlmCommand`] vocabulary consumed by the execution
//! pipeline. It isolates the JSON Schema wire syntax from the core business
//! logic and delegates concrete parsing to the structured edit protocol so the
//! schema keeps a single source of truth.

use crate::config::project::ProjectConfig;

use crate::capabilities::{decode_capability_expr, PromptRenderContext};
use crate::edits::protocol::structured::{
    resolve_decoded_edits, StructuredEditProtocol, StructuredPayload,
};
use crate::edits::stream::JsonStreamFilter;
use crate::llm::{CapabilityTree, LlmCapability, LlmCommand, TextCommand};
use crate::workflow::{EditProtocolKind, PatchStrategy};

use super::protocol::PromptProtocol;

/// Structured Output (JSON Schema) prompt protocol.
pub struct JsonPromptProtocol {
    capabilities: CapabilityTree,
    /// Project declaration the round observes, when the contour carries one.
    project: Option<ProjectConfig>,
}

impl JsonPromptProtocol {
    /// Constructs a working structured protocol with the contour's capability tree.
    pub fn new(capabilities: CapabilityTree) -> Self {
        Self {
            capabilities,
            project: None,
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

    fn capability_expr(&self) -> CapabilityTree {
        self.capabilities.clone()
    }

    fn with_project(&self, project: Option<ProjectConfig>) -> Box<dyn PromptProtocol> {
        // The declaration travels beside the capability tree, so the schema the
        // round is constrained by and the decoder that reads its answer observe
        // one project.
        Box::new(Self {
            capabilities: self.capabilities.clone(),
            project,
        })
    }

    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand> {
        self.decode_with_strategy(raw_text, context_paths, PatchStrategy::SearchReplace)
    }

    fn decode_with_strategy(
        &self,
        raw_text: &str,
        context_paths: &[String],
        strategy: PatchStrategy,
    ) -> Vec<LlmCommand> {
        // A structured round is one JSON object, and every command below is
        // read from this single decoding instead of re-parsing the response
        // once per field.
        let payload = StructuredPayload::parse(raw_text);
        let mut commands = Vec::new();

        // Conversational text with structured JSON markup removed. The message
        // channel is ambient at the root of the response, so every contour,
        // a dialogue and an isolated step alike, answers through the same
        // single field, and one response yields at most one conversational
        // command when the active tree declares the channel.
        let text = payload.clean_text();
        if !text.trim().is_empty() && self.capabilities.contains(LlmCapability::TextMessage) {
            commands.push(LlmCommand::Text(TextCommand { content: text }));
        }

        // Every operational command is decoded by walking the same
        // capability tree that composed the response schema: the ambient
        // leaves and the leaves of the branch the round selected, so a
        // capability the tree does not declare is never extracted from a
        // response.
        if let Some(root) = payload.root() {
            let ctx = PromptRenderContext::new(context_paths, None)
                .with_project(self.project.as_ref())
                .with_patch_strategy(strategy);
            let mut edits = Vec::new();
            for command in decode_capability_expr(&self.capabilities, root, &ctx) {
                match command {
                    // The module decodes paths but holds no working
                    // context, so the edit batch is resolved against the
                    // context paths of this round once the walk collected it.
                    cmd @ LlmCommand::ApplyPatch { .. } => commands.push(cmd),
                    cmd if is_edit_capability(crate::llm::command_capability(&cmd)) => {
                        edits.push(cmd)
                    }
                    // The ambient message channel was already emitted above.
                    LlmCommand::Text(_) => {}
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
        StructuredEditProtocol.system_instructions_for(
            &self.capabilities,
            editable_paths,
            patch_strategy,
            self.project.as_ref(),
        )
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
        // The declaration a round carries wins; a protocol bound through
        // `with_project` answers a caller that passes none, so the composed
        // schema and the rendered instructions observe one project.
        let project = project.or(self.project.as_ref());
        let render = PromptRenderContext::new(editable_paths, None)
            .with_project(project)
            .with_patch_strategy(patch_strategy);
        StructuredEditProtocol.response_format_for(&self.capabilities, &render)
    }

    fn create_stream_filter_with_strategy(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
        strategy: PatchStrategy,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        Box::new(
            JsonStreamFilter::new(editable_paths, repo_root)
                .with_staged_contents(staged_contents)
                .with_changes_preview(strategy != PatchStrategy::ApplyPatch),
        )
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        Box::new(
            JsonStreamFilter::new(editable_paths, repo_root).with_staged_contents(staged_contents),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::control::turn::turn_capability_expr;
    use crate::edits::EditOperation;
    use crate::edits::EditReplace;
    use crate::llm::{
        AbortCommand, CommitMessageCommand, ContextRequestCommand, CustomToolCommand,
        DirListCommand, DocRequestCommand, OuterDocRequestCommand, RepoDirsCommand, RepoMapCommand,
        RepoSearchCommand, ScratchpadCommand, TurnErrorCommand, TurnSkipCommand,
    };

    /// Capability tree of a dialogue contour, as the control loops declare it.
    fn dialogue(plan_update: bool, review_decision: bool) -> CapabilityTree {
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
        assert_eq!(protocol.name(), "structured");
        for capability in [
            LlmCapability::TextMessage,
            LlmCapability::EditReplace,
            LlmCapability::ContextRequest,
            LlmCapability::DocRequest,
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
                .any(|cmd| is_edit_capability(crate::llm::command_capability(cmd))),
            "no edit reaches staging from a payload the decoder rejected"
        );
    }

    #[test]
    fn test_decode_file_edit() {
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));

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
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
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
    fn test_working_surfaces_compose_the_edit_schema() {
        for tree in [
            dialogue(false, false),
            dialogue(true, false),
            dialogue(false, true),
            crate::control::turn::turn_capability_expr(),
        ] {
            let protocol = JsonPromptProtocol::new(tree);
            let crate::providers::ResponseFormat::JsonSchema { json_schema } = protocol
                .response_format(&[], PatchStrategy::SearchReplace, None)
                .expect("structured schema")
            else {
                panic!("expected edit schema")
            };
            let schema = flattened_surface(&json_schema.schema);
            assert!(schema["properties"].get("changes").is_some());
            assert!(schema["properties"].get("context_requests").is_some());
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    #[test]
    fn test_decode_discussion_response() {
        let protocol = JsonPromptProtocol::new(dialogue(true, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(true, false));
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));
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
                topic: crate::llm::DocTopic::Git,
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
                topic: crate::llm::DocTopic::Git,
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
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
            CapabilityTree::from_leaves([LlmCapability::TextMessage, LlmCapability::CustomTools]);
        let protocol = JsonPromptProtocol::new(text_only);
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
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
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
        let with_edits = JsonPromptProtocol::new(dialogue(false, false));
        assert_eq!(with_edits.name(), "structured");
        assert!(with_edits.supports(LlmCapability::EditReplace));
        assert_eq!(with_edits.capability_expr(), dialogue(false, false));

        let no_edits = JsonPromptProtocol::new(CapabilityTree::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::PlanUpdate,
        ]));
        assert!(!no_edits.supports(LlmCapability::EditReplace));
        assert_eq!(
            no_edits.capability_expr(),
            CapabilityTree::from_leaves([LlmCapability::TextMessage, LlmCapability::PlanUpdate,])
        );
    }

    #[test]
    fn test_decode_uses_the_context_paths_for_resolution_and_membership() {
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
            .any(|command| is_edit_capability(crate::llm::command_capability(command))));
    }

    #[test]
    fn test_decode_plan_update_from_structured_json() {
        let protocol = JsonPromptProtocol::new(dialogue(true, false));
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
        assert_eq!(
            plan.action,
            crate::llm::discussion::DiscussionPlanAction::Save
        );
    }

    #[test]
    fn test_decode_malformed_plan_update_emits_protocol_error() {
        let protocol = JsonPromptProtocol::new(dialogue(true, false));
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
        let research = CapabilityTree::from_leaves([LlmCapability::ContextRequest]);
        let protocol = JsonPromptProtocol::new(research);
        let output = r#"{
            "message": "x",
            "changes": [{"op": "create", "path": "a.rs", "to": "", "old_text": "", "new_text": "", "content": "fn x() {}"}],
            "context_requests": [{"path": "src/b.rs", "access": "read_only"}]
        }"#;
        let commands = protocol.decode(output, &[]);
        assert!(commands
            .iter()
            .any(|c| matches!(c, LlmCommand::ContextRequest(_))));
        assert!(!commands
            .iter()
            .any(|c| is_edit_capability(crate::llm::command_capability(c))));
    }

    #[test]
    fn test_decode_silent_structured_round_emits_no_text() {
        // A round whose only conversational channel is empty must not surface
        // its JSON scaffolding as text: the REPL streamed nothing for it, and
        // session history must record the same nothing (plus phase banners).
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
        let protocol = JsonPromptProtocol::new(CapabilityTree::from_leaves([
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
        let protocol = JsonPromptProtocol::new(dialogue(false, false));

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
        let protocol = JsonPromptProtocol::new(dialogue(false, true));
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
            crate::llm::discussion::ReviewDecisionStatus::Rejected
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
        let protocol = JsonPromptProtocol::new(turn_capability_expr());
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
}
