//! Parser for the `<user_language>` control tag.

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};

/// Specification of the `<user_language>` control tag from the XML tag registry.
pub(crate) fn tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("user_language")
}

/// Extracts user language tag content if present (e.g. `<user_language>Russian</user_language>`
/// or `<user_language lang="ru"/>`).
pub fn extract_user_language(text: &str) -> Option<String> {
    let spec = tag_spec()?;
    let found = engine::find_next_tag(text, spec)?;
    if let Some(value) = found.attr_any(&["value", "lang"]) {
        let trimmed = value.trim();
        if !trimmed.is_empty() && trimmed.len() < 50 && !trimmed.contains('\n') {
            return Some(trimmed.to_string());
        }
    }
    let content = found.body?.trim();
    if !content.is_empty() && content.len() < 50 && !content.contains('\n') {
        return Some(content.to_string());
    }
    None
}

/// Removes all user_language tags from text.
pub fn strip_user_language_tags(text: &str) -> String {
    match tag_spec() {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}
