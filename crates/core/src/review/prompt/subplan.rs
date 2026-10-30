//! Architectural-context translation for review-group subplans.
//!
//! A group's subplan holds implementation steps only, so a sub-step turn would
//! otherwise see neither the shared root cause nor the findings that manifest
//! it. The translation turns the group into plan scaffolding — the description
//! and the concept tree — that the isolated step prompt renders, so model-facing
//! prompt content lives beside the review prompt builders instead of in the
//! server crate.

use tauqe_protocol::plan::ConceptItem;
use tauqe_protocol::{Plan, ReviewGroup, ReviewItem};

/// Identifier of the concept root every review group turn renders.
const REVIEW_GROUP_CONCEPT_ROOT_ID: &str = "grp-1";

/// Title of the immutable architectural decision node of a review subplan.
const REVIEW_CONCEPT_INVARIANT_TITLE: &str = "Decision Immutability";

/// Contract the reviewer fixed and a sub-step turn must not re-derive.
const REVIEW_CONCEPT_INVARIANT_DETAILS: &str = "The reviewer fixed what must hold; a sub-step owns only how it is implemented. Do not replace the designed data structures, rewrite the interfaces the finding defines, substitute the selected components or libraries, or relax the recorded invariants. If a sub-step cannot be implemented within this architectural decision, report it as a blocked outcome instead of improvising an alternative design.";

/// Builds the immutable-decision node that opens every review concept root.
fn invariant_node(root_id: &str) -> ConceptItem {
    ConceptItem {
        id: format!("{root_id}.1"),
        title: REVIEW_CONCEPT_INVARIANT_TITLE.to_string(),
        details: Some(REVIEW_CONCEPT_INVARIANT_DETAILS.to_string()),
        children: Vec::new(),
    }
}

/// Appends one numbered child node below a review concept root.
fn push_concept_child(
    children: &mut Vec<ConceptItem>,
    root_id: &str,
    title: String,
    details: Option<String>,
) {
    let next_id = format!("{root_id}.{}", children.len() + 1);
    children.push(ConceptItem {
        id: next_id,
        title,
        details,
        children: Vec::new(),
    });
}

/// Translates the architectural context of a review group into plan scaffolding.
///
/// A group's subplan holds implementation steps only, so a group sub-step turn
/// would otherwise see neither the shared root cause nor the findings that
/// manifest it. The group description becomes the plan description, the concept
/// tree always carries the immutable-decision invariant the reviewer fixed, and
/// every finding becomes a child node that names the symptom and, when the
/// reviewer stated one, its body. A whole group therefore renders the cause and
/// its manifestations together for every sub-step it executes.
pub fn apply_group_architectural_context(
    plan: &mut Plan,
    group: &ReviewGroup,
    findings: &[ReviewItem],
) {
    let description = group
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    plan.description = description.map(str::to_string);

    let mut children = vec![invariant_node(REVIEW_GROUP_CONCEPT_ROOT_ID)];
    for finding in findings {
        let title = format!("#{} {}", finding.id, finding.title.trim());
        let body = finding.body.trim();
        let details = if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        };
        push_concept_child(&mut children, REVIEW_GROUP_CONCEPT_ROOT_ID, title, details);
    }

    plan.concept = vec![ConceptItem {
        id: REVIEW_GROUP_CONCEPT_ROOT_ID.to_string(),
        title: format!(
            "Architectural decision of review group '{}'",
            group.title.trim()
        ),
        details: None,
        children,
    }];
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueStatus, ReviewIssue};

    /// Builds a finding whose architectural context the translation consumes.
    fn finding(body: &str, recommendations: &[&str]) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id: 7,
            title: "Guard the parser".to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: tauqe_protocol::review::IssueType::Bug,
            status: IssueStatus::Todo,
            description: body.to_string(),
            recommendations: recommendations.iter().map(|r| (*r).to_string()).collect(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        })
    }

    /// Builds a group whose shared root cause the translation consumes.
    fn group(description: Option<&str>) -> ReviewGroup {
        ReviewGroup {
            id: "arch-1".to_string(),
            title: "Unified error handling".to_string(),
            description: description.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn group_description_becomes_plan_description() {
        let mut plan = Plan::default();
        apply_group_architectural_context(
            &mut plan,
            &group(Some("  Every module invents its own error type.  ")),
            &[],
        );
        assert_eq!(
            plan.description.as_deref(),
            Some("Every module invents its own error type.")
        );
    }

    #[test]
    fn absent_group_description_keeps_the_invariant_node() {
        let mut plan = Plan::default();
        apply_group_architectural_context(&mut plan, &group(None), &[]);

        assert!(plan.description.is_none());
        let root = plan.concept.first().expect("concept root");
        assert_eq!(
            root.title,
            "Architectural decision of review group 'Unified error handling'"
        );
        // The immutable-decision node is rendered even without a stated root
        // cause, so a sub-step turn always sees the decision it may not re-derive.
        assert_eq!(root.children.len(), 1);
        let invariant = root.children.first().expect("invariant node");
        assert_eq!(invariant.title, REVIEW_CONCEPT_INVARIANT_TITLE);
        assert!(invariant.details.is_some());
    }

    #[test]
    fn group_findings_become_concept_children() {
        let mut first = finding("The parser panics on truncated input.", &[]);
        first.id = 7;
        let mut second = finding("   ", &[]);
        second.id = 9;
        second.title = "Second symptom".to_string();

        let mut plan = Plan::default();
        apply_group_architectural_context(
            &mut plan,
            &group(Some("Shared cause")),
            &[first, second],
        );

        let root = plan.concept.first().expect("concept root");
        let titles: Vec<&str> = root.children.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                REVIEW_CONCEPT_INVARIANT_TITLE,
                "#7 Guard the parser",
                "#9 Second symptom"
            ]
        );
        assert_eq!(
            root.children[1].details.as_deref(),
            Some("The parser panics on truncated input.")
        );
        // A blank finding body renders no details branch at all.
        assert!(root.children[2].details.is_none());
    }
}
