//! Parser for the internal documentation request control tag.
//!
//! The channel publishes one canonical spelling, `tauqe_doc_request`, and keeps
//! its pre-canonical spelling, `doc_request`, as a permanent alias, so a
//! transcript or a cached response written before the rename keeps decoding.
//! Every reader of this module resolves both declared specifications and merges
//! their matches in text order.

use tauqe_protocol::DocTopic;

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Base names of the documentation request tag: the canonical spelling first,
/// followed by the permanent alias.
const DOC_REQUEST_BASES: [&str; 2] = ["tauqe_doc_request", "doc_request"];

/// Every declared specification of the documentation request channel, the
/// canonical spelling first.
///
/// A spelling the tag registry does not declare contributes nothing, so a
/// reader never invents a tag the decoder would reject.
fn tag_specs() -> Vec<&'static XmlTagSpec> {
    DOC_REQUEST_BASES
        .iter()
        .filter_map(|base| engine::tag_spec(base))
        .collect()
}

/// Removes every documentation request tag from text, whichever spelling it
/// carries.
pub fn strip_doc_request_tags(text: &str) -> String {
    let mut stripped = text.to_string();
    for spec in tag_specs() {
        stripped = engine::strip_tag_by_spec(&stripped, spec);
    }
    stripped
}

/// Returns the requested documentation topics, reusing a precomputed markdown
/// code index. An unknown, empty, or missing topic resolves to [`DocTopic::All`]
/// via [`DocTopic::parse_lossy`], honoring Postel's law at the model-output
/// ingest gateway.
pub(crate) fn parse_doc_request_tags_with_fences(
    text: &str,
    fences: &super::CodeFences,
) -> Vec<DocTopic> {
    let mut found: Vec<(usize, DocTopic)> = Vec::new();
    for spec in tag_specs() {
        found.extend(
            engine::iter_tags_with_fences(text, spec, fences).map(|tag| {
                let topic = tag
                    .attr("topic")
                    .map(|topic| DocTopic::parse_lossy(&topic))
                    .unwrap_or(DocTopic::All);
                (tag.start, topic)
            }),
        );
    }
    found.sort_by_key(|entry| entry.0);
    found.into_iter().map(|(_, topic)| topic).collect()
}

/// Returns the requested documentation topics of both spellings; an unknown,
/// empty, or missing topic resolves to [`DocTopic::All`] via
/// [`DocTopic::parse_lossy`], honoring Postel's law at the model-output ingest
/// gateway.
pub(crate) fn parse_doc_request_tags(text: &str) -> Vec<DocTopic> {
    parse_doc_request_tags_with_fences(text, &super::CodeFences::find(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_doc_request_tags() {
        let text = "Need docs.\n<tauqe_doc_request topic=\"Interface\" />\n<doc_request_AB12 />";
        assert_eq!(
            parse_doc_request_tags(text),
            vec![DocTopic::Interface, DocTopic::All]
        );
    }

    #[test]
    fn test_parse_doc_request_tags_merges_both_spellings_in_text_order() {
        let text = concat!(
            "<doc_request topic=\"git\" />\n",
            "<tauqe_doc_request topic=\"plan\" />\n",
            "<doc_request topic=\"review\" />",
        );
        assert_eq!(
            parse_doc_request_tags(text),
            vec![DocTopic::Git, DocTopic::Plan, DocTopic::Review]
        );
    }

    #[test]
    fn test_parse_doc_request_unknown_topic_falls_back_to_all() {
        let text = "<tauqe_doc_request topic=\"bogus\" />";
        assert_eq!(parse_doc_request_tags(text), vec![DocTopic::All]);
    }

    #[test]
    fn test_strip_doc_request_tags_removes_both_spellings() {
        let text = "Intro\n<tauqe_doc_request topic=\"git\" />\nMiddle\n<doc_request topic=\"plan\" />\nOutro";
        assert_eq!(strip_doc_request_tags(text), "Intro\n\nMiddle\n\nOutro");
    }

    #[test]
    fn test_parse_doc_request_ignores_code_fences_and_inline_backticks() {
        let doc_text =
            "See `<doc_request topic=\"git\" />`.\n<tauqe_doc_request topic=\"interface\" />";
        let topics = parse_doc_request_tags(doc_text);
        assert_eq!(topics, vec![DocTopic::Interface]);
    }
}
