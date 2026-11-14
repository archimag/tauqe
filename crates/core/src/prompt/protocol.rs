//! Protocol-agnostic prompt decoding abstraction.
//!
//! A [`PromptProtocol`] decouples the concrete wire syntax of model output
//! (XML markers, JSON schema) from the core execution pipeline. Each protocol
//! advertises the declarative capability tree it enables and
//! decodes raw model output into a uniform list of [`LlmCommand`] values. The
//! pipeline therefore operates purely on normalized semantics and stays
//! independent of any single protocol format.

use tauqe_protocol::{
    CapabilityExpr, EditProtocolKind, LlmCapability, LlmCommand, PatchStrategy, ProjectConfig,
};

/// Decodes raw model output into normalized core commands.
///
/// Implementors translate their protocol-specific syntax into the shared
/// [`LlmCommand`] vocabulary, emitting commands only for the capabilities they
/// advertise through [`PromptProtocol::capability_expr`]. This keeps the execution
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

    /// Strict capability tree this protocol advertises.
    ///
    /// The tree is the single source of truth of the protocol surface: it
    /// composes the response schema and scopes the decoder, so a capability can
    /// neither be advertised while remaining undecodable nor accepted while
    /// being hidden from the instructions.
    fn capability_expr(&self) -> CapabilityExpr;

    /// Flat projection of the advertised tree, in first-seen depth-first order.
    ///
    /// The projection answers which capability kinds a round may emit; the tree
    /// stays the authoritative declaration the schema and the decoder read.
    fn capabilities(&self) -> Vec<LlmCapability> {
        self.capability_expr().leaves()
    }

    /// Reports whether the protocol enables the given capability.
    fn supports(&self, capability: LlmCapability) -> bool {
        self.capability_expr().contains(capability)
    }

    /// Returns a variant of this protocol that advertises exactly `expr`.
    ///
    /// Every concrete protocol implements this, so a caller narrows a turn to a
    /// capability tree without branching on the concrete protocol type.
    fn with_capability_expr(&self, expr: CapabilityExpr) -> Box<dyn PromptProtocol>;

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
    /// capability set selects: the edit reference when a file-edit capability is
    /// enabled, and the read-only discussion schema otherwise. The narrowed
    /// capability tree is the single typed input carrying that decision — the
    /// caller narrows the surface and hands it to
    /// [`PromptProtocol::with_capability_expr`] before calling — so no parallel
    /// regime channel exists on this trait and a second source of truth cannot
    /// appear.
    ///
    /// `has_eager_repo_map` reports whether the prompt already carries the
    /// rendered repository symbol map, so the guidance never advertises an
    /// on-demand map request for contours the model can already read.
    ///
    /// `patch_strategy` carries the strategy that decides how a change set
    /// addresses the lines it replaces, so the rendered reference advertises
    /// exactly the fields the active decoder accepts.
    fn system_instructions(
        &self,
        editable_paths: &[String],
        has_eager_repo_map: bool,
        patch_strategy: PatchStrategy,
    ) -> String;

    /// Returns the structured response format this protocol requires, if any.
    ///
    /// Structured-output protocols advertise their JSON schema here; free-form
    /// protocols return `None`.
    ///
    /// The schema is a projection of the enabled capability set, the declared
    /// patch strategy, and the project command declaration `project`, never of
    /// the permitted paths: the strategy decides how a change set addresses the
    /// lines it replaces, and the declaration decides whether the project
    /// command surface exists at all, while the field names are fixed before any
    /// file enters the context. The paths travel through the protocol contract
    /// without reaching the schema.
    fn response_format(
        &self,
        _editable_paths: &[String],
        _patch_strategy: PatchStrategy,
        _project: Option<&ProjectConfig>,
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
    use tauqe_protocol::TextCommand;

    /// Minimal protocol used to exercise the trait contract in tests.
    struct TextOnlyProtocol;

    impl PromptProtocol for TextOnlyProtocol {
        fn name(&self) -> &'static str {
            "text-only"
        }

        fn kind(&self) -> EditProtocolKind {
            EditProtocolKind::Structured
        }

        fn capability_expr(&self) -> CapabilityExpr {
            CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::TextMessage)])
        }

        fn decode(&self, raw_text: &str, _context_paths: &[String]) -> Vec<LlmCommand> {
            vec![LlmCommand::Text(TextCommand {
                content: raw_text.to_string(),
            })]
        }

        fn system_instructions(
            &self,
            _editable_paths: &[String],
            _has_eager_repo_map: bool,
            _patch_strategy: PatchStrategy,
        ) -> String {
            String::new()
        }

        fn with_capability_expr(&self, _expr: CapabilityExpr) -> Box<dyn PromptProtocol> {
            // The fixture advertises a fixed capability tree; narrowing is
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
                crate::edits::stream::JsonStreamFilter::new(editable_paths, repo_root)
                    .with_staged_contents(staged_contents),
            )
        }
    }

    #[test]
    fn test_prompt_protocol_reports_capabilities() {
        let protocol = TextOnlyProtocol;
        assert_eq!(protocol.name(), "text-only");
        assert!(protocol.supports(LlmCapability::TextMessage));
        assert!(!protocol.supports(LlmCapability::EditReplace));
    }
    #[test]
    fn test_prompt_protocol_decodes_commands() {
        let protocol = TextOnlyProtocol;
        let commands = protocol.decode("Hello", &[]);
        assert_eq!(
            commands,
            vec![LlmCommand::Text(TextCommand {
                content: "Hello".to_string(),
            })]
        );
    }
}
