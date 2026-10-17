//! Parsers for the `<repo_search>` control tags.

use tauqe_protocol::RepoSearchRequest;

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Specification of the `<repo_search>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("repo_search")
}

/// Removes all repo search tags from text.
pub fn strip_repo_search_tags(text: &str) -> String {
    match tag_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

fn normalized_path(raw: Option<String>) -> String {
    raw.unwrap_or_default()
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Parses all `<repo_search>` tags into typed search requests.
///
/// Supports aliases for attributes:
/// - `path`: `path`, `dir` (defaults to empty string for repository root)
/// - `regex`: `regex`, `pattern`, `query`
pub(crate) fn parse_repo_search_tags(text: &str) -> Vec<RepoSearchRequest> {
    let Some(spec) = tag_spec() else {
        return Vec::new();
    };
    let mut requests = Vec::new();
    for found in engine::iter_tags(text, spec) {
        let path = normalized_path(found.attr_any(&["path", "dir"]));
        let regex = found
            .attr_any(&["regex", "pattern", "query"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if !regex.is_empty() {
            requests.push(RepoSearchRequest { path, regex });
        }
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_repo_search_tags_basic_and_aliases() {
        let text = r#"
Searching:
<repo_search path="crates/core" regex="fn execute" />
<repo_search dir="crates/protocol" pattern="struct RepoSearchRequest" />
<repo_search query="trait PromptProtocol" />
Done.
"#;
        let reqs = parse_repo_search_tags(text);
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].path, "crates/core");
        assert_eq!(reqs[0].regex, "fn execute");
        assert_eq!(reqs[1].path, "crates/protocol");
        assert_eq!(reqs[1].regex, "struct RepoSearchRequest");
        assert_eq!(reqs[2].path, "");
        assert_eq!(reqs[2].regex, "trait PromptProtocol");
    }

    #[test]
    fn test_parse_repo_search_ignores_code_fences_and_inline_backticks() {
        let text = r#"
Example:
```xml
<repo_search path="fence.rs" regex="ignored" />
```
Inline `<repo_search path="inline.rs" regex="ignored" />`.
Real:
<repo_search path="crates/core" regex="pub fn find" />
"#;
        let reqs = parse_repo_search_tags(text);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "crates/core");
        assert_eq!(reqs[0].regex, "pub fn find");
    }

    #[test]
    fn test_strip_repo_search_tags() {
        let text = "Intro\n<repo_search path=\"src\" regex=\"foo\" />\nOutro";
        assert_eq!(strip_repo_search_tags(text).trim(), "Intro\n\nOutro");
    }
}
