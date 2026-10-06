use workbench_protocol::ModelResult;

use crate::model::gateway::{ToolCall, ToolDefinition};

pub mod structured;
pub mod tool_call;
pub mod utils;
pub mod whole_file;
pub mod xml;

pub use structured::StructuredEditProtocol;
pub use tool_call::{FunctionCallingEditProtocol, ToolCallEditProtocol};
pub use utils::{normalize_content, resolve_target_path};
pub use whole_file::WholeFileEditProtocol;
pub use xml::{has_xml_edit_tags, XmlEditProtocol};

pub trait EditProtocol: Send + Sync {
    /// Identifier name of the edit protocol (e.g. "xml", "whole_file", "tool_call", "structured")
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

    /// Returns tool definitions if this protocol operates via native LLM tool calling.
    fn tools(&self, _editable_paths: &[String]) -> Option<Vec<ToolDefinition>> {
        None
    }

    /// Parses tool calls returned by the model into a ModelResult.
    fn parse_tool_calls(
        &self,
        _tool_calls: &[ToolCall],
        _editable_paths: &[String],
    ) -> Option<ModelResult> {
        None
    }
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol names.
    pub fn available_protocols() -> Vec<String> {
        vec![
            "xml".to_string(),
            "whole_file".to_string(),
            "tool_call".to_string(),
            "structured".to_string(),
        ]
    }

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol name.
    pub fn canonical_name(name: &str) -> Option<String> {
        match name.trim().to_lowercase().as_str() {
            "xml" => Some("xml".to_string()),
            "whole_file" => Some("whole_file".to_string()),
            "tool_call" | "tool_calling" | "tools" | "tool" | "function_calling" | "functions" => {
                Some("tool_call".to_string())
            }
            "structured" | "structured_output" | "json_schema" | "json" => {
                Some("structured".to_string())
            }
            _ => None,
        }
    }

    /// Creates an EditProtocol instance by name or alias.
    pub fn create_protocol(name: &str) -> Result<Box<dyn EditProtocol>, String> {
        match name.trim().to_lowercase().as_str() {
            "tool_call" | "tool_calling" | "tools" | "tool" | "function_calling" | "functions" => {
                Ok(Box::new(ToolCallEditProtocol))
            }
            "structured" | "structured_output" | "json_schema" | "json" => {
                Ok(Box::new(StructuredEditProtocol))
            }
            "whole_file" => Ok(Box::new(WholeFileEditProtocol)),
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
        let p1 = create_edit_protocol("tool_call");
        assert_eq!(p1.name(), "tool_call");

        let p2 = create_edit_protocol("tools");
        assert_eq!(p2.name(), "tool_call");

        let p3 = create_edit_protocol("function_calling");
        assert_eq!(p3.name(), "tool_call");

        let p4 = create_edit_protocol("xml");
        assert_eq!(p4.name(), "xml");
    }
}
