//! Parser for the `<dir_list>` control tag.
//!
//! The tag requests the tracked-file listing of one repository directory. It
//! carries an optional `path` and an optional `recursive` flag, and every path
//! resolves through the shared repository path rules, so a listing request can
//! never address a location outside the repository.

use tauqe_protocol::DirListRequest;

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Specification of the `<dir_list>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("dir_list")
}

/// Parses all `<dir_list>` tags into typed directory listing requests.
///
/// `path` (or its `dir` alias) defaults to the repository root. `recursive`
/// widens the listing when it reads as a wire boolean (`true`, `1`, or `yes`);
/// any other value keeps the request shallow, so a malformed flag narrows the
/// listing instead of failing it. Every path is normalized through
/// [`DirListRequest::normalized`]: `.` and `./` collapse to the repository
/// root, and a path that escapes the repository is dropped instead of resolved.
pub(crate) fn parse_dir_list_tags(text: &str) -> Vec<DirListRequest> {
    let Some(spec) = tag_spec() else {
        return Vec::new();
    };
    engine::iter_tags(text, spec)
        .filter_map(|found| {
            let path = found.attr_any(&["path", "dir"]).unwrap_or_default();
            let recursive = found.attr("recursive").as_deref().is_some_and(is_truthy);
            DirListRequest::new(path, recursive).normalized()
        })
        .collect()
}

/// Reports whether an attribute value asks for a recursive listing.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dir_list_tags_defaults_and_flags() {
        let text = "Listing:\n\
<dir_list path=\"crates/core\" />\n\
<dir_list path=\"crates/protocol\" recursive=\"true\" />\n\
<dir_list dir=\"crates/server\" recursive=\"false\" />";
        let requests = parse_dir_list_tags(text);
        assert_eq!(
            requests,
            vec![
                DirListRequest::new("crates/core", false),
                DirListRequest::new("crates/protocol", true),
                DirListRequest::new("crates/server", false),
            ]
        );
    }

    #[test]
    fn test_parse_dir_list_tags_normalizes_paths_and_drops_escapes() {
        let text = "<dir_list />\n\
<dir_list path=\"./crates/core/\" recursive=\"yes\" />\n\
<dir_list path=\"../escape\" recursive=\"true\" />";
        let requests = parse_dir_list_tags(text);
        assert_eq!(
            requests,
            vec![
                DirListRequest::new("", false),
                DirListRequest::new("crates/core", true),
            ]
        );
    }

    #[test]
    fn test_parse_dir_list_tags_accepts_marker_suffix() {
        let requests =
            parse_dir_list_tags("<dir_list_AB12 path=\"crates/core\" recursive=\"1\" />");
        assert_eq!(requests, vec![DirListRequest::new("crates/core", true)]);
    }

    #[test]
    fn test_parse_dir_list_ignores_code_fences_and_inline_backticks() {
        let text = "Example:\n\
```xml\n\
<dir_list path=\"fenced\" recursive=\"true\" />\n\
```\n\
Inline `<dir_list path=\"inline\" />`.";
        assert!(parse_dir_list_tags(text).is_empty());
    }
}
