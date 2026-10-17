//! XML prompt protocol.
//!
//! [`XmlPromptProtocol`] decodes XML control tags and edit blocks into the
//! normalized [`LlmCommand`] vocabulary consumed by the execution pipeline. It
//! isolates the tag syntax (context requests, verification, plans, terminal
//! outcomes, and file edits) from the core business logic. Concrete tag
//! parsing is delegated to the edit protocol so the XML grammar keeps a single
//! source of truth.

use crate::edits::protocol::xml::is_valid_turn_marker;
use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use crate::edits::protocol::{
    ContextRequest, MarkedXmlEditProtocol, TerminalOutcome, VerifyRequest, XmlEditProtocol,
};
use crate::edits::stream::XmlStreamFilter;
use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, DocTopic, LlmCapability, LlmCommand, LlmOutcome,
    ModelResult,
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
        None
    }

    fn with_turn_marker(&self, marker: &str) -> Option<Box<dyn PromptProtocol>> {
        bind_turn_marker(marker)
    }

    fn response_format(
        &self,
        _editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        None
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

/// Binds the active turn marker to a marker-bound XML prompt protocol.
///
/// A malformed marker is rejected without panicking, so a bad marker can never
/// crash the long-lived daemon.
fn bind_turn_marker(marker: &str) -> Option<Box<dyn PromptProtocol>> {
    if !is_valid_turn_marker(marker) {
        tracing::warn!(marker = %marker, "ignoring malformed turn marker");
        return None;
    }
    Some(Box::new(MarkedXmlPromptProtocol::new(marker)))
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
        bind_turn_marker(marker)
    }

    fn response_format(
        &self,
        _editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        None
    }

    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        Box::new(
            XmlStreamFilter::new(editable_paths, repo_root)
                .with_marker(self.inner.turn_marker().map(str::to_string))
                .with_staged_contents(staged_contents),
        )
    }
}

/// Private extraction surface shared by the unbound and marker-bound XML parsers.
///
/// Both parsers decode the same XML tag grammar; the marker-bound variant only
/// normalizes the active turn marker before parsing and restores marker-mangled
/// literals afterwards. This trait lets the single [`decode_xml_commands`]
/// routine serve both parsers while callers keep interacting exclusively through
/// [`PromptProtocol`].
trait XmlExtraction {
    fn clean_assistant_text(&self, raw_text: &str) -> String;
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult;
    fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest>;
    fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest>;
    fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic>;
    fn parse_plan_tags(&self, raw_text: &str) -> (Vec<ParsedPlanTag>, Vec<String>);
    fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest>;
    fn parse_scratchpad(&self, raw_text: &str) -> Option<String>;
    fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome>;
    fn parse_user_language(&self, raw_text: &str) -> Option<String>;
}

impl XmlExtraction for XmlEditProtocol {
    fn clean_assistant_text(&self, raw_text: &str) -> String {
        XmlEditProtocol::clean_assistant_text(self, raw_text)
    }
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        XmlEditProtocol::parse_output(self, raw_text, editable_paths)
    }
    fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        XmlEditProtocol::parse_context_requests(self, raw_text)
    }
    fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest> {
        XmlEditProtocol::parse_repo_searches(self, raw_text)
    }
    fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic> {
        XmlEditProtocol::parse_doc_requests(self, raw_text)
    }
    fn parse_plan_tags(&self, raw_text: &str) -> (Vec<ParsedPlanTag>, Vec<String>) {
        XmlEditProtocol::parse_plan_tags(self, raw_text)
    }
    fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest> {
        XmlEditProtocol::parse_verify_request(self, raw_text)
    }
    fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        XmlEditProtocol::parse_scratchpad(self, raw_text)
    }
    fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome> {
        XmlEditProtocol::parse_terminal_outcome(self, raw_text)
    }
    fn parse_user_language(&self, raw_text: &str) -> Option<String> {
        XmlEditProtocol::parse_user_language(self, raw_text)
    }
}

impl XmlExtraction for MarkedXmlEditProtocol {
    fn clean_assistant_text(&self, raw_text: &str) -> String {
        MarkedXmlEditProtocol::clean_assistant_text(self, raw_text)
    }
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        MarkedXmlEditProtocol::parse_output(self, raw_text, editable_paths)
    }
    fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        MarkedXmlEditProtocol::parse_context_requests(self, raw_text)
    }
    fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest> {
        MarkedXmlEditProtocol::parse_repo_searches(self, raw_text)
    }
    fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic> {
        MarkedXmlEditProtocol::parse_doc_requests(self, raw_text)
    }
    fn parse_plan_tags(&self, raw_text: &str) -> (Vec<ParsedPlanTag>, Vec<String>) {
        MarkedXmlEditProtocol::parse_plan_tags(self, raw_text)
    }
    fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest> {
        MarkedXmlEditProtocol::parse_verify_request(self, raw_text)
    }
    fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        MarkedXmlEditProtocol::parse_scratchpad(self, raw_text)
    }
    fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome> {
        MarkedXmlEditProtocol::parse_terminal_outcome(self, raw_text)
    }
    fn parse_user_language(&self, raw_text: &str) -> Option<String> {
        MarkedXmlEditProtocol::parse_user_language(self, raw_text)
    }
}

/// Extracts normalized commands from XML-family model output.
///
/// Shared by the unbound and marker-bound XML protocols so tag extraction keeps
/// a single source of truth.
fn decode_xml_commands(xml: &impl XmlExtraction, raw_text: &str) -> Vec<LlmCommand> {
    let mut commands = Vec::new();

    // Conversational text with edit blocks and control tags removed.
    let text = xml.clean_assistant_text(raw_text);
    if !text.trim().is_empty() {
        commands.push(LlmCommand::Text { content: text });
    }

    // File edit operations decoded from XML edit blocks.
    match xml.parse_output(raw_text, &[]) {
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

    // Discovery: context requests.
    for request in xml.parse_context_requests(raw_text) {
        commands.push(LlmCommand::ContextRequest {
            path: request.path,
            access: request.access,
        });
    }

    // Discovery: repository search requests.
    for search in xml.parse_repo_searches(raw_text) {
        commands.push(LlmCommand::RepoSearch {
            path: search.path,
            regex: search.regex,
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
    fn test_with_turn_marker_rejects_malformed_marker_without_panicking() {
        let proto = XmlPromptProtocol;
        assert!(proto.with_turn_marker("").is_none());
        assert!(proto.with_turn_marker("bad marker").is_none());
        assert!(proto.with_turn_marker("has-dash").is_none());
        assert!(proto.with_turn_marker("Valid123").is_some());

        let marked = MarkedXmlPromptProtocol::new("Valid123");
        assert!(marked.with_turn_marker("").is_none());
        assert!(marked.with_turn_marker("Valid123").is_some());
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
