//! Parser for the `<scratchpad>` control tag.

use super::{next_control_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

/// Locates the next complete `<scratchpad ...>...</scratchpad>` or `<scratchpad .../>` tag.
pub(crate) fn next_scratchpad_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<scratchpad")
}

/// Extracts ephemeral scratchpad content from model output if present.
pub fn extract_scratchpad(text: &str) -> Option<String> {
    let mut notes = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_scratchpad_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let tag_name = header
            .strip_prefix('<')
            .unwrap_or_default()
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect::<String>();
        let close_tag = format!("</{}>", tag_name);

        let mut note = None;
        if !is_self_closing && tag.ends_with(&close_tag) {
            let body = tag[header_end + 1..tag.len() - close_tag.len()].trim();
            if !body.is_empty() {
                note = Some(body.to_string());
            }
        }
        if note.is_none() {
            if let Some(attr_note) = extract_attribute(header, "note") {
                let trimmed = attr_note.trim();
                if !trimmed.is_empty() {
                    note = Some(trimmed.to_string());
                }
            }
        }
        if let Some(n) = note {
            notes.push(n);
        }
        pos = abs_end;
    }
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
