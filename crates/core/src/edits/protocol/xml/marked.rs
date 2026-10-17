//! Turn-marker-bound XML edit protocol.
//!
//! [`MarkedXmlEditProtocol`] wraps [`XmlEditProtocol`] with a per-turn marker so
//! structural tags stay unambiguous across concatenated turns. It also tolerates
//! reasoning models that drop the marker suffix by retrying unmarked tags.

use crate::edits::protocol::ContextRequest;
use tauqe_protocol::{DocTopic, EditOperation, ModelResult};

use super::clean::extract_conversational_text;
use super::marker::{normalize_marked_xml, restore_xml_literals};
use super::prompt;
use super::tags::{self, TerminalOutcome, VerifyRequest};
use super::XmlEditProtocol;

/// Reports whether `marker` is a syntactically valid turn marker.
///
/// A valid marker is a non-empty ASCII alphanumeric token, the exact shape
/// produced by [`generate_turn_marker`](super::generate_turn_marker). Callers
/// reject malformed markers by returning `None` or falling back to the unmarked
/// dialect instead of panicking, so a bad marker can never crash the long-lived
/// daemon.
pub(crate) fn is_valid_turn_marker(marker: &str) -> bool {
    !marker.is_empty() && marker.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[derive(Debug, Clone)]
pub struct MarkedXmlEditProtocol {
    marker: String,
}

impl MarkedXmlEditProtocol {
    /// Wraps a turn marker without validating its shape.
    ///
    /// Marker validation is enforced by
    /// [`with_turn_marker`](EditProtocol::with_turn_marker), which rejects a
    /// malformed marker by returning `None` instead of panicking.
    pub fn new(marker: &str) -> Self {
        Self { marker: marker.to_string() }
    }

    /// Runs a marker-normalized parse and retries against the raw text when the
    /// normalized pass yields nothing.
    ///
    /// The turn marker protects literal source tags from being mistaken for
    /// protocol commands. Reasoning models occasionally drop the suffix in long
    /// monologues, so when no marker-bound construct is found the raw text is
    /// parsed tolerantly, letting unmarked protocol tags still take effect.
    fn tolerant_parse<T>(
        &self,
        raw_text: &str,
        parse: impl Fn(&str) -> T,
        is_empty: impl Fn(&T) -> bool,
    ) -> T {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        let parsed = parse(&normalized);
        if is_empty(&parsed) {
            parse(raw_text)
        } else {
            parsed
        }
    }

    /// Returns the turn marker this protocol is bound to.
    pub fn turn_marker(&self) -> Option<&str> {
        Some(&self.marker)
    }

    /// Extracts context requests strictly bound to the active turn marker.
    ///
    /// Discovery requests mutate context and must not be triggered by unmarked
    /// example tags in conversational text or source code.
    pub fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::parse_context_request_tags(&normalized)
    }

    /// Extracts repository search requests strictly bound to the active turn marker.
    pub fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::parse_repo_search_tags(&normalized)
    }

    /// Extracts documentation topics strictly bound to the active turn marker.
    pub fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::parse_doc_request_tags(&normalized)
    }

    /// Extracts a verification request, tolerating an unmarked fallback.
    pub fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest> {
        self.tolerant_parse(raw_text, tags::extract_verify_request, Option::is_none)
    }

    /// Extracts the detected user language, tolerating an unmarked fallback.
    pub fn parse_user_language(&self, raw_text: &str) -> Option<String> {
        self.tolerant_parse(raw_text, tags::extract_user_language, Option::is_none)
    }

    /// Extracts plan tags strictly bound to the active turn marker.
    ///
    /// Plan modifications must never be parsed from unsuffixed tags discussed
    /// in conversation or documentation.
    pub fn parse_plan_tags(&self, raw_text: &str) -> (Vec<tags::ParsedPlanTag>, Vec<String>) {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::parse_plan_tags_with_diagnostics(&normalized)
    }

    /// Extracts scratchpad notes, preferring marker-bound notes over unmarked ones.
    ///
    /// Marker-normalized notes come first, then unmarked `<scratchpad>` notes so
    /// a reasoning model that dropped the suffix still keeps its working memory.
    pub fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::extract_scratchpad(&normalized)
            .map(|notes| restore_xml_literals(&notes, &self.marker))
            .or_else(|| tags::extract_scratchpad(raw_text))
    }

    /// Extracts the terminal outcome strictly bound to the active turn marker.
    ///
    /// Terminal outcomes determine control flow and must never fall back to
    /// unmarked tags quoted in source code or explanations.
    pub fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        let outcome = tags::extract_terminal_outcome(&normalized)?;
        Some(match outcome {
            TerminalOutcome::Success { summary } => TerminalOutcome::Success {
                summary: summary.map(|s| restore_xml_literals(&s, &self.marker)),
            },
            TerminalOutcome::Error { message } => TerminalOutcome::Error {
                message: restore_xml_literals(&message, &self.marker),
            },
            TerminalOutcome::Blocked { reason, details } => TerminalOutcome::Blocked {
                reason: restore_xml_literals(&reason, &self.marker),
                details: details.map(|d| restore_xml_literals(&d, &self.marker)),
            },
        })
    }

    /// Extracts a blocker or error message bound to the active turn marker.
    pub fn parse_error(&self, raw_text: &str) -> Option<String> {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        tags::extract_error(&normalized).map(|message| restore_xml_literals(&message, &self.marker))
    }

    /// Cleans model output, stripping terminal outcome and scratchpad tags.
    ///
    /// Both marker-suffixed and unmarked tags are removed (while respecting
    /// markdown code) so they never leak into conversational text.
    pub fn clean_assistant_text(&self, raw_text: &str) -> String {
        let without_terminal = tags::strip_terminal_outcome_tags(raw_text);
        let without_scratchpad = tags::strip_scratchpad_tags(&without_terminal);
        if !without_scratchpad.contains(&self.marker) {
            // Tolerant fallback: the model omitted the turn-marker suffix, so
            // unmarked tags are active protocol commands and are stripped too.
            return extract_conversational_text(&without_scratchpad);
        }
        let normalized = normalize_marked_xml(&without_scratchpad, &self.marker, true);
        restore_xml_literals(&extract_conversational_text(&normalized), &self.marker)
    }

    /// Returns system prompt instructions with the turn marker applied.
    pub fn system_instructions(&self, editable_paths: &[String]) -> String {
        prompt::apply_turn_marker(
            &prompt::base_system_instructions(editable_paths),
            &self.marker,
        )
    }

    /// Parses raw model text into a `ModelResult`, enforcing the turn marker.
    ///
    /// Code edits must carry the active turn marker. Unmarked edit blocks in
    /// conversation or source code are treated as literals, preventing injection
    /// or unintentional patch application.
    pub fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        let normalized = normalize_marked_xml(raw_text, &self.marker, true);
        let mut result = XmlEditProtocol.parse_output(&normalized, editable_paths);
        match &mut result {
            ModelResult::Answer { text } => *text = restore_xml_literals(text, &self.marker),
            ModelResult::Edit { summary, edits, proposal, outcome, .. } => {
                *summary = restore_xml_literals(summary, &self.marker);
                if let Some(message) = &mut outcome.error {
                    *message = restore_xml_literals(message, &self.marker);
                }
                for edit in edits {
                    match edit {
                        EditOperation::Replace { path, old_text, new_text } => {
                            *path = restore_xml_literals(path, &self.marker);
                            *old_text = restore_xml_literals(old_text, &self.marker);
                            *new_text = restore_xml_literals(new_text, &self.marker);
                        }
                        EditOperation::Create { path, content } => {
                            *path = restore_xml_literals(path, &self.marker);
                            *content = restore_xml_literals(content, &self.marker);
                        }
                        EditOperation::Overwrite { path, content } => {
                            *path = restore_xml_literals(path, &self.marker);
                            *content = restore_xml_literals(content, &self.marker);
                        }
                        EditOperation::Delete { path } => {
                            *path = restore_xml_literals(path, &self.marker);
                        }
                        EditOperation::Move { from, to } => {
                            *from = restore_xml_literals(from, &self.marker);
                            *to = restore_xml_literals(to, &self.marker);
                        }
                    }
                }
                let message =
                    restore_xml_literals(&extract_conversational_text(&normalized), &self.marker);
                *proposal = Some(tauqe_protocol::ModelResultProposal {
                    message,
                    ..Default::default()
                });
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::tags::{VerifyOnSuccess, VerifyTarget};
    use tauqe_protocol::{EditOperation, ModelResult};

    #[test]
    fn test_marked_xml_ignores_unsuffixed_plan_tags() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Here is an example in discussion: <plan action=\"update\" id=\"fake\"><item id=\"1\" /></plan>\n\
            And here is the real plan:\n\
            <plan_{m} action=\"save\" id=\"real-plan\" title=\"Real\">\n\
              <concept_{m}><concept_item_{m} id=\"c-1\" title=\"Concept\" /></concept_{m}>\n\
              <item id=\"1\" title=\"Step 1\" status=\"todo\" />\n\
            </plan_{m}>",
            m = marker
        );

        let (plans, errors) = proto.parse_plan_tags(&output);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "real-plan");
    }

    #[test]
    fn test_marked_xml_clean_assistant_text_preserves_unsuffixed_plan_literals() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "To update a plan, write: <plan action=\"update\" id=\"my-plan\"><item id=\"1\" status=\"done\" /></plan>\n\
            <plan_{m} action=\"save\" id=\"real-plan\" title=\"Real\">\n\
              <concept_{m}><concept_item_{m} id=\"c-1\" title=\"Concept\" /></concept_{m}>\n\
              <item id=\"1\" title=\"Step 1\" status=\"todo\" />\n\
            </plan_{m}>",
            m = marker
        );

        let cleaned = proto.clean_assistant_text(&output);
        assert!(cleaned.contains("<plan action=\"update\" id=\"my-plan\">"));
        assert!(!cleaned.contains("real-plan"));
    }

    #[test]
    fn test_marked_xml_scratchpad_extraction_and_cleaning() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Conversational response before.\n\
            <scratchpad_{m}>\n\
              Target: update tags.rs.\n\
              Verified: trait methods compiled.\n\
            </scratchpad_{m}>\n\
            Conversational response after.",
            m = marker
        );

        let scratchpad = proto.parse_scratchpad(&output);
        assert_eq!(
            scratchpad.as_deref(),
            Some("Target: update tags.rs.\nVerified: trait methods compiled.")
        );

        let cleaned = proto.clean_assistant_text(&output);
        assert_eq!(
            cleaned,
            "Conversational response before.\n\nConversational response after."
        );
    }

    #[test]
    fn test_marked_xml_clean_assistant_text_strips_unmarked_scratchpad() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = "Conversational response before.\n<scratchpad>\nUnmarked scratchpad note.\n</scratchpad>\nConversational response after.";
        let cleaned = proto.clean_assistant_text(output);
        assert_eq!(
            cleaned,
            "Conversational response before.\n\nConversational response after."
        );
    }

    #[test]
    fn test_marked_xml_preserves_unsuffixed_scratchpad_literals() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Here is documentation: `<scratchpad>example notes</scratchpad>`.\n\
            <scratchpad_{m}>\n\
              Real notes here.\n\
            </scratchpad_{m}>\n\
            All done.",
            m = marker
        );

        let scratchpad = proto.parse_scratchpad(&output);
        assert_eq!(scratchpad.as_deref(), Some("Real notes here."));

        let cleaned = proto.clean_assistant_text(&output);
        assert!(cleaned.contains("`<scratchpad>example notes</scratchpad>`"));
        assert!(!cleaned.contains("Real notes here."));
    }

    #[test]
    fn test_marked_xml_blocked_outcome_parsing_and_cleaning() {
        let marker = "M998877";
        let proto = MarkedXmlEditProtocol::new(marker);

        let blocked_output = format!(
            "Cannot fulfill step contract.\n\
            <blocked_{m} reason=\"Precondition violated\" details=\"File schema missing\" />\n\
            Conversational note.",
            m = marker
        );
        let outcome = proto.parse_terminal_outcome(&blocked_output).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Blocked {
                reason: "Precondition violated".to_string(),
                details: Some("File schema missing".to_string()),
            }
        );
        let cleaned = proto.clean_assistant_text(&blocked_output);
        assert!(!cleaned.contains("blocked"));
        assert_eq!(
            cleaned,
            "Cannot fulfill step contract.\n\nConversational note."
        );
    }

    #[test]
    fn test_marked_xml_terminal_outcome_parsing_and_cleaning() {
        let marker = "M456789";
        let proto = MarkedXmlEditProtocol::new(marker);

        let success_output = format!(
            "Implemented the feature.\n\
            <success_{m} summary=\"All unit tests pass cleanly\" />\n\
            Conversational outro.",
            m = marker
        );
        let outcome = proto.parse_terminal_outcome(&success_output).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Success {
                summary: Some("All unit tests pass cleanly".to_string()),
            }
        );
        let cleaned_success = proto.clean_assistant_text(&success_output);
        assert!(!cleaned_success.contains("success"));
        assert_eq!(
            cleaned_success,
            "Implemented the feature.\n\nConversational outro."
        );

        let error_output = format!(
            "Encountered blocker.\n\
            <error_{m} message=\"Missing dependency in workspace\" />\n\
            Explanation text.",
            m = marker
        );
        let err_outcome = proto.parse_terminal_outcome(&error_output).unwrap();
        assert_eq!(
            err_outcome,
            TerminalOutcome::Error {
                message: "Missing dependency in workspace".to_string(),
            }
        );
        assert_eq!(
            proto.parse_error(&error_output),
            Some("Missing dependency in workspace".to_string())
        );
        let cleaned_error = proto.clean_assistant_text(&error_output);
        assert!(!cleaned_error.contains("error"));
        assert_eq!(
            cleaned_error,
            "Encountered blocker.\n\nExplanation text."
        );
    }

    #[test]
    fn test_marked_xml_preserves_unsuffixed_terminal_literals() {
        let marker = "M456789";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Here is documentation: `<success>example</success>` and `<error message=\"x\" />`.\n\
            <success_{m} summary=\"Real outcome\" />\n\
            Done.",
            m = marker
        );

        let outcome = proto.parse_terminal_outcome(&output).unwrap();
        assert_eq!(
            outcome,
            TerminalOutcome::Success {
                summary: Some("Real outcome".to_string()),
            }
        );

        let cleaned = proto.clean_assistant_text(&output);
        assert!(cleaned.contains("`<success>example</success>`"));
        assert!(cleaned.contains("`<error message=\"x\" />`"));
        assert!(!cleaned.contains("Real outcome"));
    }

    #[test]
    fn test_marked_xml_ignores_unmarked_terminal_outcomes() {
        let marker = "M246801";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = concat!(
            "Discussing outcomes:\n",
            "<", "success summary=\"Example success\" />\n",
            "<", "error message=\"Example blocker\" />\n",
            "<", "blocked reason=\"Example reason\" />\n",
            "Done.",
        );
        assert_eq!(proto.parse_terminal_outcome(output), None);
        assert_eq!(proto.parse_error(output), None);
    }

    #[test]
    fn test_marked_xml_with_literal_tags_in_content() {
        let marker = "M12345";
        let proto = MarkedXmlEditProtocol::new(marker);
        let editable = vec!["src/test.rs".to_string()];

        let output = format!(
            "Explanation\n\
            <tauqe_edits_{m} summary=\"Fix tags\">\n  \
              <edit_{m} path=\"src/test.rs\">\n    \
                <search_{m}>\n\
let s = \"</search>\";\n\
<replace>\n\
                </search_{m}>\n    \
                <replace_{m}>\n\
let s = \"clean\";\n\
                </replace_{m}>\n  \
              </edit_{m}>\n\
            </tauqe_edits_{m}>",
            m = marker
        );

        let res = proto.parse_output(&output, &editable);
        match res {
            ModelResult::Edit { edits, .. } => {
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace { old_text, new_text, .. } => {
                        assert!(old_text.contains("let s = \"</search>\";\n<replace>"));
                        assert!(new_text.contains("let s = \"clean\";"));
                    }
                    _ => panic!("Expected Replace"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_marked_xml_scratchpad_falls_back_to_unmarked() {
        let marker = "M135790";
        let proto = MarkedXmlEditProtocol::new(marker);
        let output =
            "Working.\n<scratchpad>\nUnmarked note from a reasoning model.\n</scratchpad>\nDone.";
        assert_eq!(
            proto.parse_scratchpad(output).as_deref(),
            Some("Unmarked note from a reasoning model.")
        );
        assert_eq!(proto.clean_assistant_text(output), "Working.\n\nDone.");
    }

    #[test]
    fn test_marked_xml_ignores_unmarked_context_request() {
        let marker = "M246810";
        let proto = MarkedXmlEditProtocol::new(marker);
        let output = concat!(
            "Need context example:\n",
            "<", "context_request path=\"src/a.rs\" access=\"editable\" />\n",
            "Done.",
        );
        let requests = proto.parse_context_requests(output);
        assert!(requests.is_empty());
    }

    #[test]
    fn test_marked_xml_repo_search_requires_marker_and_ignores_unmarked() {
        let marker = "M987654";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Searching example in discussion: `<repo_search path=\"crates/core\" regex=\"fn old\" />`.\n\
            Real discovery search:\n\
            <repo_search_{m} path=\"crates/core\" regex=\"fn execute\" />\n\
            Done.",
            m = marker
        );

        let searches = proto.parse_repo_searches(&output);
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0].path, "crates/core");
        assert_eq!(searches[0].regex, "fn execute");

        let unmarked_only = "Discussion with <repo_search path=\"crates/server\" regex=\"fn run\" /> alone.";
        assert!(proto.parse_repo_searches(unmarked_only).is_empty());

        let cleaned = proto.clean_assistant_text(&output);
        assert!(cleaned.contains("`<repo_search path=\"crates/core\" regex=\"fn old\" />`"));
        assert!(!cleaned.contains("fn execute"));
    }

    #[test]
    fn test_marked_xml_verify_falls_back_to_unmarked() {
        let marker = "M246811";
        let proto = MarkedXmlEditProtocol::new(marker);
        let output = "Verify.\n<verify target=\"clippy\" on_success=\"silent\" />\nDone.";
        let verify = proto.parse_verify_request(output).unwrap();
        assert_eq!(verify.target, VerifyTarget::Clippy);
        assert_eq!(verify.on_success, VerifyOnSuccess::Silent);
    }

    #[test]
    fn test_marked_xml_ignores_unmarked_edits() {
        let marker = "M357911";
        let proto = MarkedXmlEditProtocol::new(marker);
        let editable = vec!["src/lib.rs".to_string()];
        let output = concat!(
            "Fixing example:\n",
            "<", "tauqe_edits summary=\"Fix\">\n",
            "<", "edit path=\"src/lib.rs\">\n",
            "<", "search>\nold();\n</search>\n",
            "<", "replace>\nnew();\n<", "/replace>\n",
            "<", "/edit>\n",
            "<", "/tauqe_edits>\n",
        );
        match proto.parse_output(output, &editable) {
            ModelResult::Answer { text } => {
                assert!(text.contains("Fixing example:"));
                assert!(text.contains("<tauqe_edits summary=\"Fix\">"));
            }
            ModelResult::Edit { .. } => panic!("Expected ModelResult::Answer, got Edit"),
        }
    }

    #[test]
    fn test_marked_xml_clean_assistant_text_omits_turn_marker() {
        // History stores cleaned assistant text; the turn marker must never
        // survive cleaning, otherwise a leaked marker could be replayed to
        // forge marker-bound protocol tags in a later turn.
        let marker = "M112233";
        let proto = MarkedXmlEditProtocol::new(marker);

        let output = format!(
            "Applying changes.\n\
            <tauqe_edits_{m} summary=\"Edit\">\n  \
              <edit_{m} path=\"src/lib.rs\">\n    \
                <search_{m}>\nold();\n</search_{m}>\n    \
                <replace_{m}>\nnew();\n</replace_{m}>\n  \
              </edit_{m}>\n\
            </tauqe_edits_{m}>\n\
            <success_{m} summary=\"Done\" />",
            m = marker
        );

        let cleaned = proto.clean_assistant_text(&output);
        assert!(cleaned.contains("Applying changes."));
        assert!(!cleaned.contains(marker));
    }
}
