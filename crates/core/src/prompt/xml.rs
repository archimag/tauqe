//! XML prompt protocol.
//!
//! [`XmlPromptProtocol`] decodes XML control tags and edit blocks into the
//! normalized [`LlmCommand`] vocabulary consumed by the execution pipeline. It
//! isolates the tag syntax (context requests, verification, plans, terminal
//! outcomes, and file edits) from the core business logic. Concrete tag
//! parsing is delegated to the edit protocol so the XML grammar keeps a single
//! source of truth.

use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use crate::edits::protocol::{EditProtocol, MarkedXmlEditProtocol, XmlEditProtocol};
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, LlmCapability, LlmCommand, LlmOutcome, ModelResult,
};

use super::protocol::PromptProtocol;

/// XML wire-format implementation of [`PromptProtocol`].
#[derive(Debug, Default, Clone)]
pub struct XmlPromptProtocol;

/// Declarative interaction capabilities enabled by the XML prompt contour.
const XML_PROMPT_CAPABILITIES: &[LlmCapability] = &[
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

impl PromptProtocol for XmlPromptProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn capabilities(&self) -> &[LlmCapability] {
        XML_PROMPT_CAPABILITIES
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        decode_xml_commands(&XmlEditProtocol, raw_text)
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        XmlEditProtocol.system_instructions(editable_paths)
    }

    fn turn_marker(&self) -> Option<&str> {
        XmlEditProtocol.turn_marker()
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        XmlEditProtocol
            .with_turn_marker(marker)
            .map(|_| Box::new(MarkedXmlPromptProtocol::new(marker)) as Box<dyn PromptProtocol>)
    }

    fn response_format(
        &self,
        editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        XmlEditProtocol.response_format(editable_paths)
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        XmlEditProtocol.create_stream_filter(editable_paths, repo_root, staged_contents)
    }
}

/// Marker-bound XML prompt protocol.
///
/// Wraps [`MarkedXmlEditProtocol`] so a specific turn marker can be bound to the
/// prompt protocol, exposing the shared edit-parsing machinery through the same
/// [`PromptProtocol`] surface as the unbound XML protocol.
struct MarkedXmlPromptProtocol {
    inner: MarkedXmlEditProtocol,
}

impl MarkedXmlPromptProtocol {
    /// Creates a marker-bound XML prompt protocol for the given marker suffix.
    fn new(marker: &str) -> Self {
        Self {
            inner: MarkedXmlEditProtocol::new(marker),
        }
    }
}

impl PromptProtocol for MarkedXmlPromptProtocol {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn capabilities(&self) -> &[LlmCapability] {
        XML_PROMPT_CAPABILITIES
    }

    fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
        decode_xml_commands(&self.inner, raw_text)
    }

    fn system_instructions(&self, editable_paths: &[String]) -> String {
        self.inner.system_instructions(editable_paths)
    }

    fn turn_marker(&self) -> Option<&str> {
        self.inner.turn_marker()
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        self.inner
            .with_turn_marker(marker)
            .map(|_| Box::new(MarkedXmlPromptProtocol::new(marker)) as Box<dyn PromptProtocol>)
    }

    fn response_format(
        &self,
        editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        self.inner.response_format(editable_paths)
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        self.inner
            .create_stream_filter(editable_paths, repo_root, staged_contents)
    }
}

/// Extracts normalized commands from XML-family model output.
///
/// Shared by the unbound and marker-bound XML protocols so tag extraction keeps
/// a single source of truth.
fn decode_xml_commands(xml: &dyn EditProtocol, raw_text: &str) -> Vec<LlmCommand> {
    let mut commands = Vec::new();

    // Conversational text with edit blocks and control tags removed.
    let text = xml.clean_assistant_text(raw_text);
    if !text.trim().is_empty() {
        commands.push(LlmCommand::Text { content: text });
    }

    // File edit operations decoded from XML edit blocks.
    match xml.parse_output(raw_text, &[]) {
        ModelResult::Edit { edits, error, .. } => {
            if let Some(message) = error {
                commands.push(LlmCommand::ProtocolError { message });
            }
            for operation in edits {
                commands.push(LlmCommand::Edit { operation });
            }
        }
        ModelResult::Answer { .. } => {}
    }

    // Discovery: context requests.
    for request in xml.parse_context_requests(raw_text) {
        commands.push(LlmCommand::ContextRequest {
            path: request.path,
            access: request.access,
        });
    }

    // Documentation requests.
    for topic in xml.parse_doc_requests(raw_text) {
        commands.push(LlmCommand::DocRequest { topic });
    }

    // Plan creation and updates.
    let (plans, _diagnostics) = xml.parse_plan_tags(raw_text);
    for plan in plans {
        commands.push(LlmCommand::Plan {
            update: plan_tag_to_update(plan),
        });
    }

    // Toolchain verification.
    if let Some(request) = xml.parse_verify_request(raw_text) {
        commands.push(LlmCommand::Verify {
            target: map_verify_target(request.target),
            on_success: map_verify_on_success(request.on_success),
        });
    }

    // Ephemeral working-memory notes.
    if let Some(notes) = xml.parse_scratchpad(raw_text) {
        commands.push(LlmCommand::Scratchpad { notes });
    }

    // Explicit terminal outcome or semantic blocker.
    if let Some(outcome) = xml.parse_terminal_outcome(raw_text) {
        commands.push(terminal_outcome_to_command(outcome));
    }

    // Detected user language.
    if let Some(language) = xml.parse_user_language(raw_text) {
        commands.push(LlmCommand::UserLanguage { language });
    }

    commands
}

