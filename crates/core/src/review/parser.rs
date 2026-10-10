use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity};

/// Parses raw markdown review output into structured `ReviewItem` instances.
/// Uses a robust line-oriented state machine that handles slight formatting variances.
pub fn parse_review_findings(raw_markdown: &str) -> Vec<ReviewItem> {
    let mut items = Vec::new();
    let mut current_id = 1u32;

    let mut current_title: Option<String> = None;
    let mut current_severity = ReviewSeverity::Warning;
    let mut current_file_path: Option<String> = None;
    let mut current_line_range: Option<(usize, usize)> = None;
    let mut current_body_lines: Vec<String> = Vec::new();

    let flush_item = |items: &mut Vec<ReviewItem>,
                      id: &mut u32,
                      title: &mut Option<String>,
                      severity: ReviewSeverity,
                      file_path: &mut Option<String>,
                      line_range: &mut Option<(usize, usize)>,
                      body_lines: &mut Vec<String>| {
        if let Some(t) = title.take() {
            let body = body_lines.join("\n").trim().to_string();
            items.push(ReviewItem {
                id: *id,
                title: t,
                severity,
                status: ReviewItemStatus::Discussion,
                model: None,
                file_path: file_path.take(),
                line_range: line_range.take(),
                body,
            });
            *id += 1;
            body_lines.clear();
        }
    };

    for line in raw_markdown.lines() {
        let trimmed = line.trim();

        // Check for finding section headers:
        // Examples: "## [CRITICAL] Race condition", "* [WARN] Inefficient clone", "### [SUGGESTION] Refactor"
        if let Some((severity, title)) = parse_header_line(trimmed) {
            flush_item(
                &mut items,
                &mut current_id,
                &mut current_title,
                current_severity,
                &mut current_file_path,
                &mut current_line_range,
                &mut current_body_lines,
            );
            current_title = Some(title);
            current_severity = severity;
            continue;
        }

        // If inside an item, check for file metadata lines:
        // Examples: "- **File**: src/main.rs:10-20", "- File: src/main.rs:15", ":FILE: src/main.rs"
        if current_title.is_some() && current_file_path.is_none() {
            if let Some((path, range)) = parse_file_line(trimmed) {
                current_file_path = Some(path);
                current_line_range = range;
                continue;
            }
        }

        if current_title.is_some() {
            // Strip leading "- **Details**:" prefix if present
            let content_line = if let Some(rest) = trimmed.strip_prefix("- **Details**:") {
                rest.trim()
            } else if let Some(rest) = trimmed.strip_prefix("**Details**:") {
                rest.trim()
            } else {
                line
            };
            if !content_line.is_empty() || !current_body_lines.is_empty() {
                current_body_lines.push(content_line.to_string());
            }
        }
    }

    // Flush last pending item
    flush_item(
        &mut items,
        &mut current_id,
        &mut current_title,
        current_severity,
        &mut current_file_path,
        &mut current_line_range,
        &mut current_body_lines,
    );

    // If no structured items were matched, treat the whole non-empty text as a single general item
    if items.is_empty() {
        let trimmed = raw_markdown.trim();
        if !trimmed.is_empty() {
            items.push(ReviewItem {
                id: 1,
                title: "General Review Findings".to_string(),
                model: None,
                severity: ReviewSeverity::Info,
                status: ReviewItemStatus::Discussion,
                file_path: None,
                line_range: None,
                body: trimmed.to_string(),
            });
        }
    }

    items
}

