//! Parser for the `<doc_request>` control tag.

use tauqe_protocol::DocTopic;

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Specification of the `<doc_request>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("doc_request")
}

/// Removes all documentation request tags from text.
pub fn strip_doc_request_tags(text: &str) -> String {
    match tag_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

/// Returns the requested documentation topics; an unknown, empty, or missing
/// topic resolves to [`DocTopic::All`] via [`DocTopic::parse_lossy`], honoring
/// Postel's law at the model-output ingest gateway.
pub(crate) fn parse_doc_request_tags(text: &str) -> Vec<DocTopic> {
    let Some(spec) = tag_spec() else {
        return Vec::new();
    };
    engine::iter_tags(text, spec)
        .map(|found| {
            found
                .attr("topic")
                .map(|topic| DocTopic::parse_lossy(&topic))
                .unwrap_or(DocTopic::All)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_doc_request_tags() {
        let text = "Need docs.\n<doc_request topic=\"Interface\" />\n<doc_request_AB12 />";
        assert_eq!(
            parse_doc_request_tags(text),
            vec![DocTopic::Interface, DocTopic::All]
        );
    }

    #[test]
    fn test_parse_doc_request_unknown_topic_falls_back_to_all() {
        let text = "<doc_request topic=\"bogus\" />";
        assert_eq!(parse_doc_request_tags(text), vec![DocTopic::All]);
    }

    #[test]
    fn test_strip_doc_request_tags() {
        let text = "Intro\n<doc_request topic=\"git\" />\nOutro";
        assert_eq!(strip_doc_request_tags(text).trim(), "Intro\n\nOutro");
    }

    #[test]
    fn test_parse_doc_request_ignores_code_fences_and_inline_backticks() {
        let doc_text = "See `<doc_request topic=\"git\" />`.\n<doc_request topic=\"interface\" />";
        let topics = parse_doc_request_tags(doc_text);
        assert_eq!(topics, vec![DocTopic::Interface]);
    }
}
