//! Architectural-context translation for review-finding subplans.
//!
//! A finding's embedded subplan holds implementation steps only, so a sub-step
//! turn would otherwise see neither the reviewer's rationale nor the contract it
//! established. The translation turns the finding into plan scaffolding — the
//! description and the concept tree — that the isolated step prompt renders, so
//! model-facing prompt content lives beside the review prompt builders instead
//! of in the server crate.

use tauqe_protocol::plan::ConceptItem;
use tauqe_protocol::{Plan, ReviewItem};

/// Identifier of the concept root every review subplan turn renders.
const REVIEW_CONCEPT_ROOT_ID: &str = "rev-1";

/// Title of the immutable architectural decision node of a review subplan.
const REVIEW_CONCEPT_INVARIANT_TITLE: &str = "Decision Immutability";

/// Contract the reviewer fixed and a sub-step turn must not re-derive.
const REVIEW_CONCEPT_INVARIANT_DETAILS: &str = "The reviewer fixed what must hold; a sub-step owns only how it is implemented. Do not replace the designed data structures, rewrite the interfaces the finding defines, substitute the selected components or libraries, or relax the recorded invariants. If a sub-step cannot be implemented within this architectural decision, report it as a blocked outcome instead of improvising an alternative design.";

/// Translates the architectural context of a review finding into plan scaffolding.
///
/// A finding's embedded subplan holds implementation steps only, so a sub-step
/// turn would otherwise see neither the reviewer's rationale nor the contract it
/// established. The finding body becomes the plan description, and the concept
/// tree always carries the immutable-decision invariant the reviewer fixed, so
/// the step prompt renders its `Architectural Concept` section for every
/// sub-step. The reviewer's recommendations become sibling concept nodes, and a
/// blank entry is dropped instead of rendering an empty branch.
pub fn apply_finding_architectural_context(plan: &mut Plan, item: &ReviewItem) {
    let body = item.body.trim();
    plan.description = if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    };

    let mut children = vec![ConceptItem {
        id: format!("{REVIEW_CONCEPT_ROOT_ID}.1"),
        title: REVIEW_CONCEPT_INVARIANT_TITLE.to_string(),
        details: Some(REVIEW_CONCEPT_INVARIANT_DETAILS.to_string()),
        children: Vec::new(),
    }];

    let mut next_index = 2usize;
    for recommendation in &item.recommendations {
        let trimmed = recommendation.trim();
        if trimmed.is_empty() {
            continue;
        }
        children.push(ConceptItem {
            id: format!("{REVIEW_CONCEPT_ROOT_ID}.{next_index}"),
            title: trimmed.to_string(),
            details: None,
            children: Vec::new(),
        });
        next_index += 1;
    }

    let finding_id = item.id;
    let finding_title = item.title.trim();
    plan.concept = vec![ConceptItem {
        id: REVIEW_CONCEPT_ROOT_ID.to_string(),
        title: format!("Architectural decision of review finding #{finding_id}: {finding_title}"),
        details: None,
        children,
    }];
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueStatus, ReviewIssue};
    use tauqe_protocol::PlanItemStatus;

    /// Builds a finding whose architectural context the translation consumes.
    fn finding(body: &str, recommendations: &[&str]) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id: 7,
            title: "Guard the parser".to_string(),
            model: None,
            issue_type: tauqe_protocol::review::IssueType::Bug,
            status: IssueStatus::Todo,
            description: body.to_string(),
            recommendations: recommendations.iter().map(|r| (*r).to_string()).collect(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        })
    }

    #[test]
    fn finding_body_becomes_plan_description() {
        let mut plan = Plan::default();
        apply_finding_architectural_context(&mut plan, &finding("  The parser panics.  ", &[]));
        assert_eq!(plan.description.as_deref(), Some("The parser panics."));
    }

    #[test]
    fn blank_finding_body_leaves_description_absent() {
        let mut plan = Plan::default();
        apply_finding_architectural_context(&mut plan, &finding("   ", &[]));
        assert!(plan.description.is_none());
    }

    #[test]
    fn invariant_node_is_always_rendered() {
        let mut plan = Plan::default();
        apply_finding_architectural_context(&mut plan, &finding("body", &[]));
        let root = plan.concept.first().expect("concept root");
        assert_eq!(root.children.len(), 1);
        let invariant = root.children.first().expect("invariant node");
        assert_eq!(invariant.title, REVIEW_CONCEPT_INVARIANT_TITLE);
        assert!(invariant.details.is_some());
    }

    #[test]
    fn recommendations_become_concept_children() {
        let mut plan = Plan::default();
        apply_finding_architectural_context(
            &mut plan,
            &finding("body", &["Keep it lazy", "  ", "Avoid clones"]),
        );
        let root = plan.concept.first().expect("concept root");
        let titles: Vec<&str> = root.children.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                REVIEW_CONCEPT_INVARIANT_TITLE,
                "Keep it lazy",
                "Avoid clones"
            ]
        );
    }

    /// Builds a finding whose embedded subplan carries one executable sub-step.
    fn finding_with_subplan(body: &str) -> ReviewItem {
        let mut item = finding(body, &["Keep it lazy"]);
        item.subplan = vec![tauqe_protocol::plan::PlanItem {
            id: "1".to_string(),
            title: "Return a structured error".to_string(),
            details: Some("Reuse the existing scanner.".to_string()),
            status: PlanItemStatus::Todo,
            files: vec!["crates/x/src/parser.rs".to_string()],
            readonly_files: vec!["crates/x/src/api.rs".to_string()],
            concept_refs: Vec::new(),
            depends_on: Vec::new(),
            children: Vec::new(),
        }];
        item
    }

    #[test]
    fn translated_context_reaches_the_substep_prompt() {
        let item = finding_with_subplan("The parser panics on truncated input.");
        // Mirrors how the subplan walk assembles its working plan.
        let mut plan = Plan {
            id: "R-1".to_string(),
            title: "Parser hardening".to_string(),
            items: item.subplan.clone(),
            ..Default::default()
        };
        apply_finding_architectural_context(&mut plan, &item);

        let prompt = crate::plan::prompt::format_plan_step_execution_prompt(&plan, "1", "_M7")
            .expect("sub-step prompt");

        // The finding body becomes the plan description and the reviewer's
        // decision becomes the concept section, so the sub-step turn sees the
        // context the walk used to lose.
        assert!(prompt.contains("The parser panics on truncated input."));
        assert!(prompt.contains(REVIEW_CONCEPT_INVARIANT_TITLE));
        assert!(prompt.contains("review finding #7"));
        assert!(prompt.contains("Do not replace the designed data structures"));
        assert!(prompt.contains("Keep it lazy"));
        assert!(prompt.contains("<blocked_M7 reason="));
    }
}
