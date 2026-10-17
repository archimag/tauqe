//! Parser for the `<scratchpad>` control tag.

use super::{next_tag_bounds, strip_tags};
use crate::edits::protocol::xml::engine;

/// Locates the next complete `<scratchpad ...>...</scratchpad>` or `<scratchpad .../>` tag.
pub(crate) fn next_scratchpad_tag(text: &str) -> Option<(usize, usize)> {
    next_tag_bounds(text, "<scratchpad")
}

/// Extracts ephemeral scratchpad content from model output if present.
pub fn extract_scratchpad(text: &str) -> Option<String> {
    let spec = engine::tag_spec("scratchpad")?;
    let notes: Vec<String> = engine::iter_tags(text, spec)
        .filter_map(|tag| tag.attr_or_body(&["note"]))
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty())
        .collect();
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n\n"))
    }
}

/// Removes all scratchpad tags and enclosed content from text.
pub fn strip_scratchpad_tags(text: &str) -> String {
    strip_tags(text, next_scratchpad_tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_and_strip_scratchpad() {
        let text = r#"
Introductory text
<scratchpad>
Working hypothesis: missing imports in protocol.rs.
Need to inspect protocol/xml/tags.rs.
</scratchpad>
Conversational explanation.
<scratchpad note="Secondary note" />
Final remarks.
"#;
        let extracted = extract_scratchpad(text);
        assert_eq!(
            extracted.as_deref(),
            Some("Working hypothesis: missing imports in protocol.rs.\nNeed to inspect protocol/xml/tags.rs.\n\nSecondary note")
        );

        let stripped = strip_scratchpad_tags(text);
        assert!(!stripped.contains("<scratchpad"));
        assert!(!stripped.contains("Working hypothesis"));
        assert!(stripped.contains("Introductory text"));
        assert!(stripped.contains("Final remarks."));
    }

    #[test]
    fn test_extract_scratchpad_ignores_markdown_code() {
        let text = r#"
Example:
```xml
<scratchpad>
Ignore this code fence note
</scratchpad>
```
Inline `<scratchpad>Ignore inline</scratchpad>`
Real:
<scratchpad>
Real working note
</scratchpad>
"#;
        let extracted = extract_scratchpad(text);
        assert_eq!(extracted.as_deref(), Some("Real working note"));

        let stripped = strip_scratchpad_tags(text);
        assert!(stripped.contains("Ignore this code fence note"));
        assert!(stripped.contains("`<scratchpad>Ignore inline</scratchpad>`"));
        assert!(!stripped.contains("Real working note"));
    }
}
