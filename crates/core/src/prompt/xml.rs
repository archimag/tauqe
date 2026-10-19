//! XML prompt protocol.
//!
//! [`XmlPromptProtocol`] decodes XML control tags and edit blocks into the
//! normalized [`LlmCommand`] vocabulary consumed by the execution pipeline. The
//! tag schema, the prompt text, and the decoding of every capability live in the
//! capability registry, so this module is a thin, capability-set-aware facade
//! over it: a protocol narrowed to a [`CapabilitySet`] neither advertises nor
//! decodes a disabled capability.

use crate::capabilities::CapabilityRegistry;
use crate::edits::protocol::xml::is_valid_turn_marker;
use crate::edits::protocol::XmlEditProtocol;
use crate::edits::stream::XmlStreamFilter;
use tauqe_protocol::{CapabilitySet, LlmCapability, LlmCommand};

use super::protocol::PromptProtocol;

/// Capability set advertised by the unbound XML protocol.
///
/// The order matches [`LlmCapability::ALL`], so the capability set derived from
/// it equals [`CapabilitySet::develop_xml`].
const XML_DEVELOP_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::TextMessage,
    LlmCapability::FileEdits,
    LlmCapability::Discovery,
    LlmCapability::DocRequest,
    LlmCapability::ToolchainVerify,
    LlmCapability::PlanCreation,
    LlmCapability::PlanUpdate,
    LlmCapability::Scratchpad,
    LlmCapability::Outcome,
    LlmCapability::Blocked,
    LlmCapability::UserLanguage,
];

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

    fn capabilities(&self) -> &[LlmCapability] {
        XML_DEVELOP_CAPABILITIES
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        Self::with_capabilities(capabilities)
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        decode_with_capabilities(raw_text, None, &self.capability_set())
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        XmlEditProtocol.system_instructions_for(&self.capability_set(), editable_paths, None)
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

    fn capabilities(&self) -> &[LlmCapability] {
        self.capabilities.as_slice()
    }

    fn capability_set(&self) -> CapabilitySet {
        self.capabilities.clone()
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        Self::boxed(capabilities, self.marker.clone())
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        decode_with_capabilities(raw_text, self.marker.as_deref(), &self.capabilities)
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        XmlEditProtocol.system_instructions_for(
            &self.capabilities,
            editable_paths,
            self.marker.as_deref(),
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
/// can never emit a command it does not advertise.
fn decode_with_capabilities(
    raw_text: &str,
    marker: Option<&str>,
    capabilities: &CapabilitySet,
) -> Vec<LlmCommand> {
    CapabilityRegistry::global().decode_xml_commands(capabilities, raw_text, marker, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{
        ContextAccess, DocTopic, EditOperation, LlmOutcome, VerifyOnSuccess, VerifyTarget,
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
            LlmCapability::PlanUpdate,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ] {
            assert!(protocol.supports(capability));
        }
        assert_eq!(protocol.capability_set(), CapabilitySet::develop_xml());
    }

    #[test]
    fn test_with_turn_marker_rejects_malformed_marker_without_panicking() {
        let proto = XmlPromptProtocol;
        assert!(proto.with_turn_marker("").is_none());
        assert!(proto.with_turn_marker("bad marker").is_none());
        assert!(proto.with_turn_marker("has-dash").is_none());
        assert!(proto.with_turn_marker("Valid123").is_some());

        let scoped = XmlPromptProtocol::with_capabilities(CapabilitySet::develop_xml());
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
        let commands = narrowed.decode(raw);
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
        let commands = protocol.decode("Here is my explanation.");
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
            "<tauqe_edits summary=\"Fix bug\">\n",
            "  <edit path=\"src/lib.rs\">\n",
            "    <search>\n",
            "old();\n",
            "    </search>\n",
            "    <replace>\n",
            "new();\n",
            "    </replace>\n",
            "  </edit>\n",
            "</tauqe_edits>\n",
        );
        let commands = protocol.decode(output);

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
        let commands = protocol.decode(output);

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
            "<tauqe_edits summary=\"Broken\">\n",
            "  <edit path=\"src/lib.rs\">\n",
            "    <search>\nfoo\n",
        );
        let commands = protocol.decode(output);
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
        let update = plan_update_from(&protocol.decode(output));
        assert_eq!(update.action, tauqe_protocol::DiscussionPlanAction::Save);
        assert_eq!(update.id.as_deref(), Some("p1"));
        assert_eq!(update.concept.len(), 1);
        assert_eq!(update.items.len(), 1);
    }

    #[test]
    fn test_decode_plan_update_action() {
        let protocol = XmlPromptProtocol;
        let output = "<plan action=\"update\" id=\"p1\"><item id=\"1\" status=\"done\" /></plan>";
        let update = plan_update_from(&protocol.decode(output));
        assert_eq!(update.action, tauqe_protocol::DiscussionPlanAction::Update);
        assert_eq!(update.id.as_deref(), Some("p1"));
    }

    #[test]
    fn test_decode_terminal_error_outcome() {
        let protocol = XmlPromptProtocol;
        let commands = protocol.decode("Blocked.\n<error message=\"Missing dependency\" />");
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
        );
        assert!(commands.contains(&LlmCommand::Blocked {
            reason: "Precondition failed".to_string(),
            details: Some("Missing file x.rs".to_string()),
        }));
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
