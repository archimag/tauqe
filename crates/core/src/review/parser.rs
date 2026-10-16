use serde::Deserialize;
use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity};

use crate::providers::{JsonSchemaDefinition, ResponseFormat};

/// Strict JSON Schema for the review synthesis output.
///
/// Review synthesis never produces code changes, so it is a Structured Output
/// operation (see `crates/core/Conventions.md`, section 2.2). Findings, severity
/// classifications, and file locations are exchanged as machine-readable JSON
/// instead of free-form markdown recovered by line heuristics.
pub fn review_findings_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "High-level Markdown conclusion covering architectural integrity, concurrency and safety risks, error handling, and overall reliability, in the primary natural language of the user's instructions"
            },
            "findings": {
                "type": "array",
                "description": "Structured code review findings",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Concise finding title in the primary natural language of the user's instructions"
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["critical", "warning", "suggestion", "info"],
                            "description": "Finding severity: 'critical' (bug, panic, leak, or security flaw), 'warning' (architecture or runtime risk), 'suggestion' (cleanup or optimization), 'info' (general insight)"
                        },
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "string"
                            },
                            "description": "Repository-relative paths of the affected files (empty array when the finding is not file-specific)"
                        },
                        "body": {
                            "type": "string",
                            "description": "Clear explanation of the problem, why it occurs, and concrete remediation advice"
                        }
                    },
                    "required": ["title", "severity", "files", "body"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "findings"],
        "additionalProperties": false
    })
}

/// Builds the provider [`ResponseFormat`] that enforces the review JSON schema.
pub fn review_findings_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "review_findings".to_string(),
            description: Some(
                "Structured code review findings with severity classification and file locations"
                    .to_string(),
            ),
            schema: review_findings_response_schema(),
            strict: Some(true),
        },
    }
}

/// Model-emitted review synthesis payload.
///
/// The `summary` field carries the reviewer's high-level Markdown narrative and
/// is deserialized alongside the structured `findings` array.
#[derive(Debug, Deserialize)]
pub struct ReviewFindingsOutput {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFindingOutput>,
}

/// A single model-emitted finding before server-side normalization.
#[derive(Debug, Deserialize)]
pub struct ReviewFindingOutput {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub body: String,
}

/// Parses the model's review synthesis output into a high-level summary and
/// structured findings.
///
/// The payload is decoded by deserializing strict JSON. Surrounding markdown
/// code fences or wrapper prose are tolerated, but the payload itself must form
/// a valid JSON document. The returned summary is `None` when the model produced
/// no textual conclusion. When no structured payload can be recovered, the raw
/// text is preserved as a single informational finding so review output is never
/// silently discarded.
pub fn parse_review_output(raw: &str) -> (Option<String>, Vec<ReviewItem>) {
    match parse_findings_output(raw) {
        Some(output) => {
            let ReviewFindingsOutput { summary, findings } = output;
            let items = findings
                .into_iter()
                .enumerate()
                .map(|(index, finding)| {
                    let mut files: Vec<String> = finding
                        .files
                        .into_iter()
                        .map(|f| f.trim().to_string())
                        .filter(|f| !f.is_empty())
                        .collect();
                    if files.is_empty() {
                        if let Some(fp) = finding.file_path {
                            let trimmed = fp.trim();
                            if !trimmed.is_empty() {
                                files.push(trimmed.to_string());
                            }
                        }
                    }
                    ReviewItem {
                        id: index as u32 + 1,
                        title: normalize_title(finding.title, finding.severity),
                        severity: finding.severity,
                        status: ReviewItemStatus::Discussion,
                        model: None,
                        files,
                        subplan: Vec::new(),
                        resolution_notes: None,
                        body: finding.body.trim().to_string(),
                    }
                })
                .collect();
            (normalize_summary(summary), items)
        }
        None => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                (None, Vec::new())
            } else {
                (
                    None,
                    vec![ReviewItem {
                        id: 1,
                        title: "General Review Findings".to_string(),
                        model: None,
                        severity: ReviewSeverity::Info,
                        status: ReviewItemStatus::Discussion,
                        files: Vec::new(),
                        subplan: Vec::new(),
                        resolution_notes: None,
                        body: trimmed.to_string(),
                    }],
                )
            }
        }
    }
}

/// Parses the model's review synthesis output into structured `ReviewItem` instances.
///
/// Compatibility wrapper around [`parse_review_output`] that discards the
/// high-level summary.
pub fn parse_review_findings(raw: &str) -> Vec<ReviewItem> {
    parse_review_output(raw).1
}

fn normalize_title(title: String, severity: ReviewSeverity) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        format!("{} Finding", severity)
    } else {
        trimmed.to_string()
    }
}

/// Trims a model-emitted summary, yielding `None` when it is empty.
fn normalize_summary(summary: String) -> Option<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}


/// Deserializes a review payload, tolerating code fences and wrapper prose.
fn parse_findings_output(raw: &str) -> Option<ReviewFindingsOutput> {
    let cleaned = strip_code_fence(raw);
    if let Ok(output) = serde_json::from_str::<ReviewFindingsOutput>(cleaned) {
        return Some(output);
    }

    let mut from = 0;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        if let Some(end) = balanced_object_end(raw, start) {
            if let Ok(output) = serde_json::from_str::<ReviewFindingsOutput>(&raw[start..end]) {
                return Some(output);
            }
        }
        from = start + 1;
    }
    None
}

