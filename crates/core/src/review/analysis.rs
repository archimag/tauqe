//! Review post-analysis: strict schema and tolerant decoding.
//!
//! Post-analysis is the Architect role's answer over an immutable set of
//! findings: it decides which findings stand on their own and which ones are
//! manifestations of a shared architectural root cause. It rewrites no finding
//! and produces no code, so the payload travels through the Structured Output
//! contour (`crates/core/Conventions.md`, section 1.2) and is decoded by
//! [`parse_post_analysis`] instead of by an XML parser.
//!
//! Findings are referenced exclusively by the UUIDv7 identity they were minted
//! with. [`resolve_identity`] normalizes every reference before resolving it,
//! so a decorated spelling of a real identity is accepted while a hallucinated
//! one is rejected with a diagnostic instead of entering the classification
//! layer.

use serde::Deserialize;

use crate::edits::stream::json::decode::{balanced_object_end, strip_code_fence};
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

/// Strict JSON Schema for the review post-analysis output.
///
/// Post-analysis never produces code changes, so it is a Structured Output
/// operation. The Architect role receives the immutable findings of every
/// review group and classifies them into findings that stand on their own and
/// clusters that share one architectural root cause. Findings are referenced
/// by their UUIDv7 identity, so the classification is a projection that never
/// rewrites a finding.
///
/// The schema is deliberately flat: it contains no recursive `$ref` and
/// declares `additionalProperties: false` on every object, so a Structured
/// Output engine can enforce it verbatim.
pub fn post_analysis_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "High-level Markdown narrative of the systemic root causes found across the whole session, in the primary natural language of the user's instructions. Final distilled content only: no chain-of-thought, deliberation, or intermediate reasoning steps."
            },
            "independent_issues": {
                "type": "array",
                "description": "UUIDv7 identifiers of the findings that stand on their own and share no root cause with another finding. Every identifier must name a finding of this session.",
                "items": {
                    "type": "string",
                    "description": "UUIDv7 identifier of a finding"
                }
            },
            "architectural_groups": {
                "type": "array",
                "description": "Clusters of findings that manifest one shared architectural root cause",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Concise title of the shared root cause in the primary natural language of the user's instructions"
                        },
                        "description": {
                            "type": "string",
                            "description": "Explanation of why these findings belong together and which root cause they manifest, in the primary natural language of the user's instructions"
                        },
                        "issue_ids": {
                            "type": "array",
                            "description": "UUIDv7 identifiers of the findings that manifest this root cause. Every identifier must name a finding of this session.",
                            "items": {
                                "type": "string",
                                "description": "UUIDv7 identifier of a finding"
                            }
                        }
                    },
                    "required": ["title", "description", "issue_ids"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "independent_issues", "architectural_groups"],
        "additionalProperties": false
    })
}

/// Builds the provider [`ResponseFormat`] that enforces the post-analysis schema.
pub fn post_analysis_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "review_post_analysis".to_string(),
            description: Some(
                "Classification of review findings into independent issues and architectural root-cause groups"
                    .to_string(),
            ),
            schema: post_analysis_response_schema(),
            strict: Some(true),
        },
    }
}

/// Model-emitted post-analysis payload.
///
/// `summary` carries the Architect's global verdict, `independent_issues`
/// carries the findings that stand on their own, and `architectural_groups`
/// carries the clusters that share one root cause. Every collection defaults
/// to empty, so a partial payload still decodes and the caller decides whether
/// the classification is complete enough to persist.
#[derive(Debug, Deserialize)]
pub struct PostAnalysisOutput {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub independent_issues: Vec<String>,
    #[serde(default)]
    pub architectural_groups: Vec<PostAnalysisGroupOutput>,
}

/// One model-emitted cluster of findings that share a root cause.
#[derive(Debug, Deserialize)]
pub struct PostAnalysisGroupOutput {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub issue_ids: Vec<String>,
}

