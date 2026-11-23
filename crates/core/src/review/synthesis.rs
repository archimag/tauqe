//! Decode a single audit and materialize its review session.

use crate::models::ModelRef;
use crate::review::parser::SingleReviewOutput;
use crate::review::uuid::generate_uuid_v7;
use crate::review::{ReviewIssue, ReviewItem, ReviewSession};

/// Launch values captured before provider execution, including its cache identity.
pub struct ReviewSynthesisMetadata {
    pub id: String,
    pub created_at: u64,
    pub model: ModelRef,
    pub user_prompt: Option<String>,
    pub target_files: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("Model returned an empty review")]
pub struct EmptyReviewResponse;

/// Project a decoded audit into a session without interpreting provider JSON.
pub fn materialize_review(
    metadata: ReviewSynthesisMetadata,
    decoded: SingleReviewOutput,
) -> ReviewSession {
    let SingleReviewOutput {
        raw_markdown,
        title: parsed_title,
        summary,
        issues,
    } = decoded;
    let title = parsed_title
        .or_else(|| {
            metadata
                .user_prompt
                .as_deref()
                .and_then(|prompt| prompt.lines().next())
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| line.chars().take(60).collect::<String>())
        })
        .unwrap_or_else(|| format!("Review {}", metadata.created_at));
    ReviewSession {
        id: metadata.id,
        title,
        created_at: metadata.created_at,
        model: metadata.model,
        description: summary.or_else(|| metadata.user_prompt.clone()),
        user_prompt: metadata.user_prompt,
        target_files: metadata.target_files,
        items: with_finding_identities(issues),
        raw_markdown,
        groups: Vec::new(),
        post_analysis: None,
        launch_config: None,
    }
}

/// Projects synthesized findings into review items carrying a UUIDv7 identity.
///
/// The reviewer emits findings without identifiers, while the post-analysis
/// layer and the Architect decomposition reference a finding by its UUIDv7.
/// Minting the identity before the session is persisted guarantees that every
/// stored finding of a single audit is addressable, exactly as the grouped
/// audit guarantees it through `ReviewStorage::append_group_findings`. An
/// identity a finding already carries is preserved, so a projection never
/// mints a second one for the same finding.
fn with_finding_identities(issues: Vec<ReviewIssue>) -> Vec<ReviewItem> {
    let mut items = Vec::with_capacity(issues.len());
    for issue in issues {
        let mut item = ReviewItem::from(issue);
        if item.uuid.is_none() {
            item.uuid = Some(generate_uuid_v7());
        }
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::parser::decode_single_review;
    use crate::review::uuid::is_valid_uuid_v7;
    use crate::review::{IssueStatus, IssueType};

    /// Builds a finding exactly as the reviewer parser emits it: a complete
    /// issue without an identity.
    fn synthesized_issue(id: u32, title: &str) -> ReviewIssue {
        ReviewIssue {
            id,
            title: title.to_string(),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: IssueType::Quality,
            status: IssueStatus::Discussion,
            description: "Description".to_string(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        }
    }

    #[test]
    fn every_synthesized_finding_receives_a_valid_uuid_v7() {
        let first = synthesized_issue(1, "First finding");
        let second = synthesized_issue(2, "Second finding");
        let items = with_finding_identities(vec![first, second]);
        assert_eq!(items.len(), 2);

        let mut identities = Vec::new();
        for item in &items {
            let identity = item.uuid.clone().expect("finding identity");
            assert!(is_valid_uuid_v7(&identity), "not a UUIDv7: {identity}");
            identities.push(identity);
        }
        assert_ne!(identities[0], identities[1], "identities are unique");
        assert_eq!(items[0].title, "First finding");
        assert_eq!(items[1].id, 2);
        assert_eq!(items[0].status, IssueStatus::Discussion);
    }

    #[test]
    fn an_identity_a_finding_already_carries_is_preserved() {
        let existing = generate_uuid_v7();
        let mut issue = synthesized_issue(1, "Carried identity");
        issue.uuid = Some(existing.clone());

        let items = with_finding_identities(vec![issue]);
        assert_eq!(items[0].uuid.as_deref(), Some(existing.as_str()));
    }

    fn metadata(prompt: Option<&str>) -> ReviewSynthesisMetadata {
        ReviewSynthesisMetadata {
            id: "launch-review".into(),
            created_at: 42,
            model: ModelRef::openrouter("test/reviewer"),
            user_prompt: prompt.map(str::to_string),
            target_files: vec!["src/lib.rs".into()],
        }
    }

    #[test]
    fn materializes_the_single_audit_shape_and_preserves_launch_metadata_and_raw_text() {
        let raw = serde_json::json!({
            "title": " Model title ", "summary": " Summary\n\"quoted\" ",
            "findings": [{"title": "Avoid cloning", "type": "quality",
                "description": "Borrow instead", "files": ["src/lib.rs"],
                "recommendations": ["Use a reference"], "readonly_files": []}]
        })
        .to_string();
        let review = materialize_review(
            metadata(Some("Prompt fallback")),
            decode_single_review(&raw).unwrap(),
        );
        assert_eq!(review.id, "launch-review");
        assert_eq!(review.created_at, 42);
        assert_eq!(review.model, ModelRef::openrouter("test/reviewer"));
        assert_eq!(review.title, "Model title");
        assert_eq!(review.description.as_deref(), Some("Summary\n\"quoted\""));
        assert_eq!(review.user_prompt.as_deref(), Some("Prompt fallback"));
        assert_eq!(review.target_files, ["src/lib.rs"]);
        assert_eq!(review.raw_markdown, raw);
        assert_eq!(review.items.len(), 1);
        assert!(is_valid_uuid_v7(review.items[0].uuid.as_deref().unwrap()));
        assert!(review.groups.is_empty());
        assert!(review.post_analysis.is_none());
        assert!(review.launch_config.is_none());
    }

    #[test]
    fn title_and_description_fallback_keep_first_line_and_unicode_limits() {
        let prompt = format!("  {}  \nIgnored second line", "界".repeat(70));
        let review = materialize_review(
            metadata(Some(&prompt)),
            decode_single_review(r#"{"title":" ","summary":" ","findings":[]}"#).unwrap(),
        );
        assert_eq!(review.title, "界".repeat(60));
        assert_eq!(review.description.as_deref(), Some(prompt.as_str()));
        let review = materialize_review(
            metadata(Some("  \nDo not use the second line")),
            decode_single_review(r#"{"findings":[]}"#).unwrap(),
        );
        assert_eq!(review.title, "Review 42");
        assert!(review.items.is_empty());
    }

    #[test]
    fn nonempty_unstructured_output_stays_a_finding() {
        let raw = "A non-JSON review that must remain visible.";
        let review = materialize_review(metadata(None), decode_single_review(raw).unwrap());
        assert_eq!(review.raw_markdown, raw);
        assert_eq!(review.items.len(), 1);
        assert!(review.items[0].body.contains(raw));
        assert!(is_valid_uuid_v7(review.items[0].uuid.as_deref().unwrap()));
    }

    #[test]
    fn decoded_audit_materializes_without_losing_provider_bytes() {
        let raw = " \n```json\n{\"title\":\"Audit\",\"summary\":\"OK\",\"findings\":[]}\n``` \n";
        let review = materialize_review(metadata(None), decode_single_review(raw).unwrap());
        assert_eq!(review.raw_markdown, raw);
        assert_eq!(review.title, "Audit");
        assert_eq!(review.description.as_deref(), Some("OK"));
    }
}
