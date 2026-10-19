//! Parser for the `<repo_dirs>` control tag.
//!
//! The tag requests the compact repository directory tree: folders only, no
//! files, rendered deterministically from the tracked file list. It carries no
//! scope argument because the outline always covers the whole repository.

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Specification of the `<repo_dirs>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("repo_dirs")
}

/// Removes all repository directory tree tags from text.
pub fn strip_repo_dirs_tags(text: &str) -> String {
    match tag_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

/// Reports whether the model requested the compact repository directory tree.
pub(crate) fn parse_repo_dirs_tags(text: &str) -> bool {
    match tag_spec() {
        Some(spec) => engine::find_next_tag(text, spec).is_some(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_repo_dirs_tag_self_closing_and_paired() {
        assert!(parse_repo_dirs_tags("Outline:\n<repo_dirs />\nDone."));
        assert!(parse_repo_dirs_tags("<repo_dirs></repo_dirs>"));
        assert!(parse_repo_dirs_tags("<repo_dirs_AB12 />"));
        assert!(!parse_repo_dirs_tags("no request here"));
        assert!(!parse_repo_dirs_tags("<repo_dirs"));
    }

    #[test]
    fn test_parse_repo_dirs_ignores_code_fences_and_inline_backticks() {
        let text = "Example:\n```xml\n<repo_dirs />\n```\nInline `<repo_dirs />`.";
        assert!(!parse_repo_dirs_tags(text));
    }

    #[test]
    fn test_strip_repo_dirs_tags() {
        let text = "Intro\n<repo_dirs />\nOutro";
        assert_eq!(strip_repo_dirs_tags(text).trim(), "Intro\n\nOutro");
    }
}
