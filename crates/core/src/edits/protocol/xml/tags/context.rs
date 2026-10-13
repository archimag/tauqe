//! Parsers for the `<context_request>` control tags.

use tauqe_protocol::ContextAccess;

use super::{next_simple_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;
use crate::edits::protocol::ContextRequest;

/// Locates the next complete `<context_request .../>` tag (unsuffixed dialect).
pub(crate) fn next_context_request_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<context_request")
}

/// Removes all context request tags from text.
pub fn strip_context_request_tags(text: &str) -> String {
    strip_tags(text, next_context_request_tag)
}

/// Parses all `<context_request>` tags into typed access requests.
pub(crate) fn parse_context_request_tags(text: &str) -> Vec<ContextRequest> {
    let mut requests = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_context_request_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        if let Some(path) = extract_attribute(header, "path") {
            let path = path
                .trim()
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            if !path.is_empty() {
                let editable = extract_attribute(header, "access")
                    .is_some_and(|a| a.trim().eq_ignore_ascii_case("editable"));
                requests.push(ContextRequest {
                    path,
                    access: if editable {
                        ContextAccess::Editable
                    } else {
                        ContextAccess::ReadOnly
                    },
                });
            }
        }
        pos += end;
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_context_request_ignores_code_fences_and_inline_backticks() {
        let text = "Here is an example:\n```xml\n<context_request path=\"fence.rs\" />\n```\nAnd inline `<context_request path=\"inline.rs\" />`.\nReal:\n<context_request path=\"real.rs\" />";
        let reqs = parse_context_request_tags(text);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "real.rs");
    }
}
