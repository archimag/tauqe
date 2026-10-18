//! XML edit protocol facade.
//!
//! [`XmlEditProtocol`] implements the XML code-modification contour. Each
//! supporting concern lives in a focused submodule: [`prompt`] renders the
//! protocol reference, [`parser`] decodes edit blocks, [`clean`] strips protocol
//! syntax from conversational text, and [`marked`] binds the protocol to a
//! per-turn marker. Control tags that are not code edits live in [`tags`].

use tauqe_protocol::{DocTopic, EditOutcome, ModelResult};

use super::ContextRequest;

mod clean;
pub(crate) mod engine;
mod marker;
mod marked;
mod parser;
mod prompt;
pub(crate) mod tags;

use engine::strip_all_control_tags;
use parser::{extract_summary_from_xml, parse_xml_edits};
use tags::{parse_context_request_tags, parse_doc_request_tags, parse_repo_search_tags};

pub use clean::extract_conversational_text;
pub use marker::generate_turn_marker;
pub use marked::MarkedXmlEditProtocol;
pub use parser::has_xml_edit_tags;
pub(crate) use marker::{
    normalize_marked_xml, normalize_marked_xml_incremental, restore_xml_literals,
};
pub(crate) use marked::is_valid_turn_marker;
pub(crate) use parser::{extract_attribute, find_base_tag_start};
pub use tags::{
    extract_blocked, extract_error, extract_scratchpad, extract_skip, extract_success,
    extract_terminal_outcome, extract_user_language, extract_verify_request,
    strip_blocked_tags, strip_context_request_tags, strip_doc_request_tags,
    strip_error_tags, strip_plan_tags, strip_repo_search_tags, strip_scratchpad_tags,
    strip_skip_tags, strip_success_tags, strip_terminal_outcome_tags, strip_user_language_tags,
    strip_verify_tags, TerminalOutcome, VerifyOnSuccess, VerifyRequest, VerifyTarget,
};
/// XML-based code edit protocol
#[derive(Debug, Default, Clone)]
pub struct XmlEditProtocol;

