//! Parser for the `<doc_request>` control tag.

use super::{next_simple_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

/// Locates the next complete `<doc_request .../>` tag.
pub(crate) fn next_doc_request_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<doc_request")
}

/// Removes all documentation request tags from text.
pub fn strip_doc_request_tags(text: &str) -> String {
    strip_tags(text, next_doc_request_tag)
}

/// Returns the requested documentation topics (lowercase); a missing topic means `all`.
pub(crate) fn parse_doc_request_tags(text: &str) -> Vec<String> {
    let mut topics = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_doc_request_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let topic = extract_attribute(&tag[..header_end], "topic")
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "all".to_string());
        topics.push(topic);
        pos += end;
    }
    topics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_doc_request_tags() {
        let text = "Need docs.\n<doc_request topic=\"Interface\" />\n<doc_request_AB12 />";
        assert_eq!(
            parse_doc_request_tags(text),
            vec!["interface".to_string(), "all".to_string()]
        );
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
        assert_eq!(topics, vec!["interface".to_string()]);
    }
}