/// Normalizes a parsed `<plan>` tag into a [`DiscussionPlanUpdate`].
fn plan_tag_to_update(tag: ParsedPlanTag) -> DiscussionPlanUpdate {
    let action = match tag.action.as_str() {
        "save" => DiscussionPlanAction::Save,
        "delete" => DiscussionPlanAction::Delete,
        _ => DiscussionPlanAction::Update,
    };
    DiscussionPlanUpdate {
        action,
        id: Some(tag.id),
        title: tag.title,
        description: tag.description,
        concept: tag.concept,
        items: tag.items.into_iter().map(ParsedPlanItem::into_plan_item).collect(),
    }
}

/// Maps the edit-protocol verification target onto the normalized command target.
pub(super) fn map_verify_target(
    target: crate::edits::protocol::VerifyTarget,
) -> tauqe_protocol::VerifyTarget {
    use crate::edits::protocol::VerifyTarget as Edit;
    use tauqe_protocol::VerifyTarget as Command;
    match target {
        Edit::All => Command::All,
        Edit::Check => Command::Check,
        Edit::Test => Command::Test,
        Edit::Clippy => Command::Clippy,
    }
}

/// Maps the edit-protocol success policy onto the normalized command policy.
pub(super) fn map_verify_on_success(
    on_success: crate::edits::protocol::VerifyOnSuccess,
) -> tauqe_protocol::VerifyOnSuccess {
    use crate::edits::protocol::VerifyOnSuccess as Edit;
    use tauqe_protocol::VerifyOnSuccess as Command;
    match on_success {
        Edit::Silent => Command::Silent,
        Edit::Report => Command::Report,
    }
}

/// Normalizes a parsed terminal outcome into the matching normalized command.
///
/// The match is exhaustive over the edit-protocol terminal outcome, so a
/// semantic blocker can never be collapsed into an error: blockers stay as
/// `LlmCommand::Blocked` and keep their details, preserving the "blocked is not
/// an error" invariant that drives semantic recovery.
fn terminal_outcome_to_command(
    outcome: crate::edits::protocol::TerminalOutcome,
) -> LlmCommand {
    use crate::edits::protocol::TerminalOutcome;
    match outcome {
        TerminalOutcome::Blocked { reason, details } => LlmCommand::Blocked { reason, details },
        TerminalOutcome::Success { summary } => LlmCommand::Outcome {
            outcome: LlmOutcome::Success { summary },
        },
        TerminalOutcome::Error { message } => LlmCommand::Outcome {
            outcome: LlmOutcome::Error { message },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, DocTopic, EditOperation, VerifyOnSuccess, VerifyTarget};

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
        assert!(has_error, "Malformed XML should produce ProtocolError command");
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
        assert_eq!(update.action, DiscussionPlanAction::Save);
        assert_eq!(update.id.as_deref(), Some("p1"));
        assert_eq!(update.concept.len(), 1);
        assert_eq!(update.items.len(), 1);
    }

    #[test]
    fn test_decode_plan_update_action() {
        let protocol = XmlPromptProtocol;
        let output = "<plan action=\"update\" id=\"p1\"><item id=\"1\" status=\"done\" /></plan>";
        let update = plan_update_from(&protocol.decode(output));
        assert_eq!(update.action, DiscussionPlanAction::Update);
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
        let commands = protocol.decode("Blocked.\n<blocked reason=\"Precondition failed\" details=\"Missing file x.rs\" />");
        assert!(commands.contains(&LlmCommand::Blocked {
            reason: "Precondition failed".to_string(),
            details: Some("Missing file x.rs".to_string()),
        }));
    }

    fn plan_update_from(commands: &[LlmCommand]) -> DiscussionPlanUpdate {
        commands
            .iter()
            .find_map(|command| match command {
                LlmCommand::Plan { update } => Some(update.clone()),
                _ => None,
            })
            .expect("plan command")
    }
}