impl XmlEditProtocol {
    /// Extracts context requests (files the model wants added to context).
    pub fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        parse_context_request_tags(raw_text)
    }

    /// Extracts repository search requests (`<repo_search path="..." regex="..." />`).
    pub fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest> {
        parse_repo_search_tags(raw_text)
    }

    /// Extracts requested documentation topics (`All` when unspecified).
    pub fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic> {
        parse_doc_request_tags(raw_text)
    }

    /// Extracts a code verification request (check/clippy/test) when present.
    pub fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest> {
        tags::extract_verify_request(raw_text)
    }

    /// Extracts the detected user language when present.
    pub fn parse_user_language(&self, raw_text: &str) -> Option<String> {
        tags::extract_user_language(raw_text)
    }

    /// Extracts plan tags and validation diagnostics from raw output.
    pub fn parse_plan_tags(&self, raw_text: &str) -> (Vec<tags::ParsedPlanTag>, Vec<String>) {
        tags::parse_plan_tags_with_diagnostics(raw_text)
    }

    /// Extracts ephemeral scratchpad notes (`<scratchpad>`) when present.
    pub fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        tags::extract_scratchpad(raw_text)
    }

    /// Extracts the terminal outcome (`<success>`, `<error>`, `<blocked>`) when present.
    pub fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome> {
        tags::extract_terminal_outcome(raw_text)
    }

    /// Extracts a blocker or error message (`<error>`) when present.
    pub fn parse_error(&self, raw_text: &str) -> Option<String> {
        tags::extract_error(raw_text)
    }

    /// Extracts a skip reason (`<skip>`) when present.
    pub fn parse_skip(&self, raw_text: &str) -> Option<String> {
        tags::extract_skip(raw_text)
    }

    /// Cleans model output for presentation and history by stripping protocol control tags.
    pub fn clean_assistant_text(&self, raw_text: &str) -> String {
        extract_conversational_text(raw_text)
    }

    /// Returns system prompt instructions tailored for this edit protocol.
    pub fn system_instructions(&self, editable_paths: &[String]) -> String {
        prompt::base_system_instructions(editable_paths)
    }

    /// Returns system prompt instructions for an explicit capability set.
    ///
    /// The reference is composed from the capability registry, so a narrowed set
    /// never advertises an instruction for a disabled capability. An optional
    /// turn marker binds every rendered structural tag to the active turn.
    pub fn system_instructions_for(
        &self,
        capabilities: &tauqe_protocol::CapabilitySet,
        editable_paths: &[String],
        marker: Option<&str>,
    ) -> String {
        let reference = prompt::system_instructions_for(capabilities, editable_paths);
        match marker {
            Some(marker) => prompt::apply_turn_marker(&reference, marker),
            None => reference,
        }
    }

    /// Parses raw model text into a `ModelResult`.
    pub fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        if !has_xml_edit_tags(raw_text) {
            return ModelResult::Answer {
                text: strip_all_control_tags(raw_text),
            };
        }

        let extracted_summary = extract_summary_from_xml(raw_text);

        match parse_xml_edits(raw_text, editable_paths) {
            Ok(edits) => {
                if edits.is_empty() {
                    ModelResult::Answer {
                        text: raw_text.to_string(),
                    }
                } else {
                    let summary =
                        extracted_summary.unwrap_or_else(|| "Apply AI code changes".to_string());
                    ModelResult::Edit {
                        summary,
                        edits,
                        proposal: Some(tauqe_protocol::ModelResultProposal {
                            message: extract_conversational_text(raw_text),
                            ..Default::default()
                        }),
                        outcome: EditOutcome {
                            applied: false,
                            error: None,
                            changed_files: Vec::new(),
                            commit_hash: None,
                        },
                    }
                }
            }
            Err(err_msg) => ModelResult::Edit {
                summary: "Malformed XML edit block".to_string(),
                edits: Vec::new(),
                proposal: None,
                outcome: EditOutcome {
                    applied: false,
                    error: Some(err_msg),
                    changed_files: Vec::new(),
                    commit_hash: None,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{EditOperation, ModelResult};

    #[test]
    fn test_xml_protocol_edit_with_summary_attribute() {
        let proto = XmlEditProtocol;
        let editable = vec!["crates/core/src/lib.rs".to_string()];

        let output = r#"I have updated the code:

<tauqe_edits summary="Refactor core initialization">
  <edit path="crates/core/src/lib.rs">
    <search>
pub fn old_fn() {}
    </search>
    <replace>
pub fn new_fn() {}
    </replace>
  </edit>
</tauqe_edits>
"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Refactor core initialization");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_strip_context_request_tags_suffixed() {
        let raw = "Intro\n<context_request_A1B2C3 path=\"src/a.rs\" access=\"read_only\" />\nMiddle\n<context_request_A1B2C3 path=\"src/b.rs\"></context_request_A1B2C3>\nOutro";
        let stripped = strip_context_request_tags(raw);
        assert_eq!(stripped.trim(), "Intro\n\nMiddle\n\nOutro");
    }

    #[test]
    fn test_extract_and_strip_skip_request() {
        let text = "Not applicable here.\n<skip reason=\"Finding does not apply to this crate\" />\nDone.";
        assert_eq!(
            XmlEditProtocol.parse_skip(text),
            Some("Finding does not apply to this crate".to_string())
        );
        assert_eq!(
            strip_skip_tags(text).trim(),
            "Not applicable here.\n\nDone."
        );

        // The shared terminal strip keeps `<skip>` for the review contour.
        assert!(strip_terminal_outcome_tags(text).contains("<skip"));
    }

    #[test]
    fn test_extract_and_strip_verify_request() {
        let text = "Here are changes.\n<verify target=\"clippy\" on_success=\"silent\" />\nAll done.";
        let req = extract_verify_request(text).unwrap();
        assert_eq!(req.target, VerifyTarget::Clippy);
        assert_eq!(req.on_success, VerifyOnSuccess::Silent);
        assert_eq!(
            strip_verify_tags(text).trim(),
            "Here are changes.\n\nAll done."
        );
    }

    #[test]
    fn test_extract_and_strip_user_language() {
        let text = "Hello!\n<user_language>Russian</user_language>\nHere is my explanation.";
        assert_eq!(extract_user_language(text), Some("Russian".to_string()));
        assert_eq!(
            strip_user_language_tags(text).trim(),
            "Hello!\n\nHere is my explanation."
        );

        let suffixed = "Hi\n<user_language_ABC123>en</user_language_ABC123>\nDone.";
        assert_eq!(extract_user_language(suffixed), Some("en".to_string()));
        assert_eq!(strip_user_language_tags(suffixed).trim(), "Hi\n\nDone.");
    }

    #[test]
    fn test_xml_protocol_overwrite_parse() {
        let editable = vec!["src/lib.rs".to_string()];
        let output = "Rewriting.\n<tauqe_edits summary=\"Rewrite lib\">\n<overwrite path=\"src/lib.rs\">\npub fn all_new() {}\n</overwrite>\n</tauqe_edits>";
        match XmlEditProtocol.parse_output(output, &editable) {
            ModelResult::Edit { edits, outcome, .. } if outcome.error.is_none() => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Overwrite { path, content } => {
                        assert_eq!(path, "src/lib.rs");
                        assert!(content.contains("pub fn all_new() {}"));
                    }
                    _ => panic!("Expected Overwrite"),
                }
            }
            _ => panic!("Expected valid edit"),
        }
    }

    #[test]
    fn test_xml_protocol_edit_with_closing_edit_literal_in_content() {
        let proto = XmlEditProtocol;
        let editable = vec!["src/test.rs".to_string()];

        let output = r#"Explanation
<tauqe_edits summary="Fix tags">
  <edit path="src/test.rs">
    <search>
let s = "</edit>";
    </search>
    <replace>
let s = "clean </edit>";
    </replace>
  </edit>
</tauqe_edits>"#;

        let res = proto.parse_output(output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace { old_text, new_text, .. } => {
                        assert!(old_text.contains("let s = \"</edit>\";"));
                        assert!(new_text.contains("let s = \"clean </edit>\";"));
                    }
                    _ => panic!("Expected Replace"),
                }
            }
            _ => panic!("Expected ModelResult::Edit, got: {:?}", res),
        }
    }
}
