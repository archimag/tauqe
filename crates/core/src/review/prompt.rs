//! Prompt builders for review workflows.
//!
//! The review prompt surface is decomposed into three cohesive submodules:
//! `discussion` (review Q&A and full-session audit prompts), `refine`
//! (Architect finding decomposition into a resolution subplan), and `step`
//! (isolated finding resolution, retry, and skip handling). Every public item
//! is re-exported here so callers keep using the stable `review::prompt::*`
//! paths.

pub mod discussion;
pub mod refine;
pub mod step;

pub use discussion::{
    build_review_system_prompt, build_review_user_prompt, format_review_discussion_prompt,
    format_review_discussion_prompt_items, REVIEW_DISCUSSION_PREFIX,
};
pub use refine::{
    build_review_finding_refine_system_prompt, build_review_finding_refine_user_prompt,
    build_review_refine_system_prompt, build_review_refine_user_prompt, MAX_REVIEW_SUBPLAN_DEPTH,
};
pub use step::{
    format_review_step_execution_prompt, format_review_step_failure_note,
    format_review_step_retry_prompt, ReviewStepFailure, REVIEW_STEP_EXECUTION_PREFIX,
    REVIEW_STEP_RETRY_PREFIX,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::ReviewSession;
    use tauqe_protocol::PlanItem;

    #[test]
    fn test_format_review_step_retry_prompt() {
        let prompt = format_review_step_retry_prompt(
            "I inspected the file and it looks correct.",
            Some("No edits were applied"),
        );
        assert!(prompt.contains("# Correction Required: Review Finding Turn Incomplete"));
        assert!(prompt.contains("<previous_response>"));
        assert!(prompt.contains("I inspected the file and it looks correct."));
        assert!(prompt.contains("## Diagnostic Feedback:"));
        assert!(prompt.contains("No edits were applied"));
        assert!(prompt.contains("emit a valid edit block"));
        assert!(prompt.contains("terminal success tag"));

        let empty = format_review_step_retry_prompt("   ", None);
        assert!(empty.contains("(empty response)"));
        assert!(!empty.contains("<previous_response>"));
        assert!(!empty.contains("Diagnostic Feedback"));
    }

    #[test]
    fn test_format_review_step_execution_prompt_with_subplan() {
        use tauqe_protocol::review::{IssueType, ReviewItem, ReviewItemStatus};
        use tauqe_protocol::PlanItemStatus;

        let session = ReviewSession {
            id: "rev-sub".to_string(),
            title: "Subplan Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: None,
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Refactor architecture".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                severity: tauqe_protocol::ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                recommendations: vec![],
                files: vec!["crates/core/src/lib.rs".to_string()],
                readonly_files: vec![],
                subplan: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Split module".to_string(),
                    details: Some("Create submodules".to_string()),
                    status: PlanItemStatus::Done,
                    concept_refs: vec![],
                    files: vec!["crates/core/src/sub.rs".to_string()],
                    readonly_files: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
                subplan_architect: None,
                resolution_notes: None,
                body: "Break down large file".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_step_execution_prompt(&session, 1, "_TAG").unwrap();
        assert!(prompt.contains("Decomposed Subplan Steps:"));
        assert!(prompt.contains(
            "- [DONE] #1.1 Split module (files: crates/core/src/sub.rs): Create submodules"
        ));
    }

    #[test]
    fn test_build_review_finding_refine_prompts() {
        use tauqe_protocol::review::{IssueType, ReviewItem, ReviewItemStatus};

        let session = ReviewSession {
            id: "rev-ref".to_string(),
            title: "Refinement Session".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: Some("Audit findings to refine".to_string()),
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 42,
                title: "Decompose this issue".to_string(),
                model: None,
                issue_type: IssueType::Bug,
                severity: tauqe_protocol::ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                recommendations: vec![],
                files: vec!["crates/core/src/lib.rs".to_string()],
                readonly_files: vec![],
                subplan: vec![],
                subplan_architect: None,
                resolution_notes: Some("Break into sub-steps".to_string()),
                body: "Needs structural division".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let sys = build_review_finding_refine_system_prompt();
        assert!(sys.contains("Review Finding Decomposition"));
        assert!(sys.contains("Compilability Invariants"));

        let user = build_review_finding_refine_user_prompt(
            &session,
            &session.items[0],
            &[],
            Some("fn outline()"),
            None,
            None,
            Some("Focus on safety"),
        );
        assert!(user.contains("# Review Finding Refinement: [rev-ref#42] Decompose this issue"));
        assert!(user.contains("Focus on safety"));
        assert!(user.contains("fn outline()"));
        assert!(user.contains("Break into sub-steps"));
    }

    #[test]
    fn test_format_review_step_execution_prompt() {
        use tauqe_protocol::review::{IssueType, ReviewItem, ReviewItemStatus};

        let session = ReviewSession {
            id: "rev-7".to_string(),
            title: "Performance Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: Some("Optimize string copies".to_string()),
            user_prompt: None,
            target_files: vec!["crates/tui/src/view.rs".to_string()],
            items: vec![ReviewItem {
                id: 3,
                title: "Excessive clone in hot path".to_string(),
                model: None,
                issue_type: IssueType::Performance,
                severity: tauqe_protocol::ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                recommendations: vec![],
                files: vec!["crates/tui/src/view.rs".to_string()],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "Replace clone with reference borrowing".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_step_execution_prompt(&session, 3, "_TAG").unwrap();
        assert!(prompt
            .contains("# Target Review Step Execution: [rev-7:3] Excessive clone in hot path"));
        assert!(prompt.contains("<success_TAG summary=\"...\" />"));
        assert!(prompt.contains("<error_TAG message=\"...\" />"));
    }

    #[test]
    fn test_review_discussion_prompt_formatting() {
        use tauqe_protocol::review::{IssueType, ReviewItem, ReviewItemStatus};
        use tauqe_protocol::ModelRef;

        let session = ReviewSession {
            id: "rev-42".to_string(),
            title: "Security Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("test-model"),
            description: Some("Security review for core crate".to_string()),
            user_prompt: None,
            target_files: vec!["src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Race condition in cache".to_string(),
                model: None,
                issue_type: IssueType::Bug,
                severity: tauqe_protocol::ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                recommendations: vec![],
                files: vec!["src/lib.rs".to_string()],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: Some("Extend scope of mutex guard".to_string()),
                body: "Lock is dropped prematurely.".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_discussion_prompt(&session, Some(1), "Is this really possible?");
        assert!(prompt.contains("# Review Discussion: [rev-42] Security Audit"));
        assert!(prompt.contains("Race condition in cache"));
        assert!(prompt.contains("Resolution Note: Extend scope of mutex guard"));
        assert!(prompt.contains("Is this really possible?"));
        assert!(
            prompt.contains("explain the proposed modifications clearly in conversational text")
        );
        // A focused review discussion must let the model either reason about
        // the finding or apply the requested fix directly.
        assert!(!prompt.contains("Do NOT emit file edits"));
        assert!(prompt
            .contains("apply the required code changes directly instead of only describing them"));
    }

    #[test]
    fn test_build_review_system_prompt_requires_structured_output() {
        let prompt = build_review_system_prompt();
        assert!(prompt.contains("structured output schema"));
        assert!(prompt.contains("`summary`"));
        assert!(prompt.contains("`title`"));
        assert!(prompt.contains("`findings`"));
        assert!(prompt.contains("`type`"));
        assert!(prompt.contains("`description`"));
        assert!(prompt.contains("`recommendations`"));
        assert!(prompt.contains("`readonly_files`"));
        assert!(!prompt.contains("## [SEVERITY]"));
        assert!(
            !prompt.contains("`severity`"),
            "the schema must not expose the legacy severity field"
        );

        let summary_idx = prompt.find("`summary`").expect("summary documented");
        let title_idx = prompt.find("`title`").expect("title documented");
        let findings_idx = prompt.find("`findings`").expect("findings documented");
        assert!(
            summary_idx < title_idx,
            "summary must be specified before title"
        );
        assert!(
            title_idx < findings_idx,
            "title must be specified before findings"
        );
    }

    #[test]
    fn test_review_step_failure_classification_and_note() {
        let domain = ReviewStepFailure::domain("compiler error");
        assert!(domain.is_domain());
        assert_eq!(domain.reason(), "compiler error");

        let infra = ReviewStepFailure::infrastructure("429 rate limit");
        assert!(!infra.is_domain());
        assert_eq!(infra.into_reason(), "429 rate limit");

        assert_eq!(
            format_review_step_failure_note("  compiler still fails  "),
            "Skipped after execution failure: compiler still fails"
        );
        assert_eq!(
            format_review_step_failure_note("   "),
            "Skipped after execution failure without diagnostics"
        );
    }
}
