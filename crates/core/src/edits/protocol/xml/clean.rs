//! Conversational-text extraction for the XML edit protocol.
//!
//! [`extract_conversational_text`] removes every edit block and control tag
//! from model output so history and the UI display only the model's prose. The
//! edit block names it strips are resolved from the capability registry, so the
//! cleaner never re-declares a tag spelling of its own.

use super::engine::strip_all_control_tags;
use super::parser::{file_edit_tags, find_base_tag_start};

/// Extracts conversational text from model output by stripping edit tags
/// (the edit container, its file blocks, and every control tag) and cleaning up
/// extra whitespace.
pub fn extract_conversational_text(raw_text: &str) -> String {
    let tags = file_edit_tags();
    let mut text = raw_text.to_string();

    // 1. Remove code fences labelled with an edit tag name.
    for name in tags.opening_names() {
        strip_fenced_edit_blocks(&mut text, name);
    }

    // 2. Remove every edit container, then every standalone file block.
    for container in &tags.containers {
        strip_edit_block(&mut text, container, false);
    }
    for block in &tags.file_blocks {
        strip_edit_block(&mut text, block.name, block.requires_target);
    }

    clean_conversational_lines(&strip_all_control_tags(&text))
}

/// Removes code fences labelled with an edit tag name.
fn strip_fenced_edit_blocks(text: &mut String, base: &str) {
    let opening = format!("```{}", base);
    while let Some(start) = text.find(&opening) {
        let after_start = start + opening.len();
        if let Some(end_idx) = text[after_start..].find("```") {
            text.replace_range(start..after_start + end_idx + 3, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }
}

/// Removes every occurrence of the named edit block from `text`.
///
/// A block is removed together with its closing tag, or up to its opening
/// header when it is self-closing or unterminated; an enclosing code fence is
/// removed as well. When `requires_target` is set, only a block whose header
/// names a target path is removed, so a spelling shared with a hunk block
/// leaves prose mentions untouched.
fn strip_edit_block(text: &mut String, base: &str, requires_target: bool) {
    while let Some((start, tag_name)) = find_base_tag_start(text, base) {
        let Some(gt_offset) = text[start..].find('>') else {
            text.replace_range(start.., "");
            break;
        };
        let tag_open_end = start + gt_offset + 1;
        if requires_target && !text[start..tag_open_end].contains("path") {
            break;
        }

        let self_closing = text[start..tag_open_end].trim_end().ends_with("/>");
        let close_tag = format!("</{}>", tag_name);
        let end = if self_closing {
            None
        } else {
            text[start..]
                .find(&close_tag)
                .map(|offset| start + offset + close_tag.len())
        };

        let (exp_start, exp_end) = match end {
            Some(end) => expand_fence_bounds(text, start, end),
            None if self_closing => expand_fence_bounds(text, start, tag_open_end),
            None => (start, tag_open_end),
        };
        text.replace_range(exp_start..exp_end, "");
    }
}

fn expand_fence_bounds(text: &str, start: usize, end: usize) -> (usize, usize) {
    let before = &text[..start];
    let after = &text[end..];

    let mut exp_start = start;
    let mut exp_end = end;

    let trimmed_before = before.trim_end();
    if let Some(last_newline) = trimmed_before.rfind('\n') {
        let line_before = &trimmed_before[last_newline + 1..];
        if line_before.trim().starts_with("```") {
            exp_start = last_newline + 1;
        }
    } else if trimmed_before.trim().starts_with("```") {
        exp_start = 0;
    }

    let after_lines: Vec<&str> = after.lines().collect();
    let mut has_closing_fence = false;
    for line in &after_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "```" {
            has_closing_fence = true;
        }
        break;
    }

    if has_closing_fence {
        if let Some(fence_pos) = after.find("```") {
            let offset = fence_pos + 3;
            let skip_nl = if after[offset..].starts_with("\r\n") {
                2
            } else if after[offset..].starts_with('\n') {
                1
            } else {
                0
            };
            exp_end = end + offset + skip_nl;
        }
    }

    (exp_start, exp_end)
}

fn clean_conversational_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("```") && trimmed.len() > 3 && !trimmed.ends_with("```") {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "```" {
                i = j + 1;
                continue;
            }
        }

        if trimmed == "```" {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "```" {
                i = j + 1;
                continue;
            }
        }

        result_lines.push(line.trim_end());
        i += 1;
    }

    let mut final_lines: Vec<&str> = Vec::new();
    let mut last_was_empty = true;

    for line in result_lines {
        if line.trim().is_empty() {
            if !last_was_empty {
                final_lines.push("");
                last_was_empty = true;
            }
        } else {
            final_lines.push(line);
            last_was_empty = false;
        }
    }

    while let Some(last) = final_lines.last() {
        if last.is_empty() {
            final_lines.pop();
        } else {
            break;
        }
    }

    final_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_conversational_text_removes_xml_blocks() {
        let output = r#"I have updated the code to fix the issue.

<tauqe_edits summary="Fix issue">
  <edit path="crates/core/src/lib.rs">
    <search>
old_code();
    </search>
    <replace>
new_code();
    </replace>
  </edit>
</tauqe_edits>

Everything is tested and working properly."#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "I have updated the code to fix the issue.\n\nEverything is tested and working properly."
        );
    }

    #[test]
    fn test_extract_conversational_text_individual_tags_and_fences() {
        let output = r#"Here is the change:

```xml
<create path="crates/core/src/helper.rs">
pub fn help() {}
</create>
```

And deleting obsolete:
<delete path="crates/core/src/old.rs" />

All done!"#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "Here is the change:\n\nAnd deleting obsolete:\n\nAll done!"
        );
    }

    #[test]
    fn test_extract_conversational_text_only_edits() {
        let output = r#"<tauqe_edits summary="Pure edit">
  <delete path="test.rs" />
</tauqe_edits>"#;
        let extracted = extract_conversational_text(output);
        assert_eq!(extracted, "");
    }

    #[test]
    fn test_extract_conversational_text_removes_suffixed_xml_blocks_and_context_requests() {
        let output = r#"I have updated the code to fix the issue.

<context_request_TESTMARKER123 path="crates/tui/src/main.rs" access="editable" />

<tauqe_edits_TESTMARKER123 summary="Fix issue">
  <edit_TESTMARKER123 path="crates/core/src/lib.rs">
    <search_TESTMARKER123>
old_code();
    </search_TESTMARKER123>
    <replace_TESTMARKER123>
new_code();
    </replace_TESTMARKER123>
  </edit_TESTMARKER123>
</tauqe_edits_TESTMARKER123>

Everything is tested and working properly."#;

        let extracted = extract_conversational_text(output);
        assert_eq!(
            extracted,
            "I have updated the code to fix the issue.\n\nEverything is tested and working properly."
        );
    }

    #[test]
    fn test_extract_conversational_text_removes_plan_tags() {
        let output = "Here is the plan:\n<plan id=\"my-plan\" title=\"Title\">\n<item id=\"1\" title=\"A\" />\n</plan>\nLet's do this!";
        assert_eq!(
            extract_conversational_text(output),
            "Here is the plan:\n\nLet's do this!"
        );
    }
}
