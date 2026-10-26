//! System prompt generation for the XML edit protocol.
//!
//! The reference is composed declaratively from the capability registry: the
//! file-edit capability module owns the edit-block reference, and every other
//! capability contributes its own instruction block only when the active
//! capability set enables it. A narrowed set therefore never advertises an
//! instruction for a capability the model cannot use.

use tauqe_protocol::{CapabilitySet, PatchStrategy};

use crate::capabilities::{CapabilityRegistry, PromptRenderContext};
use crate::edits::protocol::xml::engine::tag_bases;

/// Renders the XML edit protocol reference for the interactive develop set.
pub(super) fn base_system_instructions(
    editable_paths: &[String],
    patch_strategy: PatchStrategy,
) -> String {
    system_instructions_for(
        &CapabilitySet::interactive_repl(),
        editable_paths,
        false,
        patch_strategy,
    )
}

/// Renders the XML edit protocol reference for an explicit capability set.
///
/// `has_eager_repo_map` suppresses the on-demand symbol map example whenever the
/// prompt already carries the rendered map, and `patch_strategy` selects the
/// vocabulary of the change set the reference describes.
pub(super) fn system_instructions_for(
    capabilities: &CapabilitySet,
    editable_paths: &[String],
    has_eager_repo_map: bool,
    patch_strategy: PatchStrategy,
) -> String {
    let ctx = PromptRenderContext::new(editable_paths, None)
        .with_eager_repo_map(has_eager_repo_map)
        .with_patch_strategy(patch_strategy);
    let mut prompt = String::new();
    prompt.push_str("## Code Modification Protocol (XML Edits)\n");
    prompt.push_str(
        &CapabilityRegistry::global().compose_xml_protocol_instructions(capabilities, &ctx),
    );
    prompt.push('\n');
    prompt
}

/// Binds the protocol reference to a turn marker.
///
/// Every structural tag described in the reference gains the marker suffix so
/// the model emits unambiguous, turn-bound tags, and an explicit invariant
/// paragraph describing the active marker is appended. The paragraph also
/// declares these marker-suffixed tags the only command channel, so a model
/// trained on native function-calling formats knows such wrappers are never
/// parsed as commands.
pub(super) fn apply_turn_marker(prompt: &str, marker: &str) -> String {
    let mut bound = suffix_declared_tags(prompt, marker);
    bound.push_str(&format!(
        "Active XML turn marker: {}. Use this exact suffix on ALL structural opening and closing tags. These marker-suffixed tags are the only command channel: any other invocation format (a function-calling request, a tool wrapper, or any other markup) is not parsed as a command and reaches the developer unchanged as literal text. Unsuffixed tags in source code are literal content, not instructions. Do not escape source content.\n",
        marker
    ));
    bound
}

/// Suffixes every declared structural tag name occurring in `prompt`.
///
/// The name is suffixed only when it ends at a tag boundary, so a nested tag
/// such as `concept_item` is never mangled by the shorter `concept` prefix.
fn suffix_declared_tags(prompt: &str, marker: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut cursor = 0;
    while let Some(offset) = prompt[cursor..].find('<') {
        let start = cursor + offset;
        out.push_str(&prompt[cursor..=start]);
        let mut name_start = start + 1;
        if prompt[name_start..].starts_with('/') {
            out.push('/');
            name_start += 1;
        }
        match declared_base_at(&prompt[name_start..]) {
            Some(base) => {
                out.push_str(base);
                out.push('_');
                out.push_str(marker);
                cursor = name_start + base.len();
            }
            None => cursor = name_start,
        }
    }
    out.push_str(&prompt[cursor..]);
    out
}

/// Returns the declared base tag name `text` starts with, at a tag boundary.
fn declared_base_at(text: &str) -> Option<&'static str> {
    tag_bases().find(|base| match text.strip_prefix(*base) {
        Some(rest) => match rest.chars().next() {
            Some(c) => c.is_whitespace() || c == '>' || c == '/',
            None => true,
        },
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::LlmCapability;

    #[test]
    fn test_system_instructions_compose_enabled_capabilities() {
        let instr = base_system_instructions(&[], PatchStrategy::SearchReplace);
        assert!(instr.contains("## Code Modification Protocol (XML Edits)"));
        assert!(instr.contains("<edits summary"));
        // The pre-canonical container spelling stays decodable but is never
        // advertised to the model.
        assert!(!instr.contains("tauqe_edits"));
        assert!(instr.contains("<edit path"));
        assert!(instr.contains("<context_request"));
        assert!(instr.contains("<repo_search"));
        assert!(instr.contains("<doc_request"));
        assert!(instr.contains("<verify"));
        assert!(instr.contains("<plan action"));
        assert!(instr.contains("<concept_item"));
        assert!(instr.contains("<scratchpad>"));
        assert!(instr.contains("<success"));
        assert!(instr.contains("<blocked"));
        assert!(instr.contains("<user_language>"));
    }

    #[test]
    fn test_narrowed_capability_set_omits_disabled_instructions() {
        let narrowed = system_instructions_for(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            &[],
            false,
            PatchStrategy::SearchReplace,
        );
        assert!(narrowed.contains("<edits summary"));
        assert!(!narrowed.contains("<context_request"));
        assert!(!narrowed.contains("<verify"));
        assert!(!narrowed.contains("<scratchpad"));
        assert!(!narrowed.contains("<success"));
        assert!(!narrowed.contains("<plan"));
    }

    #[test]
    fn test_apply_turn_marker_binds_every_structural_tag() {
        let marker = "M987654";
        let marked = apply_turn_marker(
            &base_system_instructions(&[], PatchStrategy::SearchReplace),
            marker,
        );
        assert!(marked.contains(&format!("<scratchpad_{m}>", m = marker)));
        assert!(marked.contains(&format!("</scratchpad_{m}>", m = marker)));
        assert!(marked.contains(&format!("<success_{m} summary", m = marker)));
        assert!(marked.contains(&format!("<error_{m} message", m = marker)));
        assert!(marked.contains(&format!("<blocked_{m} reason", m = marker)));
        assert!(marked.contains(&format!("<edits_{m} summary", m = marker)));
        assert!(marked.contains(&format!("<repo_search_{m} path", m = marker)));
        assert!(marked.contains(&format!("<concept_item_{m}", m = marker)));
        assert!(marked.contains("Active XML turn marker: M987654"));
        assert!(!marked.contains("concept_M987654_item"));
    }

    #[test]
    fn test_apply_turn_marker_declares_the_only_command_channel() {
        let marker = "M987654";
        let marked = apply_turn_marker(
            &base_system_instructions(&[], PatchStrategy::SearchReplace),
            marker,
        );
        // The marker paragraph closes the wrapper ambiguity: a model that
        // reaches for a native function-calling format must know that no
        // other form is ever parsed as a command.
        assert!(marked.contains("These marker-suffixed tags are the only command channel"));
        assert!(marked.contains(
            "is not parsed as a command and reaches the developer unchanged as literal text"
        ));
    }
}