/// Returns the trimmed contents of the first markdown code fence, if present.
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(idx) = trimmed.find("```json") {
        let after = &trimmed[idx + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    } else if let Some(idx) = trimmed.find("```") {
        let after = &trimmed[idx + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    trimmed
}

/// Returns the exclusive end offset of the JSON object starting at `start`, if complete.
fn balanced_object_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_structured_json_review() {
        let raw = r#"{
            "findings": [
                {
                    "title": "Race condition in state mutex",
                    "severity": "critical",
                    "files": ["crates/server/src/state.rs", "crates/server/src/handlers/review.rs"],
                    "body": "Multiple background tasks acquire read locks concurrently while writing."
                },
                {
                    "title": "Inefficient string cloning",
                    "severity": "warning",
                    "files": ["crates/tui/src/ui/wrap.rs"],
                    "body": "Cloning strings inside loop creates memory churn."
                },
                {
                    "title": "Use SmallVec for items",
                    "severity": "suggestion",
                    "files": ["crates/core/src/edits.rs"],
                    "body": "Can reduce heap allocations for small hunk counts."
                }
            ]
        }"#;

        let items = parse_review_findings(raw);
        assert_eq!(items.len(), 3);

        assert_eq!(items[0].id, 1);
        assert_eq!(items[0].severity, ReviewSeverity::Critical);
        assert_eq!(items[0].title, "Race condition in state mutex");
        assert_eq!(
            items[0].files,
            vec![
                "crates/server/src/state.rs",
                "crates/server/src/handlers/review.rs"
            ]
        );
        assert_eq!(items[0].status, ReviewItemStatus::Discussion);
        assert!(items[0].body.contains("Multiple background tasks"));

        assert_eq!(items[1].severity, ReviewSeverity::Warning);
        assert_eq!(items[1].title, "Inefficient string cloning");
        assert_eq!(items[1].files, vec!["crates/tui/src/ui/wrap.rs"]);

        assert_eq!(items[2].severity, ReviewSeverity::Suggestion);
        assert_eq!(items[2].files, vec!["crates/core/src/edits.rs"]);
    }

    #[test]
    fn test_parse_review_findings_tolerates_fences_and_wrapper_prose() {
        let raw = "Here is the review:\n```json\n{\"findings\":[{\"title\":\"Bug\",\"severity\":\"warning\",\"files\":[],\"body\":\"details\"}]}\n```\nHope this helps!";
        let items = parse_review_findings(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Bug");
        assert_eq!(items[0].severity, ReviewSeverity::Warning);
        assert!(items[0].files.is_empty());
        assert_eq!(items[0].body, "details");
    }

    #[test]
    fn test_parse_review_findings_empty_array_yields_no_items() {
        let items = parse_review_findings("{\"findings\": []}");
        assert!(items.is_empty());
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

    #[test]
    fn test_review_findings_schema_is_strict_and_non_recursive() {
        let schema = review_findings_response_schema();
        let text = schema.to_string();
        assert!(
            !text.contains("$ref"),
            "schema must not contain recursive $ref"
        );
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(schema["required"], serde_json::json!(["summary", "findings"]));
        let required = schema["properties"]["findings"]["items"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(required.len(), 4);
    }

    #[test]
    fn test_summary_is_first_schema_property() {
        let schema = review_findings_response_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("properties object must exist");
        let first_key = properties.keys().next().expect("first property must exist");
        assert_eq!(first_key, "summary", "summary MUST be the first property");

        let required = schema["required"].as_array().expect("required array");
        assert_eq!(required[0], serde_json::json!("summary"));
        assert_eq!(required[1], serde_json::json!("findings"));

        let schema_str = serde_json::to_string(&schema).unwrap();
        let summary_idx = schema_str.find("\"summary\"").expect("summary present");
        let findings_idx = schema_str.find("\"findings\"").expect("findings present");
        assert!(summary_idx < findings_idx);
    }

    #[test]
    fn test_parse_review_output_extracts_summary_and_findings() {
        let raw = r#"{
            "summary": "The architecture is sound, but locking is racy.",
            "findings": [
                {
                    "title": "Race condition",
                    "severity": "critical",
                    "files": ["crates/server/src/state.rs"],
                    "body": "Lock ordering is inconsistent."
                }
            ]
        }"#;

        let (summary, items) = parse_review_output(raw);
        assert_eq!(
            summary.as_deref(),
            Some("The architecture is sound, but locking is racy.")
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Race condition");
        assert_eq!(items[0].severity, ReviewSeverity::Critical);
        assert_eq!(items[0].files, vec!["crates/server/src/state.rs"]);
    }

    #[test]
    fn test_parse_review_output_blank_or_missing_summary_is_none() {
        let (blank, blank_items) = parse_review_output(r#"{"summary":"   ","findings":[]}"#);
        assert!(blank.is_none());
        assert!(blank_items.is_empty());

        let (missing, missing_items) = parse_review_output(r#"{"findings":[]}"#);
        assert!(missing.is_none());
        assert!(missing_items.is_empty());
    }

    #[test]
    fn test_parse_review_output_unstructured_fallback_has_no_summary() {
        let (summary, items) = parse_review_output("Just prose, no JSON payload.");
        assert!(summary.is_none());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, ReviewSeverity::Info);
    }

    #[test]
    fn test_parse_review_findings_wrapper_discards_summary() {
        let raw = r#"{
            "summary": "High-level conclusion.",
            "findings": [
                {
                    "title": "Cloning in loop",
                    "severity": "warning",
                    "file_path": null,
                    "line_start": null,
                    "line_end": null,
                    "body": "Avoid clones."
                }
            ]
        }"#;
        let items = parse_review_findings(raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Cloning in loop");
        assert_eq!(items[0].severity, ReviewSeverity::Warning);
    }
}
