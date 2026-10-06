use workbench_protocol::ModelResult;

pub mod structured;
pub mod utils;
pub mod xml;

pub use structured::StructuredEditProtocol;
pub use utils::{normalize_content, resolve_target_path, resolve_target_path_for_op};
pub use xml::{has_xml_edit_tags, XmlEditProtocol};

pub trait EditProtocol: Send + Sync {
    /// Identifier name of the edit protocol (e.g. "xml", "structured")
    fn name(&self) -> &'static str;

    /// Returns system prompt instructions tailored for this edit protocol.
    fn system_instructions(&self, editable_paths: &[String]) -> String;

    /// Parses the model text output into a ModelResult.
    fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult;

    /// Returns response format if this protocol operates via Structured Output (json_schema).
    fn response_format(
        &self,
        _editable_paths: &[String],
    ) -> Option<crate::model::gateway::ResponseFormat> {
        None
    }
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol names.
    pub fn available_protocols() -> Vec<String> {
        vec!["xml".to_string(), "structured".to_string()]
    }

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol name.
    pub fn canonical_name(name: &str) -> Option<String> {
        match name.trim().to_lowercase().as_str() {
            "xml" => Some("xml".to_string()),
            "structured" | "structured_output" | "json_schema" | "json" => {
                Some("structured".to_string())
            }
            _ => None,
        }
    }

    /// Creates an EditProtocol instance by name or alias.
    pub fn create_protocol(name: &str) -> Result<Box<dyn EditProtocol>, String> {
        match name.trim().to_lowercase().as_str() {
            "structured" | "structured_output" | "json_schema" | "json" => {
                Ok(Box::new(StructuredEditProtocol))
            }
            "xml" => Ok(Box::new(XmlEditProtocol)),
            other => Err(format!(
                "Unknown edit protocol '{}'. Available protocols: {}",
                other,
                Self::available_protocols().join(", ")
            )),
        }
    }
}

/// Factory function to create an EditProtocol by identifier name.
pub fn create_edit_protocol(name: &str) -> Box<dyn EditProtocol> {
    EditProtocolFactory::create_protocol(name).unwrap_or_else(|_| Box::new(XmlEditProtocol))
}

/// Backward compatibility shim for any external references
#[derive(Debug, Clone, Default)]
pub struct SearchReplaceMarkers;

/// Backward compatibility alias
pub type CustomSearchReplaceEditProtocol = XmlEditProtocol;

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
            vec!["xml".to_string(), "structured".to_string()]
        );
    }
}
