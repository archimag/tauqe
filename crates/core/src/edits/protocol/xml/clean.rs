//! Conversational-text extraction for the XML edit protocol.
//!
//! [`extract_conversational_text`] removes every edit block and control tag
//! from model output so history and the UI display only the model's prose.

use super::engine::strip_all_control_tags;
use super::parser::find_base_tag_start;

/// Extracts conversational text from model output by stripping edit tags
/// (<tauqe_edits>, <edit>, <create>, <delete>, etc.) and cleaning up extra whitespace.
pub fn extract_conversational_text(raw_text: &str) -> String {
    let mut text = raw_text.to_string();

    // 1. Remove ```tauqe_edit ... ``` JSON fenced blocks if present
    while let Some(start_idx) = text.find("```tauqe_edit") {
        let after_start = start_idx + "```tauqe_edit".len();
        if let Some(end_idx) = text[after_start..].find("```") {
            let total_end = after_start + end_idx + 3;
            text.replace_range(start_idx..total_end, "");
        } else {
            text.replace_range(start_idx.., "");
            break;
        }
    }

    // 2. Remove <tauqe_edits>...</tauqe_edits> blocks (and enclosing fences if any)
    while let Some((start, tag_name)) = find_base_tag_start(&text, "tauqe_edits") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let header = &text[start..start + gt_offset + 1];
            if header.trim_end().ends_with("/>") {
                let end = start + gt_offset + 1;
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let end = start + gt_offset + 1;
                text.replace_range(start..end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 3. Remove standalone <edit ...>...</edit>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "edit") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let end = start + gt_offset + 1;
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 4. Remove standalone <create ...>...</create>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "create") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let end = start + gt_offset + 1;
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 4b. Remove standalone <overwrite ...>...</overwrite>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "overwrite") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(close_offset) = text[start..].find(&close_tag) {
            let end = start + close_offset + close_tag.len();
            let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
            text.replace_range(exp_start..exp_end, "");
        } else if let Some(gt_offset) = text[start..].find('>') {
            let end = start + gt_offset + 1;
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 5. Remove standalone <delete .../> or <delete ...>...</delete>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "delete") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(gt_offset) = text[start..].find('>') {
            let tag_open_end = start + gt_offset + 1;
            let header = &text[start..tag_open_end];
            if header.trim_end().ends_with("/>") {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            } else if let Some(close_offset) = text[start..].find(&close_tag) {
                let end = start + close_offset + close_tag.len();
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 5b. Remove standalone <move .../> or <move ...>...</move>
    while let Some((start, tag_name)) = find_base_tag_start(&text, "move") {
        let close_tag = format!("</{}>", tag_name);
        if let Some(gt_offset) = text[start..].find('>') {
            let tag_open_end = start + gt_offset + 1;
            let header = &text[start..tag_open_end];
            if header.trim_end().ends_with("/>") {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            } else if let Some(close_offset) = text[start..].find(&close_tag) {
                let end = start + close_offset + close_tag.len();
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                text.replace_range(exp_start..exp_end, "");
            } else {
                let (exp_start, exp_end) = expand_fence_bounds(&text, start, tag_open_end);
                text.replace_range(exp_start..exp_end, "");
            }
        } else {
            text.replace_range(start.., "");
            break;
        }
    }

    // 6. Remove standalone <replace path="...">...</replace> if any
    while let Some((start, tag_name)) = find_base_tag_start(&text, "replace") {
        if let Some(gt_offset) = text[start..].find('>') {
            let header = &text[start..start + gt_offset + 1];
            if header.contains("path") {
                let close_tag = format!("</{}>", tag_name);
                if let Some(close_offset) = text[start..].find(&close_tag) {
                    let end = start + close_offset + close_tag.len();
                    let (exp_start, exp_end) = expand_fence_bounds(&text, start, end);
                    text.replace_range(exp_start..exp_end, "");
                    continue;
                }
            }
        }
        break;
    }

    clean_conversational_lines(&strip_all_control_tags(&text))
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
