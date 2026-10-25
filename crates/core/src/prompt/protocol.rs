//! Protocol-agnostic prompt decoding abstraction.
//!
//! A [`PromptProtocol`] decouples the concrete wire syntax of model output
//! (XML markers, JSON schema) from the core execution pipeline. Each protocol
//! advertises the declarative set of [`LlmCapability`] values it enables and
//! decodes raw model output into a uniform list of [`LlmCommand`] values. The
//! pipeline therefore operates purely on normalized semantics and stays
//! independent of any single protocol format.

use tauqe_protocol::{CapabilitySet, EditProtocolKind, LlmCapability, LlmCommand};

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

    /// Typed wire format this protocol serializes.
    ///
    /// The format is a transport fact, never a capability decision: prompt
    /// composition reads it only to keep the rendered contract free of the
    /// other contour's spellings, so a structured prompt advertises no control
    /// tag while an XML prompt advertises no JSON field.
    fn kind(&self) -> EditProtocolKind;

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
    /// `context_paths` lists the files an edit may address this turn: every file
    /// present in `<context>`, because a file marked read-only becomes editable
    /// as soon as an edit proposes a change to it. Resolution therefore happens
    /// at the decode boundary against the same list the streaming filter
    /// received, so a preview and its decoded command never name different
    /// files.
    ///
    /// Only commands corresponding to advertised capabilities are produced.
    fn decode(&self, raw_text: &str, context_paths: &[String]) -> Vec<LlmCommand>;

    /// Renders protocol-specific instructions appended to the system prompt.
    ///
    /// The active protocol renders its own guidance directly, so prompt assembly
    /// never reconstructs a protocol from its name and never renders mode
    /// instructions of its own.
    ///
    /// An implementor renders the instructions of the wire surface its narrowed
    /// capability set selects: the edit reference when
    /// [`LlmCapability::FileEdits`] is enabled, and the read-only discussion
    /// schema otherwise. The narrowed [`CapabilitySet`] is the single typed
    /// input carrying that decision — the caller narrows through
    /// `TurnIntent::narrow_capabilities` and [`PromptProtocol::with_capability_set`]
    /// before calling — so no parallel intent channel exists on this trait and
    /// a second source of truth cannot appear.
    ///
    /// `has_eager_repo_map` reports whether the prompt already carries the
    /// rendered repository symbol map, so the guidance never advertises an
    /// on-demand map request for contours the model can already read.
    fn system_instructions(&self, editable_paths: &[String], has_eager_repo_map: bool) -> String;

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

        fn kind(&self) -> EditProtocolKind {
            EditProtocolKind::Structured
        }

        fn capabilities(&self) -> &[LlmCapability] {
            &[LlmCapability::TextMessage, LlmCapability::UserLanguage]
        }

        fn decode(&self, raw_text: &str, _context_paths: &[String]) -> Vec<LlmCommand> {
            vec![LlmCommand::Text {
                content: raw_text.to_string(),
            }]
        }

        fn system_instructions(
            &self,
            _editable_paths: &[String],
            _has_eager_repo_map: bool,
        ) -> String {
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
        let commands = protocol.decode("Hello", &[]);
        assert_eq!(
            commands,
            vec![LlmCommand::Text {
                content: "Hello".to_string(),
            }]
        );
    }
}
