//! Parsers for the `<context_request>` control tags.

use tauqe_protocol::ContextAccess;

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};
use crate::edits::protocol::ContextRequest;

/// Specification of the `<context_request>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("context_request")
}

/// Removes all context request tags from text.
pub fn strip_context_request_tags(text: &str) -> String {
    match tag_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

/// Parses all `<context_request>` tags into typed access requests.
pub(crate) fn parse_context_request_tags(text: &str) -> Vec<ContextRequest> {
    let Some(spec) = tag_spec() else {
        return Vec::new();
    };
    let mut requests = Vec::new();
    for found in engine::iter_tags(text, spec) {
        let Some(raw_path) = found.attr("path") else {
            continue;
        };
        let path = raw_path
            .trim()
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        if path.is_empty() {
            continue;
        }
        let editable = found
            .attr("access")
            .is_some_and(|access| access.trim().eq_ignore_ascii_case("editable"));
        requests.push(ContextRequest {
            path,
            access: if editable {
                ContextAccess::Editable
            } else {
                ContextAccess::ReadOnly
            },
        });
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
