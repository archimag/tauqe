use serde::Deserialize;
use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue};

use crate::edits::stream::json::decode::{balanced_object_end, strip_code_fence};
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

/// Strict JSON Schema for the review synthesis output.
///
/// Review synthesis never produces code changes, so it is a Structured Output
/// operation (see `crates/core/Conventions.md`, section 2.2). The reviewer role
/// emits a concise session title, a high-level summary, and machine-readable
/// findings. Each finding carries an actionable category, a technical
/// description of the defect, an explicit remediation strategy, the files to
/// modify, and the immutable files the fix must rely on but never change.
pub fn review_findings_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "High-level Markdown conclusion covering architectural integrity, concurrency and safety risks, error handling, and overall reliability, in the primary natural language of the user's instructions"
            },
            "title": {
                "type": "string",
                "description": "Concise, human-readable title of the review session in the primary natural language of the user's instructions"
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
                        "type": {
                            "type": "string",
                            "enum": ["bug", "security", "architecture", "performance", "quality"],
                            "description": "Finding category: 'bug' (incorrect behavior, panic, or leak), 'security' (security flaw or unsafe boundary), 'architecture' (structural or runtime design risk), 'performance' (costly hot path or wasted resources), 'quality' (maintainability, readability, or dead-code cleanup)"
                        },
                        "description": {
                            "type": "string",
                            "description": "Technical description of the defect: what is wrong, why it occurs, and its root cause"
                        },
                        "recommendations": {
                            "type": "array",
                            "items": {
                                "type": "string"
                            },
                            "description": "Concrete remediation strategy: ordered, actionable steps that resolve the defect"
                        },
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "string"
                            },
                            "description": "Repository-relative paths of the files to modify when resolving the finding (empty array when the finding is not file-specific)"
                        },
                        "readonly_files": {
                            "type": "array",
                            "items": {
                                "type": "string"
                            },
                            "description": "Repository-relative paths of immutable contracts, interfaces, or invariants the implementation must rely on but must never modify (empty array when the finding depends on none)"
                        }
                    },
                    "required": ["title", "type", "description", "recommendations", "files", "readonly_files"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "title", "findings"],
        "additionalProperties": false
    })
}

/// Builds the provider [`ResponseFormat`] that enforces the review JSON schema.
pub fn review_findings_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "review_findings".to_string(),
            description: Some(
                "Structured code review findings with a typed category, a remediation strategy, and file locations"
                    .to_string(),
            ),
            schema: review_findings_response_schema(),
            strict: Some(true),
        },
    }
}

/// Model-emitted review synthesis payload.
///
/// `title` names the session, `summary` carries the reviewer's high-level
/// Markdown narrative, and `findings` carries the structured findings.
#[derive(Debug, Deserialize)]
pub struct ReviewFindingsOutput {
    #[serde(default)]
    pub title: String,
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
    #[serde(rename = "type", alias = "severity", alias = "issue_type", default)]
    pub issue_type: IssueType,
    /// Technical description of the defect. The legacy `body` key is still
    /// accepted so pre-existing payloads stay readable.
    #[serde(default, alias = "body")]
    pub description: String,
    #[serde(default)]
    pub recommendations: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub readonly_files: Vec<String>,
    /// Legacy single-file location folded into `files` during normalization.
    #[serde(default)]
    pub file_path: Option<String>,
}

/// Parses the model's review synthesis output into a session title, a
/// high-level summary, and structured issues.
///
/// The payload is decoded by deserializing strict JSON. Surrounding markdown
/// code fences or wrapper prose are tolerated, but the payload itself must form
/// a valid JSON document. The returned title and summary are `None` when the
/// model produced no textual conclusion. When no structured payload can be
/// recovered, the raw text is preserved as a single informational issue so
/// review output is never silently discarded.
pub fn parse_review_output(raw: &str) -> (Option<String>, Option<String>, Vec<ReviewIssue>) {
    match parse_findings_output(raw) {
        Some(output) => {
            let ReviewFindingsOutput {
                title,
                summary,
                findings,
            } = output;
            let issues = findings
                .into_iter()
                .enumerate()
                .map(|(index, finding)| {
                    let issue_type = finding.issue_type;
                    let mut files = normalize_list(finding.files);
                    if files.is_empty() {
                        if let Some(path) = finding.file_path {
                            let trimmed = path.trim();
                            if !trimmed.is_empty() {
                                files.push(trimmed.to_string());
                            }
                        }
                    }
                    ReviewIssue {
                        id: index as u32 + 1,
                        title: normalize_title(finding.title, issue_type),
                        model: None,
                        issue_type,
                        status: IssueStatus::Discussion,
                        description: finding.description.trim().to_string(),
                        recommendations: normalize_list(finding.recommendations),
                        files,
                        readonly_files: normalize_list(finding.readonly_files),
                        plan: Vec::new(),
                        note: None,
                    }
                })
                .collect();
            (
                normalize_optional(title),
                normalize_optional(summary),
                issues,
            )
        }
        None => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                (None, None, Vec::new())
            } else {
                (
                    None,
                    None,
                    vec![ReviewIssue {
                        id: 1,
                        title: "General Review Findings".to_string(),
                        model: None,
                        issue_type: IssueType::Quality,
                        status: IssueStatus::Discussion,
                        description: trimmed.to_string(),
                        recommendations: Vec::new(),
                        files: Vec::new(),
                        readonly_files: Vec::new(),
                        plan: Vec::new(),
                        note: None,
                    }],
                )
            }
        }
    }
}