/// Parses the model's post-analysis output into a classification payload.
///
/// Decoding tolerates the wrapping a model sometimes adds: a markdown code
/// fence is unwrapped first, and when wrapper prose surrounds the payload every
/// balanced JSON object is tried until one decodes. The payload itself must
/// form a valid JSON document, because post-analysis runs through the
/// Structured Output contour and a failure to decode it is a real failure
/// rather than recoverable prose.
pub fn parse_post_analysis(raw: &str) -> anyhow::Result<PostAnalysisOutput> {
    let unfenced = strip_code_fence(raw);
    let error = match serde_json::from_str::<PostAnalysisOutput>(unfenced) {
        Ok(output) => return Ok(output),
        Err(error) => error,
    };

    // Wrapper prose may itself contain braces, so every balanced object is
    // tried until one decodes as a post-analysis payload.
    let mut from = 0;
    while let Some(rel) = raw[from..].find('{') {
        let start = from + rel;
        if let Some(end) = balanced_object_end(raw, start) {
            if let Ok(output) = serde_json::from_str::<PostAnalysisOutput>(&raw[start..end]) {
                return Ok(output);
            }
        }
        from = start + 1;
    }

    Err(anyhow::anyhow!(
        "Failed to parse review post-analysis JSON output: {error}"
    ))
}

/// Characters a model may wrap an identity reference in.
const IDENTITY_DECORATION: &[char] = &['`', '"', '\'', '*', '«', '»', '“', '”'];

/// Strips the presentation wrapper a model may put around one identity.
///
/// Post-analysis is decoded from model output, so an identity can arrive
/// wrapped in a markdown code span, surrounded by quotes, or labelled with a
/// `uuid:` prefix even though the prompt asks for the bare value. Normalizing
/// the reference keeps resolution about the identifier itself instead of the
/// way it was decorated.
fn normalize_identity(raw: &str) -> &str {
    let mut value = raw.trim();
    loop {
        let before = value;
        value = value.trim_matches(IDENTITY_DECORATION).trim();
        value = strip_identity_label(value).unwrap_or(value);
        if value == before {
            return value;
        }
    }
}

/// Strips a leading `uuid:` label from an identity reference.
fn strip_identity_label(value: &str) -> Option<&str> {
    let label = value.get(..5)?;
    if !label.eq_ignore_ascii_case("uuid:") {
        return None;
    }
    Some(&value[5..])
}

/// Resolves one model-emitted identity reference against the session.
///
/// The reference is normalized first, so a decorated spelling still resolves to
/// the identity the session holds instead of failing on presentation alone.
/// Comparison ignores ASCII case, and the resolved value is the identity as the
/// session stores it, never the spelling the model used.
pub fn resolve_identity<'a>(raw: &str, known_uuids: &'a [String]) -> Option<&'a str> {
    let candidate = normalize_identity(raw);
    if candidate.is_empty() {
        return None;
    }
    known_uuids
        .iter()
        .find(|known| known.eq_ignore_ascii_case(candidate))
        .map(String::as_str)
}

