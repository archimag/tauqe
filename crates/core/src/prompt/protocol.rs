//! Protocol-agnostic prompt decoding abstraction.
//!
//! A [`PromptProtocol`] decouples the concrete wire syntax of model output
//! (XML markers, JSON schema) from the core execution pipeline. Each protocol
//! advertises the declarative set of [`LlmCapability`] values it enables and
//! decodes raw model output into a uniform list of [`LlmCommand`] values. The
//! pipeline therefore operates purely on normalized semantics and stays
//! independent of any single protocol format.

use tauqe_protocol::{CapabilitySet, LlmCapability, LlmCommand};

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

    /// Declarative capability set this protocol currently advertises.
    ///
    /// The default derives the set from [`PromptProtocol::capabilities`]. A
    /// protocol that can be narrowed overrides it to report the narrowed set, so
    /// prompt composition and decoding stay aligned with one source of truth.
    fn capability_set(&self) -> CapabilitySet {
        CapabilitySet::from_slice(self.capabilities())
    }

    /// Returns a variant of this protocol that advertises exactly `capabilities`.
    ///
    /// Every concrete protocol implements this, so a caller narrows a turn to a
    /// capability set without branching on the concrete protocol type.
    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol>;

    /// Decodes raw model output into normalized core commands.
    ///
    /// Only commands corresponding to advertised capabilities are produced.
    fn decode(&self, raw_text: &str) -> Vec<LlmCommand>;

    /// Renders protocol-specific instructions appended to the system prompt.
    ///
    /// The active protocol renders its own guidance directly, so prompt assembly
    /// never reconstructs a protocol from its name.
    fn system_instructions(&self, editable_paths: &[String]) -> String;

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
    /// Each protocol constructs its own filter, so no protocol is ever
    /// reconstructed from its [`PromptProtocol::name`] string.
    fn create_stream_filter(
        &self,
        editable_paths: Vec<String>,
        repo_root: std::path::PathBuf,
        staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn crate::edits::stream::EditStreamFilter>;
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

        fn system_instructions(&self, _editable_paths: &[String]) -> String {
            String::new()
        }

        fn with_capability_set(&self, _capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
            // The fixture advertises a fixed capability slice; narrowing is
            // exercised by the concrete protocols.
            Box::new(TextOnlyProtocol)
        }

        fn create_stream_filter(
            &self,
            editable_paths: Vec<String>,
            repo_root: std::path::PathBuf,
            staged_contents: std::collections::HashMap<String, String>,
        ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
            Box::new(
                crate::edits::stream::XmlStreamFilter::new(editable_paths, repo_root)
                    .with_staged_contents(staged_contents),
            )
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