/// Parses the model's review synthesis output into structured `ReviewIssue` instances.
///
/// Compatibility wrapper around [`parse_review_output`] that discards the
/// session title and the high-level summary.
pub fn parse_review_findings(raw: &str) -> Vec<ReviewIssue> {
    parse_review_output(raw).2
}

fn normalize_title(title: String, issue_type: IssueType) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        format!("{} Finding", issue_type.display_badge())
    } else {
        trimmed.to_string()
    }
}

/// Trims an optional model-emitted string, yielding `None` when it is empty.
fn normalize_optional(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Trims a list of model-emitted strings, dropping blank entries.
fn normalize_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_structured_json_review() {
        let raw = r#"{
            "title": "Concurrency and allocation audit",
            "summary": "The architecture is sound, but locking is racy.",
            "findings": [
                {
                    "title": "Race condition in state mutex",
                    "type": "bug",
                    "description": "Multiple background tasks acquire read locks concurrently while writing.",
                    "recommendations": ["Hold the write lock across the whole turn", "Add a regression test"],
                    "files": ["crates/server/src/state.rs", "crates/server/src/handlers/review.rs"],
                    "readonly_files": ["crates/protocol/src/model.rs"]
                },
                {
                    "title": "Inefficient string cloning",
                    "type": "performance",
                    "description": "Cloning strings inside the loop creates memory churn.",
                    "recommendations": ["Borrow instead of cloning"],
                    "files": ["crates/tui/src/ui/wrap.rs"],
                    "readonly_files": []
                },
                {
                    "title": "Use SmallVec for items",
                    "type": "quality",
                    "description": "Can reduce heap allocations for small hunk counts.",
                    "recommendations": [],
                    "files": [],
                    "readonly_files": []
                }
            ]
        }"#;

        let (title, summary, issues) = parse_review_output(raw);
        assert_eq!(title.as_deref(), Some("Concurrency and allocation audit"));
        assert_eq!(
            summary.as_deref(),
            Some("The architecture is sound, but locking is racy.")
        );
        assert_eq!(issues.len(), 3);

        assert_eq!(issues[0].id, 1);
        assert_eq!(issues[0].issue_type, IssueType::Bug);
        assert_eq!(issues[0].title, "Race condition in state mutex");
        assert_eq!(
            issues[0].files,
            vec![
                "crates/server/src/state.rs",
                "crates/server/src/handlers/review.rs"
            ]
        );
        assert_eq!(issues[0].readonly_files, vec!["crates/protocol/src/model.rs"]);
        assert_eq!(
            issues[0].recommendations,
            vec![
                "Hold the write lock across the whole turn",
                "Add a regression test"
            ]
        );
        assert_eq!(issues[0].status, IssueStatus::Discussion);
        assert!(issues[0].description.contains("Multiple background tasks"));

        assert_eq!(issues[1].issue_type, IssueType::Performance);
        assert_eq!(issues[1].files, vec!["crates/tui/src/ui/wrap.rs"]);
        assert!(issues[1].readonly_files.is_empty());

        assert_eq!(issues[2].issue_type, IssueType::Quality);
        assert!(issues[2].files.is_empty());
        assert!(issues[2].recommendations.is_empty());
    }

    #[test]
    fn test_parse_review_findings_tolerates_fences_and_wrapper_prose() {
        let raw = "Here is the review:\n```json\n{\"findings\":[{\"title\":\"Bug\",\"type\":\"architecture\",\"description\":\"details\",\"files\":[],\"readonly_files\":[]}]}\n```\nHope this helps!";
        let issues = parse_review_findings(raw);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "Bug");
        assert_eq!(issues[0].issue_type, IssueType::Architecture);
        assert_eq!(issues[0].description, "details");
        assert!(issues[0].files.is_empty());
    }

    #[test]
    fn test_parse_review_findings_accepts_legacy_aliases() {
        let raw = r#"{"findings":[{"title":"Legacy","severity":"warning","body":"legacy body","file_path":"crates/core/src/review/parser.rs"}]}"#;
        let issues = parse_review_findings(raw);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue_type, IssueType::Architecture);
        assert_eq!(issues[0].description, "legacy body");
        assert_eq!(issues[0].files, vec!["crates/core/src/review/parser.rs"]);
    }

    #[test]
    fn test_parse_review_findings_empty_array_yields_no_items() {
        let issues = parse_review_findings("{\"findings\": []}");
        assert!(issues.is_empty());
    }

    #[test]
    fn test_fallback_unstructured_review() {
        let md = "Overall looks good, but pay attention to error handling in main.rs.";
        let (title, summary, issues) = parse_review_output(md);
        assert!(title.is_none());
        assert!(summary.is_none());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "General Review Findings");
        assert_eq!(issues[0].issue_type, IssueType::Quality);
        assert_eq!(issues[0].description, md);
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
        assert_eq!(schema["required"], serde_json::json!(["summary", "title", "findings"]));

        let finding = &schema["properties"]["findings"]["items"];
        assert_eq!(finding["additionalProperties"], serde_json::json!(false));
        let required = finding["required"].as_array().unwrap();
        assert_eq!(required.len(), 6);

        let type_enum = finding["properties"]["type"]["enum"].as_array().unwrap();
        let categories: Vec<&str> = type_enum
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            categories,
            vec!["bug", "security", "architecture", "performance", "quality"]
        );
        assert!(
            !categories.contains(&"info"),
            "the schema must not expose the info category"
        );
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
        assert_eq!(required[1], serde_json::json!("title"));
        assert_eq!(required[2], serde_json::json!("findings"));

        let schema_str = serde_json::to_string(&schema).unwrap();
        let summary_idx = schema_str.find("\"summary\"").expect("summary present");
        let title_idx = schema_str.find("\"title\"").expect("title present");
        let findings_idx = schema_str.find("\"findings\"").expect("findings present");
        assert!(summary_idx < title_idx);
        assert!(title_idx < findings_idx);
    }

    #[test]
    fn test_parse_review_output_extracts_title_summary_and_findings() {
        let raw = r#"{
            "title": "Locking audit",
            "summary": "The architecture is sound, but locking is racy.",
            "findings": [
                {
                    "title": "Race condition",
                    "type": "bug",
                    "description": "Lock ordering is inconsistent.",
                    "recommendations": ["Enforce a single lock order"],
                    "files": ["crates/server/src/state.rs"],
                    "readonly_files": []
                }
            ]
        }"#;

        let (title, summary, issues) = parse_review_output(raw);
        assert_eq!(title.as_deref(), Some("Locking audit"));
        assert_eq!(
            summary.as_deref(),
            Some("The architecture is sound, but locking is racy.")
        );
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "Race condition");
        assert_eq!(issues[0].issue_type, IssueType::Bug);
        assert_eq!(issues[0].files, vec!["crates/server/src/state.rs"]);
        assert_eq!(issues[0].recommendations, vec!["Enforce a single lock order"]);
    }

    #[test]
    fn test_parse_review_output_blank_or_missing_text_is_none() {
        let (title, summary, issues) =
            parse_review_output(r#"{"title":"   ","summary":"   ","findings":[]}"#);
        assert!(title.is_none());
        assert!(summary.is_none());
        assert!(issues.is_empty());

        let (title, summary, issues) = parse_review_output(r#"{"findings":[]}"#);
        assert!(title.is_none());
        assert!(summary.is_none());
        assert!(issues.is_empty());
    }

    #[test]
    fn test_parse_review_output_unstructured_fallback_has_no_title_or_summary() {
        let (title, summary, issues) = parse_review_output("Just prose, no JSON payload.");
        assert!(title.is_none());
        assert!(summary.is_none());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue_type, IssueType::Quality);
    }

    #[test]
    fn test_parse_review_findings_wrapper_discards_title_and_summary() {
        let raw = r#"{
            "title": "Session",
            "summary": "High-level conclusion.",
            "findings": [
                {
                    "title": "Cloning in loop",
                    "type": "quality",
                    "description": "Avoid clones.",
                    "recommendations": [],
                    "files": [],
                    "readonly_files": []
                }
            ]
        }"#;
        let issues = parse_review_findings(raw);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "Cloning in loop");
        assert_eq!(issues[0].issue_type, IssueType::Quality);
    }

    #[test]
    fn test_issue_type_deserialization_and_legacy_aliases() {
        fn category(payload: &str) -> IssueType {
            parse_review_findings(payload)[0].issue_type
        }

        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"bug"}]}"#),
            IssueType::Bug
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"security"}]}"#),
            IssueType::Security
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"architecture"}]}"#),
            IssueType::Architecture
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"performance"}]}"#),
            IssueType::Performance
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"quality"}]}"#),
            IssueType::Quality
        );

        // Legacy severity words stay readable.
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"critical"}]}"#),
            IssueType::Bug
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"warning"}]}"#),
            IssueType::Architecture
        );
        assert_eq!(
            category(r#"{"findings":[{"title":"a","type":"suggestion"}]}"#),
            IssueType::Quality
        );
    }

    #[test]
    fn test_empty_finding_title_falls_back_to_category_badge() {
        let issues = parse_review_findings(r#"{"findings":[{"title":"  ","type":"security"}]}"#);
        assert_eq!(issues[0].title, "SEC Finding");
    }
}
