use tauqe_protocol::{ContextAccess, DocTopic, EditProtocolKind, ModelResult};

pub mod structured;
pub mod utils;
pub mod xml;

pub use structured::StructuredEditProtocol;
pub use xml::{
    MarkedXmlEditProtocol, TerminalOutcome, VerifyOnSuccess, VerifyRequest, VerifyTarget,
    XmlEditProtocol,
};

/// A file the model asks to be added to the context before it proposes edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRequest {
    pub path: String,
    pub access: ContextAccess,
}

pub trait EditProtocol: Send + Sync {
    /// Identifier name of the edit protocol (e.g. "xml", "structured")
    fn name(&self) -> &'static str;

    /// Returns system prompt instructions tailored for this edit protocol.
    fn system_instructions(
        &self,
        editable_paths: &[String],
    ) -> String;

    /// Parses the model text output into a ModelResult.
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult;

    /// Extracts context requests (files the model wants added to context) from raw output.
    fn parse_context_requests(&self, _raw_text: &str) -> Vec<ContextRequest> {
        Vec::new()
    }

    /// Extracts requested documentation topics (`All` when unspecified) from raw output.
    fn parse_doc_requests(&self, _raw_text: &str) -> Vec<DocTopic> {
        Vec::new()
    }

    /// Extracts code verification request (check/clippy/test) from raw output if supported.
    fn parse_verify_request(&self, _raw_text: &str) -> Option<VerifyRequest> {
        None
    }

    /// Extracts detected user language from raw output if supported.
    fn parse_user_language(&self, _raw_text: &str) -> Option<String> {
        None
    }

    /// Extracts plan tags and validation diagnostics from raw output.
    fn parse_plan_tags(
        &self,
        _raw_text: &str,
    ) -> (Vec<crate::edits::protocol::xml::tags::ParsedPlanTag>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// Extracts ephemeral scratchpad notes (<scratchpad>) from raw output if present.
    fn parse_scratchpad(&self, _raw_text: &str) -> Option<String> {
        None
    }

    /// Extracts terminal outcome (<success> or <error>) from raw output if present.
    fn parse_terminal_outcome(&self, _raw_text: &str) -> Option<TerminalOutcome> {
        None
    }

    /// Extracts blocker or error message (<error>) from raw output if present.
    fn parse_error(&self, _raw_text: &str) -> Option<String> {
        None
    }

    /// Cleans model output for presentation and history by stripping edit blocks and protocol control tags.
    fn clean_assistant_text(&self, raw_text: &str) -> String {
        raw_text.to_string()
    }

    /// Binds a protocol to a per-request marker; non-XML protocols stay unchanged.
    fn with_turn_marker(&self, _marker: &str) -> Option<Box<dyn EditProtocol>> {
        None
    }

    fn turn_marker(&self) -> Option<&str> {
        None
    }

    /// Returns response format if this protocol operates via Structured Output (json_schema).
    fn response_format(
        &self,
        _editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        None
    }

    /// Creates a streaming filter for processing model output in real time.
    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter>;
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol kinds.
    pub fn available_protocols() -> Vec<EditProtocolKind> {
        vec![EditProtocolKind::Xml, EditProtocolKind::Structured]
    }

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol kind.
    pub fn canonical_name(name: &str) -> Option<EditProtocolKind> {
        name.parse::<EditProtocolKind>().ok()
    }

    /// Creates an EditProtocol instance by protocol kind.
    pub fn create_protocol(kind: EditProtocolKind) -> Box<dyn EditProtocol> {
        match kind {
            EditProtocolKind::Structured => Box::new(StructuredEditProtocol),
            EditProtocolKind::Xml => Box::new(XmlEditProtocol),
        }
    }
}

/// Factory function to create an EditProtocol by identifier name or alias.
pub fn create_edit_protocol(name: &str) -> Box<dyn EditProtocol> {
    name.parse::<EditProtocolKind>()
        .map(EditProtocolFactory::create_protocol)
        .unwrap_or_else(|_| Box::new(XmlEditProtocol))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_edit_protocol_factory() {
        let p1 = create_edit_protocol("structured");
        assert_eq!(p1.name(), "structured");

        let p2 = create_edit_protocol("json_schema");
        assert_eq!(p2.name(), "structured");

        let p3 = create_edit_protocol("xml");
        assert_eq!(p3.name(), "xml");

        // Removed protocols fall back to xml
        let p4 = create_edit_protocol("tool_call");
        assert_eq!(p4.name(), "xml");

        assert_eq!(
            EditProtocolFactory::available_protocols(),
            vec![EditProtocolKind::Xml, EditProtocolKind::Structured]
        );
    }
}
