//! Parsers for the `<repo_search>` control tags.

use tauqe_protocol::RepoSearchRequest;

use super::{next_simple_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

/// Locates the next complete `<repo_search .../>` tag.
pub(crate) fn next_repo_search_tag(text: &str) -> Option<(usize, usize)> {
    next_simple_tag(text, "<repo_search")
}

/// Removes all repo search tags from text.
pub fn strip_repo_search_tags(text: &str) -> String {
    strip_tags(text, next_repo_search_tag)
}

/// Parses all `<repo_search>` tags into typed search requests.
///
/// Supports aliases for attributes:
/// - `path`: `path`, `dir` (defaults to empty string for repository root)
/// - `regex`: `regex`, `pattern`, `query`
pub(crate) fn parse_repo_search_tags(text: &str) -> Vec<RepoSearchRequest> {
    let mut requests = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_repo_search_tag(&text[pos..]) {
        let tag = &text[pos + start..pos + end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];

        let path = extract_attribute(header, "path")
            .or_else(|| extract_attribute(header, "dir"))
            .unwrap_or_default()
            .trim()
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();

        let regex = extract_attribute(header, "regex")
            .or_else(|| extract_attribute(header, "pattern"))
            .or_else(|| extract_attribute(header, "query"))
            .unwrap_or_default()
            .trim()
            .to_string();

        if !regex.is_empty() {
            requests.push(RepoSearchRequest { path, regex });
        }
        pos += end;
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
