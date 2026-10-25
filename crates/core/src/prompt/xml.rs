//! XML prompt protocol.
//!
//! [`XmlPromptProtocol`] decodes XML control tags and edit blocks into the
//! normalized [`LlmCommand`] vocabulary consumed by the execution pipeline. The
//! tag schema, the prompt text, and the decoding of every capability live in the
//! capability registry, so this module is a thin, capability-set-aware facade
//! over it: a protocol narrowed to a [`CapabilitySet`] neither advertises nor
//! decodes a disabled capability.

use std::sync::OnceLock;

use crate::capabilities::CapabilityRegistry;
use crate::edits::protocol::xml::is_valid_turn_marker;
use crate::edits::protocol::XmlEditProtocol;
use crate::edits::stream::XmlStreamFilter;
use tauqe_protocol::{CapabilitySet, EditProtocolKind, LlmCapability, LlmCommand};

use super::protocol::PromptProtocol;

/// Capability surface advertised by the unbound XML protocol.
///
/// The XML transport contributes the encoding only: the surface is the
/// intent-oriented interactive REPL profile, so choosing between XML and
/// Structured Output never changes what a turn of the same intent may do.
fn base_capabilities() -> CapabilitySet {
    CapabilitySet::interactive_repl()
}

/// XML wire-format implementation of [`PromptProtocol`].
#[derive(Debug, Default, Clone)]
pub struct XmlPromptProtocol;

impl XmlPromptProtocol {
    /// Creates an XML protocol narrowed to an explicit capability set.
    pub fn with_capabilities(capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        ScopedXmlPromptProtocol::boxed(capabilities, None)
    }
}

impl PromptProtocol for XmlPromptProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn kind(&self) -> EditProtocolKind {
        EditProtocolKind::Xml
    }

    fn capabilities(&self) -> &[LlmCapability] {
        static BASE: OnceLock<CapabilitySet> = OnceLock::new();
        BASE.get_or_init(base_capabilities).as_slice()
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        Self::with_capabilities(capabilities)
    }

    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand> {
        decode_with_capabilities(raw_text, None, &self.capability_set(), context_paths)
    }

    fn system_instructions(&self, editable_paths: &[String], has_eager_repo_map: bool) -> String {
        XmlEditProtocol.system_instructions_for(
            &self.capability_set(),
            editable_paths,
            None,
            has_eager_repo_map,
        )
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        ScopedXmlPromptProtocol::bind(self.capability_set(), marker)
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        Box::new(
            XmlStreamFilter::new(editable_paths, repo_root).with_staged_contents(staged_contents),
        )
    }
}

/// Capability-set-aware XML protocol, optionally bound to a turn marker.
struct ScopedXmlPromptProtocol {
    capabilities: CapabilitySet,
    marker: Option<String>,
}

impl ScopedXmlPromptProtocol {
    /// Boxes a scoped protocol for the trait-object surface.
    fn boxed(capabilities: CapabilitySet, marker: Option<String>) -> Box<dyn PromptProtocol> {
        Box::new(Self {
            capabilities,
            marker,
        })
    }

    /// Binds `marker`, rejecting a malformed marker without panicking.
    fn bind(capabilities: CapabilitySet, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        if !is_valid_turn_marker(marker) {
            tracing::warn!(marker = %marker, "ignoring malformed turn marker");
            return None;
        }
        Some(Self::boxed(capabilities, Some(marker.to_string())))
    }
}

