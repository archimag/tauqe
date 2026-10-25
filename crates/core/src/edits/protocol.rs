use tauqe_protocol::{ContextAccess, EditProtocolKind};

pub mod structured;
pub mod utils;
pub mod xml;

pub use structured::StructuredEditProtocol;
pub use xml::{
    strip_repo_search_tags, MarkedXmlEditProtocol, TerminalOutcome, VerifyOnSuccess, VerifyRequest,
    VerifyTarget, XmlEditProtocol,
};

/// A file the model asks to be added to the context before it proposes edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRequest {
    pub path: String,
    pub access: ContextAccess,
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol kinds.
    pub fn available_protocols() -> Vec<EditProtocolKind> {
        vec![
            EditProtocolKind::Xml,
            EditProtocolKind::Structured,
            EditProtocolKind::Mixed,
        ]
    }

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol kind.
    pub fn canonical_name(name: &str) -> Option<EditProtocolKind> {
        name.parse::<EditProtocolKind>().ok()
    }

    /// Creates a [`PromptProtocol`](crate::prompt::protocol::PromptProtocol)
    /// instance by protocol kind.
    ///
    /// The factory returns the unified prompt protocol contract, so callers
    /// never depend on a separate edit-protocol hierarchy.
    ///
    /// `Mixed` resolves to the XML contour, because code edits never travel
    /// through the structured contour in that mode; the structured transport of
    /// the mode is reserved for discussions, commit messages, and history
    /// compaction, which are resolved by the workflows that own them.
    pub fn create_protocol(
        kind: EditProtocolKind,
    ) -> Box<dyn crate::prompt::protocol::PromptProtocol> {
        match kind {
            EditProtocolKind::Structured => {
                Box::new(crate::prompt::json::JsonPromptProtocol::edits())
            }
            EditProtocolKind::Xml | EditProtocolKind::Mixed => {
                Box::new(crate::prompt::xml::XmlPromptProtocol)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_protocol_factory_returns_prompt_protocol() {
        let structured = EditProtocolFactory::create_protocol(EditProtocolKind::Structured);
        assert_eq!(structured.name(), "structured");

        let xml = EditProtocolFactory::create_protocol(EditProtocolKind::Xml);
        assert_eq!(xml.name(), "xml");

        // The two-contour mode edits code through the XML transport.
        let mixed = EditProtocolFactory::create_protocol(EditProtocolKind::Mixed);
        assert_eq!(mixed.name(), "xml");
    }

    #[test]
    fn test_canonical_name_resolves_aliases() {
        assert_eq!(
            EditProtocolFactory::canonical_name("json_schema"),
            Some(EditProtocolKind::Structured)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("xml"),
            Some(EditProtocolKind::Xml)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("mixed"),
            Some(EditProtocolKind::Mixed)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("auto"),
            Some(EditProtocolKind::Mixed)
        );
        assert_eq!(EditProtocolFactory::canonical_name("tool_call"), None);
        assert_eq!(
            EditProtocolFactory::available_protocols(),
            vec![
                EditProtocolKind::Xml,
                EditProtocolKind::Structured,
                EditProtocolKind::Mixed
            ]
        );
    }
}
