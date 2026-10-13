//! Protocol-agnostic prompt decoding abstraction.
//!
//! A [`PromptProtocol`] decouples the concrete wire syntax of model output
//! (XML markers, JSON schema) from the core execution pipeline. Each protocol
//! advertises the declarative set of [`LlmCapability`] values it enables and
//! decodes raw model output into a uniform list of [`LlmCommand`] values. The
//! pipeline therefore operates purely on normalized semantics and stays
//! independent of any single protocol format.

use crate::edits::protocol::create_edit_protocol;
use tauqe_protocol::{LlmCapability, LlmCommand};

/// Decodes raw model output into normalized core commands.
///
/// Implementors translate their protocol-specific syntax into the shared
/// [`LlmCommand`] vocabulary, emitting commands only for the capabilities they
/// advertise through [`PromptProtocol::capabilities`]. This keeps the execution
/// pipeline agnostic of the concrete wire format and makes protocols
/// interchangeable.
pub trait PromptProtocol: Send + Sync {
    /// Identifier name of the protocol (for example `"xml"` or `"structured"`).
    fn name(&self) -> &'static str;

    /// Declarative set of interaction capabilities this protocol enables.
    ///
    /// The core pipeline relies on this set to know which command kinds may
    /// appear in decoded output.
    fn capabilities(&self) -> &[LlmCapability];

    /// Reports whether the protocol enables the given capability.
    fn supports(&self, capability: LlmCapability) -> bool {
        self.capabilities().contains(&capability)
    }

    /// Decodes raw model output into normalized core commands.
    ///
    /// Only commands corresponding to advertised capabilities are produced.
    fn decode(&self, raw_text: &str) -> Vec<LlmCommand>;

    /// Returns the active per-turn marker bound to this protocol, if any.
    ///
    /// Marker-bound protocols tag structural elements with a unique marker so
    /// tag extraction stays unambiguous across concatenated turns. Protocols
    /// without marker-bound syntax return `None`.
    fn turn_marker(&self) -> Option<&str> {
        None
    }

    /// Returns a marker-bound variant of this protocol.
    ///
    /// Protocols that tag their output with a marker produce a bound variant;
    /// others return `None` and keep the unbound protocol.
    fn with_turn_marker(&self, _marker: &str) -> Option<Box<dyn PromptProtocol>> {
        None
    }

    /// Returns the structured response format this protocol requires, if any.
    ///
    /// Structured-output protocols advertise their JSON schema here; free-form
    /// protocols return `None`.
    fn response_format(
        &self,
        _editable_paths: &[String],
    ) -> Option<crate::providers::ResponseFormat> {
        None
    }

    /// Creates a streaming filter that intercepts code edits in real time.
    ///
    /// The default reuses the reference edit protocol resolved from
    /// [`PromptProtocol::name`], so protocols without custom streaming reuse the
    /// shared edit-parsing machinery.
    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
        create_edit_protocol(self.name()).create_stream_filter(
            editable_paths,
            repo_root,
            staged_contents,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal protocol used to exercise the trait contract in tests.
    struct TextOnlyProtocol;

    impl PromptProtocol for TextOnlyProtocol {
        fn name(&self) -> &'static str {
            "text-only"
        }

        fn capabilities(&self) -> &[LlmCapability] {
            &[LlmCapability::TextMessage, LlmCapability::UserLanguage]
        }

        fn decode(&self, raw_text: &str) -> Vec<LlmCommand> {
            vec![LlmCommand::Text {
                content: raw_text.to_string(),
            }]
        }
    }

    #[test]
    fn test_prompt_protocol_reports_capabilities() {
        let protocol = TextOnlyProtocol;
        assert_eq!(protocol.name(), "text-only");
        assert!(protocol.supports(LlmCapability::TextMessage));
        assert!(protocol.supports(LlmCapability::UserLanguage));
        assert!(!protocol.supports(LlmCapability::FileEdits));
    }

    #[test]
    fn test_prompt_protocol_decodes_commands() {
        let protocol = TextOnlyProtocol;
        let commands = protocol.decode("Hello");
        assert_eq!(
            commands,
            vec![LlmCommand::Text {
                content: "Hello".to_string(),
            }]
        );
    }
}