impl PromptProtocol for ScopedXmlPromptProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn kind(&self) -> EditProtocolKind {
        EditProtocolKind::Xml
    }

    fn capabilities(&self) -> &[LlmCapability] {
        self.capabilities.as_slice()
    }

    fn capability_set(&self) -> CapabilitySet {
        self.capabilities.clone()
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        Self::boxed(capabilities, self.marker.clone())
    }

    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand> {
        decode_with_capabilities(
            raw_text,
            self.marker.as_deref(),
            &self.capabilities,
            context_paths,
        )
    }

    fn system_instructions(&self, editable_paths: &[String], has_eager_repo_map: bool) -> String {
        XmlEditProtocol.system_instructions_for(
            &self.capabilities,
            editable_paths,
            self.marker.as_deref(),
            has_eager_repo_map,
        )
    }

    fn turn_marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        Self::bind(self.capabilities.clone(), marker)
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        Box::new(
            XmlStreamFilter::new(editable_paths, repo_root)
                .with_marker(self.marker.clone())
                .with_staged_contents(staged_contents),
        )
    }
}

/// Decodes model output through the capability registry for `capabilities`.
///
/// Only the capabilities present in the set are decoded, so a narrowed protocol
/// can never emit a command it does not advertise. `context_paths` is forwarded
/// unchanged to the parser, so a decoded edit carries the same resolved path the
/// streaming filter already reported.
fn decode_with_capabilities(
    raw_text: &str,
    marker: Option<&str>,
    capabilities: &CapabilitySet,
    context_paths: &[String],
) -> Vec<LlmCommand> {
    CapabilityRegistry::global().decode_xml_commands(capabilities, raw_text, marker, context_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::stream::EditStreamFilter;
    use tauqe_protocol::{
        ContextAccess, DocTopic, EditFileStatus, EditOperation, LlmOutcome, VerifyOnSuccess,
        VerifyTarget,
    };

    #[test]
    fn test_name_and_capabilities() {
        let protocol = XmlPromptProtocol;
        assert_eq!(protocol.name(), "xml");
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
        // Plan mutation is a matter of the active plan focus, never of the
        // wire format, so the interactive profile omits it here exactly as the
        // Structured Output profile does.
        assert!(!protocol.supports(LlmCapability::PlanUpdate));
        assert_eq!(protocol.capability_set(), CapabilitySet::interactive_repl());
    }

    #[test]
    fn test_with_turn_marker_rejects_malformed_marker_without_panicking() {
        let proto = XmlPromptProtocol;
        assert!(proto.with_turn_marker("").is_none());
        assert!(proto.with_turn_marker("bad marker").is_none());
        assert!(proto.with_turn_marker("has-dash").is_none());
        assert!(proto.with_turn_marker("Valid123").is_some());

        let scoped = XmlPromptProtocol::with_capabilities(CapabilitySet::interactive_repl());
        assert!(scoped.with_turn_marker("").is_none());
        assert!(scoped.with_turn_marker("Valid123").is_some());
    }

    #[test]
    fn test_narrowed_capability_set_is_reported_and_blocks_decoding() {
        let narrowed = XmlPromptProtocol::with_capabilities(CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
        ]));
        assert_eq!(
            narrowed.capability_set(),
            CapabilitySet::from_slice(&[LlmCapability::TextMessage])
        );
        assert_eq!(narrowed.capabilities(), &[LlmCapability::TextMessage]);
        assert!(!narrowed.supports(LlmCapability::FileEdits));

        let raw =
            "Working.\n<context_request path=\"src/a.rs\" />\n<scratchpad>notes</scratchpad>\n";
        let commands = narrowed.decode(raw, &[]);
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest { .. })));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Scratchpad { .. })));
        assert!(commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Text { .. })));
    }

    #[test]
    fn test_decode_text_only() {
        let protocol = XmlPromptProtocol;
        let commands = protocol.decode("Here is my explanation.", &[]);
        assert_eq!(
            commands,
            vec![LlmCommand::Text {
                content: "Here is my explanation.".to_string(),
            }]
        );
    }

    #[test]
    fn test_decode_file_edit() {
        let protocol = XmlPromptProtocol;
        let output = concat!(
            "I fixed the bug.\n\n",
            "<edits summary=\"Fix bug\">\n",
            "  <edit path=\"src/lib.rs\">\n",
            "    <search>\n",
            "old();\n",
            "    </search>\n",
            "    <replace>\n",
            "new();\n",
            "    </replace>\n",
            "  </edit>\n",
            "</edits>\n",
        );
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::Text {
            content: "I fixed the bug.".to_string(),
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
                assert!(old_text.contains("old();"));
                assert!(new_text.contains("new();"));
            }
            other => panic!("unexpected operation: {other:?}"),
        }
    }

    #[test]
    fn test_decode_legacy_edit_container_alias() {
        let protocol = XmlPromptProtocol;
        let output = concat!(
            "Fixed it.\n\n",
            "<tauqe_edits summary=\"Fix bug\">\n",
            "  <edit path=\"src/lib.rs\">\n",
            "    <search>\nold();\n    </search>\n",
            "    <replace>\nnew();\n    </replace>\n",
            "  </edit>\n",
            "</tauqe_edits>\n",
        );

        // The pre-canonical container spelling stays decodable.
        let commands = protocol.decode(output, &[]);
        assert!(commands.iter().any(|command| matches!(
            command,
            LlmCommand::Edit {
                operation: EditOperation::Replace { path, .. }
            } if path == "src/lib.rs"
        )));

        // It is cleaned from conversational text exactly like the canonical
        // spelling, so a legacy transcript renders the same prose.
        let cleaned = CapabilityRegistry::global().clean_text(
            &CapabilitySet::interactive_repl(),
            output,
            None,
        );
        assert_eq!(cleaned, "Fixed it.");
    }

    #[test]
    fn test_decode_control_tags() {
        let protocol = XmlPromptProtocol;
        let output = concat!(
            "Working on it.\n",
            "<context_request path=\"src/a.rs\" access=\"editable\" />\n",
            "<repo_search path=\"src\" regex=\"fn main\" />\n",
            "<doc_request topic=\"workflow\" />\n",
            "<verify target=\"clippy\" on_success=\"silent\" />\n",
            "<scratchpad>Verified: parser works.</scratchpad>\n",
            "<user_language>Russian</user_language>\n",
            "<success summary=\"All checks pass\" />\n",
        );
        let commands = protocol.decode(output, &[]);

        assert!(commands.contains(&LlmCommand::Text {
            content: "Working on it.".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::Editable,
        }));
        assert!(commands.contains(&LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        }));
        assert!(commands.contains(&LlmCommand::Verify {
            target: VerifyTarget::Clippy,
            on_success: VerifyOnSuccess::Silent,
        }));
        assert!(commands.contains(&LlmCommand::Scratchpad {
            notes: "Verified: parser works.".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                summary: Some("All checks pass".to_string()),
            },
        }));
    }

    #[test]
    fn test_decode_malformed_xml_emits_protocol_error() {
        let protocol = XmlPromptProtocol;
        let output = concat!(
            "Here are my changes:\n",
            "<edits summary=\"Broken\">\n",
            "  <edit path=\"src/lib.rs\">\n",
            "    <search>\nfoo\n",
        );
        let commands = protocol.decode(output, &[]);
        let has_error = commands
            .iter()
            .any(|cmd| matches!(cmd, LlmCommand::ProtocolError { .. }));
        assert!(
            has_error,
            "Malformed XML should produce ProtocolError command"
        );
    }

    #[test]
    fn test_decode_plan_creation() {
        let protocol = XmlPromptProtocol;
        let output = concat!(
            "<plan action=\"save\" id=\"p1\" title=\"Plan\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"Concept\" />\n",
            "  </concept>\n",
            "  <item id=\"1\" title=\"Step\" status=\"todo\" />\n",
            "</plan>",
        );
        let update = plan_update_from(&protocol.decode(output, &[]));
        assert_eq!(update.action, tauqe_protocol::DiscussionPlanAction::Save);
        assert_eq!(update.id.as_deref(), Some("p1"));
        assert_eq!(update.concept.len(), 1);
        assert_eq!(update.items.len(), 1);
    }

    #[test]
    fn test_decode_plan_update_action() {
        // Plan mutation follows the plan focus, not the transport, so the
        // interactive profile omits it and the discussion profile carries it.
        let output = "<plan action=\"update\" id=\"p1\"><item id=\"1\" status=\"done\" /></plan>";
        let protocol = XmlPromptProtocol::with_capabilities(CapabilitySet::discussion());
        let update = plan_update_from(&protocol.decode(output, &[]));
        assert_eq!(update.action, tauqe_protocol::DiscussionPlanAction::Update);
        assert_eq!(update.id.as_deref(), Some("p1"));

        assert!(XmlPromptProtocol
            .decode(output, &[])
            .iter()
            .all(|command| !matches!(command, LlmCommand::Plan { .. })));
    }

    #[test]
    fn test_decode_terminal_error_outcome() {
        let protocol = XmlPromptProtocol;
        let commands = protocol.decode("Blocked.\n<error message=\"Missing dependency\" />", &[]);
        assert!(commands.contains(&LlmCommand::Outcome {
            outcome: LlmOutcome::Error {
                message: "Missing dependency".to_string(),
            },
        }));
    }

    #[test]
    fn test_decode_terminal_blocked_outcome() {
        let protocol = XmlPromptProtocol;
        let commands = protocol.decode(
            "Blocked.\n<blocked reason=\"Precondition failed\" details=\"Missing file x.rs\" />",
            &[],
        );
        assert!(commands.contains(&LlmCommand::Blocked {
            reason: "Precondition failed".to_string(),
            details: Some("Missing file x.rs".to_string()),
        }));
    }

    #[test]
    fn test_stream_and_decode_resolve_the_same_edit_path() {
        let context_paths = vec!["crates/core/src/lib.rs".to_string()];
        let output = concat!(
            "Editing.\n\n",
            "<edits summary=\"Touch lib\">\n",
            "  <edit path=\"lib.rs\">\n",
            "    <search>\nold();\n    </search>\n",
            "    <replace>\nnew();\n    </replace>\n",
            "  </edit>\n",
            "</edits>\n",
        );

        // The decoded command resolves the short suffix against the context
        // list instead of carrying the raw path the model emitted.
        let decoded_path = XmlPromptProtocol
            .decode(output, &context_paths)
            .into_iter()
            .find_map(|command| match command {
                LlmCommand::Edit {
                    operation: EditOperation::Replace { path, .. },
                } => Some(path),
                _ => None,
            })
            .expect("decoded edit");

        // The streaming preview receives the identical list, so the file it
        // certifies as edited is the file the engine stages.
        let mut filter = XmlStreamFilter::new(context_paths.clone(), std::env::temp_dir())
            .with_staged_contents(std::collections::HashMap::from([(
                "crates/core/src/lib.rs".to_string(),
                "old();\n".to_string(),
            )]));
        let mut events = filter.push_chunk(output);
        events.extend(Box::new(filter).finish());

        let streamed_path = events
            .iter()
            .find_map(|event| match event {
                crate::providers::StreamEvent::EditFileStarted { path, .. } => Some(path.clone()),
                _ => None,
            })
            .expect("streamed file start");

        assert_eq!(decoded_path, "crates/core/src/lib.rs");
        assert_eq!(streamed_path, decoded_path);
        assert!(events.iter().any(|event| matches!(
            event,
            crate::providers::StreamEvent::EditFileDone {
                status: EditFileStatus::Ok,
                ..
            }
        )));
    }

    fn plan_update_from(commands: &[LlmCommand]) -> tauqe_protocol::DiscussionPlanUpdate {
        commands
            .iter()
            .find_map(|command| match command {
                LlmCommand::Plan { update } => Some(update.clone()),
                _ => None,
            })
            .expect("plan command")
    }
}