/// Resolves every finding reference of a post-analysis payload against the
/// session it classifies.
///
/// Post-analysis is a projection over immutable findings: it may reference a
/// finding by identity, but it can neither invent one nor reach outside the
/// session. Each reference is resolved through [`resolve_identity`], so a
/// decorated spelling of a real identity is accepted while a reference that
/// resolves to nothing is reported together with the identifier that caused it.
pub fn validate_post_analysis(
    output: &PostAnalysisOutput,
    known_uuids: &[String],
) -> Result<(), String> {
    for uuid in &output.independent_issues {
        if resolve_identity(uuid, known_uuids).is_none() {
            return Err(format!(
                "Independent issue references an unknown finding uuid '{}'",
                normalize_identity(uuid)
            ));
        }
    }

    for group in &output.architectural_groups {
        for uuid in &group.issue_ids {
            if resolve_identity(uuid, known_uuids).is_none() {
                return Err(format!(
                    "Architectural group '{}' references an unknown finding uuid '{}'",
                    group.title,
                    normalize_identity(uuid)
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding identity the classified session actually holds.
    const FINDING_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";
    /// Second finding identity the classified session holds.
    const SECOND_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000002";

    fn known_uuids() -> Vec<String> {
        vec![FINDING_UUID.to_string(), SECOND_UUID.to_string()]
    }

    #[test]
    fn test_post_analysis_schema_is_strict_and_non_recursive() {
        let schema = post_analysis_response_schema();
        let text = schema.to_string();
        assert!(
            !text.contains("$ref"),
            "schema must not contain recursive $ref"
        );

        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            schema["required"],
            serde_json::json!(["summary", "independent_issues", "architectural_groups"])
        );

        let group = &schema["properties"]["architectural_groups"]["items"];
        assert_eq!(group["type"], serde_json::json!("object"));
        assert_eq!(group["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            group["required"],
            serde_json::json!(["title", "description", "issue_ids"])
        );
    }

    #[test]
    fn test_summary_is_first_schema_property() {
        let schema = post_analysis_response_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("properties object must exist");
        let first_key = properties.keys().next().expect("first property must exist");
        assert_eq!(first_key, "summary", "summary MUST be the first property");

        let required = schema["required"].as_array().expect("required array");
        assert_eq!(required[0], serde_json::json!("summary"));
        assert_eq!(required[1], serde_json::json!("independent_issues"));
        assert_eq!(required[2], serde_json::json!("architectural_groups"));

        let schema_str = serde_json::to_string(&schema).unwrap();
        let summary_idx = schema_str.find("\"summary\"").expect("summary present");
        let independent_idx = schema_str
            .find("\"independent_issues\"")
            .expect("independent_issues present");
        let groups_idx = schema_str
            .find("\"architectural_groups\"")
            .expect("architectural_groups present");
        assert!(summary_idx < independent_idx);
        assert!(independent_idx < groups_idx);
    }

    #[test]
    fn test_post_analysis_response_format_is_a_strict_json_schema() {
        match post_analysis_response_format() {
            ResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "review_post_analysis");
                assert_eq!(json_schema.strict, Some(true));
                assert_eq!(json_schema.schema, post_analysis_response_schema());
            }
            other => panic!("expected a strict JSON schema response format, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_post_analysis_unwraps_a_plain_fence() {
        let raw = "```json\n{\"summary\":\"Grounded\",\"independent_issues\":[],\"architectural_groups\":[]}\n```";
        let output = parse_post_analysis(raw).unwrap();
        assert_eq!(output.summary, "Grounded");
        assert!(output.independent_issues.is_empty());
        assert!(output.architectural_groups.is_empty());
    }

    #[test]
    fn test_parse_post_analysis_tolerates_fences_and_wrapper_prose() {
        let raw = r#"Here is the classification:
```json
{"summary":"One lock explains both symptoms.","independent_issues":["018f9c1e-6b3a-7c1e-8b3a-000000000001"],"architectural_groups":[{"title":"Shared lock order","description":"Both defects come from one inverted lock order.","issue_ids":["018f9c1e-6b3a-7c1e-8b3a-000000000002"]}]}
```
Let me know if you need more."#;
        let output = parse_post_analysis(raw).unwrap();
        assert_eq!(output.summary, "One lock explains both symptoms.");
        assert_eq!(output.independent_issues, vec![FINDING_UUID]);
        assert_eq!(output.architectural_groups.len(), 1);
        assert_eq!(output.architectural_groups[0].title, "Shared lock order");
        assert_eq!(output.architectural_groups[0].issue_ids, vec![SECOND_UUID]);

        // A bare payload with trailing notes is recovered by the same scan.
        let bare = format!("{raw}\nTrailing notes.");
        assert_eq!(parse_post_analysis(&bare).unwrap().summary, output.summary);
    }

    #[test]
    fn test_parse_post_analysis_applies_defaults_to_absent_collections() {
        let output = parse_post_analysis(r#"{"summary":"No systemic defect found"}"#).unwrap();
        assert_eq!(output.summary, "No systemic defect found");
        assert!(output.independent_issues.is_empty());
        assert!(output.architectural_groups.is_empty());
    }

    #[test]
    fn test_parse_post_analysis_rejects_garbage() {
        assert!(parse_post_analysis("").is_err());
        assert!(parse_post_analysis("No JSON payload at all.").is_err());
        assert!(parse_post_analysis("{\"summary\": \"unterminated").is_err());
    }

    #[test]
    fn test_validate_post_analysis_accepts_known_references() {
        let output = PostAnalysisOutput {
            summary: "One lock explains both symptoms.".to_string(),
            independent_issues: vec![FINDING_UUID.to_string()],
            architectural_groups: vec![PostAnalysisGroupOutput {
                title: "Shared lock order".to_string(),
                description: "Both defects come from one inverted lock order.".to_string(),
                issue_ids: vec![SECOND_UUID.to_string()],
            }],
        };
        assert!(validate_post_analysis(&output, &known_uuids()).is_ok());

        // A session with nothing to classify validates an empty classification.
        let empty = PostAnalysisOutput {
            summary: "No findings".to_string(),
            independent_issues: Vec::new(),
            architectural_groups: Vec::new(),
        };
        assert!(validate_post_analysis(&empty, &[]).is_ok());
    }

    #[test]
    fn test_validate_post_analysis_rejects_foreign_uuid() {
        let foreign = "018f9c1e-6b3a-7c1e-8b3a-0000000000ff";

        let independent = PostAnalysisOutput {
            summary: "Root cause".to_string(),
            independent_issues: vec![foreign.to_string()],
            architectural_groups: Vec::new(),
        };
        let error = validate_post_analysis(&independent, &known_uuids()).unwrap_err();
        assert!(
            error.contains(foreign),
            "diagnostic must name the foreign uuid: {error}"
        );

        let grouped = PostAnalysisOutput {
            summary: "Root cause".to_string(),
            independent_issues: Vec::new(),
            architectural_groups: vec![PostAnalysisGroupOutput {
                title: "Cluster".to_string(),
                description: String::new(),
                issue_ids: vec![FINDING_UUID.to_string(), foreign.to_string()],
            }],
        };
        let error = validate_post_analysis(&grouped, &known_uuids()).unwrap_err();
        assert!(
            error.contains(foreign),
            "diagnostic must name the foreign uuid: {error}"
        );
        assert!(
            error.contains("Cluster"),
            "diagnostic must name the offending group: {error}"
        );
    }

    #[test]
    fn test_validate_post_analysis_accepts_re_spelled_identity() {
        let output = PostAnalysisOutput {
            summary: "Root cause".to_string(),
            independent_issues: vec![FINDING_UUID.to_ascii_uppercase()],
            architectural_groups: Vec::new(),
        };
        assert!(validate_post_analysis(&output, &known_uuids()).is_ok());
    }
    #[test]
    fn test_normalize_identity_strips_decoration_quoting_and_labels() {
        for raw in [
            FINDING_UUID,
            " 018f9c1e-6b3a-7c1e-8b3a-000000000001 ",
            "`018f9c1e-6b3a-7c1e-8b3a-000000000001`",
            "\"018f9c1e-6b3a-7c1e-8b3a-000000000001\"",
            "**018f9c1e-6b3a-7c1e-8b3a-000000000001**",
            "«018f9c1e-6b3a-7c1e-8b3a-000000000001»",
            "uuid:018f9c1e-6b3a-7c1e-8b3a-000000000001",
            "UUID: ` 018f9c1e-6b3a-7c1e-8b3a-000000000001 `",
        ] {
            assert_eq!(
                normalize_identity(raw),
                FINDING_UUID,
                "raw reference: {raw}"
            );
        }
    }

    #[test]
    fn test_normalize_identity_handles_bare_and_empty_references() {
        assert_eq!(normalize_identity("None"), "None");
        assert_eq!(normalize_identity("``"), "");
        assert_eq!(normalize_identity("   "), "");
    }

    #[test]
    fn test_resolve_identity_returns_the_session_identity() {
        let known = known_uuids();

        assert_eq!(
            resolve_identity(" UUID: `018f9c1e-6b3a-7c1e-8b3a-000000000001` ", &known),
            Some(FINDING_UUID)
        );
        assert_eq!(
            resolve_identity("018F9C1E-6B3A-7C1E-8B3A-000000000001", &known),
            Some(FINDING_UUID)
        );
        assert_eq!(resolve_identity("0000000000000000", &known), None);
        assert_eq!(resolve_identity("``", &known), None);
    }

    #[test]
    fn test_validate_post_analysis_accepts_decorated_references() {
        let output = PostAnalysisOutput {
            summary: "One lock explains both symptoms.".to_string(),
            independent_issues: vec![format!("`{FINDING_UUID}`")],
            architectural_groups: vec![PostAnalysisGroupOutput {
                title: "Shared lock order".to_string(),
                description: String::new(),
                issue_ids: vec![format!("UUID: {}", SECOND_UUID.to_ascii_uppercase())],
            }],
        };
        assert!(validate_post_analysis(&output, &known_uuids()).is_ok());
    }
}