fn parse_header_line(trimmed: &str) -> Option<(ReviewSeverity, String)> {
    if let Some(rest) = trimmed.strip_prefix("### ") {
        parse_bracketed_severity(rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix("## ") {
        parse_bracketed_severity(rest.trim())
    } else {
        let rest = trimmed.strip_prefix("* [")?;
        parse_bracketed_severity(&format!("[{}", rest))
    }
}

fn parse_bracketed_severity(text: &str) -> Option<(ReviewSeverity, String)> {
    if !text.starts_with('[') {
        return None;
    }

    let closing_idx = text.find(']')?;
    let tag = text[1..closing_idx].trim().to_uppercase();
    let rest_title = text[closing_idx + 1..].trim();

    let title = rest_title
        .strip_prefix(':')
        .or_else(|| rest_title.strip_prefix('-'))
        .unwrap_or(rest_title)
        .trim()
        .to_string();

    let severity = match tag.as_str() {
        "CRITICAL" | "ERROR" | "FATAL" => ReviewSeverity::Critical,
        "WARN" | "WARNING" => ReviewSeverity::Warning,
        "SUGGESTION" | "SUGGEST" | "IMPROVEMENT" => ReviewSeverity::Suggestion,
        "INFO" | "NOTE" => ReviewSeverity::Info,
        _ => ReviewSeverity::Warning,
    };

    if title.is_empty() {
        Some((severity, format!("{:?} Finding", severity)))
    } else {
        Some((severity, title))
    }
}

fn parse_file_line(trimmed: &str) -> Option<(String, Option<(usize, usize)>)> {
    let raw = trimmed
        .strip_prefix("- **File**:")
        .or_else(|| trimmed.strip_prefix("- **file**:"))
        .or_else(|| trimmed.strip_prefix("- File:"))
        .or_else(|| trimmed.strip_prefix("- file:"))
        .or_else(|| trimmed.strip_prefix(":FILE:"))
        .or_else(|| trimmed.strip_prefix("File:"))
        .or_else(|| trimmed.strip_prefix("file:"))?;

    let spec = raw.trim().trim_matches('`').trim();
    if spec.is_empty() {
        return None;
    }

    // Parse path:line or path:start-end
    if let Some((path, lines)) = spec.rsplit_once(':') {
        let trimmed_path = path.trim().to_string();
        let trimmed_lines = lines.trim();
        if let Some((start_s, end_s)) = trimmed_lines.split_once('-') {
            if let (Ok(start), Ok(end)) = (start_s.trim().parse::<usize>(), end_s.trim().parse::<usize>()) {
                return Some((trimmed_path, Some((start, end))));
            }
        } else if let Ok(line) = trimmed_lines.parse::<usize>() {
            return Some((trimmed_path, Some((line, line))));
        }
        Some((trimmed_path, None))
    } else {
        Some((spec.to_string(), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_structured_review() {
        let md = r#"
## [CRITICAL] Race condition in state mutex
- **File**: crates/server/src/state.rs:42-50
- **Details**:
Multiple background tasks acquire read locks concurrently while writing.

## [WARNING] Inefficient string cloning
- **File**: crates/tui/src/ui/wrap.rs:115
Cloning strings inside loop creates memory churn.

### [SUGGESTION] Use SmallVec for items
- **File**: crates/core/src/edits.rs
Can reduce heap allocations for small hunk counts.
"#;

        let items = parse_review_findings(md);
        assert_eq!(items.len(), 3);

        assert_eq!(items[0].severity, ReviewSeverity::Critical);
        assert_eq!(items[0].title, "Race condition in state mutex");
        assert_eq!(items[0].file_path.as_deref(), Some("crates/server/src/state.rs"));
        assert_eq!(items[0].line_range, Some((42, 50)));
        assert!(items[0].body.contains("Multiple background tasks"));

        assert_eq!(items[1].severity, ReviewSeverity::Warning);
        assert_eq!(items[1].title, "Inefficient string cloning");
        assert_eq!(items[1].file_path.as_deref(), Some("crates/tui/src/ui/wrap.rs"));
        assert_eq!(items[1].line_range, Some((115, 115)));

        assert_eq!(items[2].severity, ReviewSeverity::Suggestion);
        assert_eq!(items[2].title, "Use SmallVec for items");
        assert_eq!(items[2].file_path.as_deref(), Some("crates/core/src/edits.rs"));
        assert_eq!(items[2].line_range, None);
    }

    #[test]
    fn test_fallback_unstructured_review() {
        let md = "Overall looks good, but pay attention to error handling in main.rs.";
        let items = parse_review_findings(md);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "General Review Findings");
        assert_eq!(items[0].severity, ReviewSeverity::Info);
        assert_eq!(items[0].body, md);
    }
}
