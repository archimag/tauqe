//! Prompt builders, DTOs, and JSON schemas for plan workflows.
//!
//! The plan prompt surface is decomposed into five cohesive submodules:
//! `architect` (the Architect tier-resolution policy shared by plan and review
//! synthesis), `critique` (the independent audit of a draft horizon and the
//! reconciliation that answers it), `discussion` (plan Q&A and active-context
//! rendering), `refine` (Architect horizon synthesis and its schemas), and
//! `step` (isolated plan step execution). Every public item is re-exported here
//! so callers keep using the stable `plan::prompt::*` paths.

pub mod architect;
pub mod critique;
pub mod discussion;
pub mod refine;
pub mod step;

pub use architect::{resolve_architect_tier, ArchitectLevelUp};
pub use critique::{
    build_plan_critique_prompt, build_plan_critique_system_prompt,
    build_plan_reconciliation_prompt, parse_plan_critique_output, plan_critique_response_schema,
    plan_critique_response_schema_definition, PlanCritiqueCategory, PlanCritiqueFinding,
    PlanCritiqueOutput, PlanCritiqueVerdict,
};
pub use discussion::{
    format_active_plan_context, format_plan_discussion_prompt, PLAN_DISCUSSION_PREFIX,
};
pub use refine::{
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt, parse_plan_refinement_output,
    plan_refine_response_schema, plan_refine_response_schema_definition, PlanHorizonDiagnostics,
    PlanRefinementAbort, PlanRefinementOutput, MAX_PLAN_REFINEMENT_DEPTH,
};
pub use step::{
    format_plan_step_execution_prompt, format_plan_step_retry_prompt, PLAN_STEP_EXECUTION_PREFIX,
    PLAN_STEP_RETRY_PREFIX,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{EditProtocolKind, Plan, PlanItem, PlanItemStatus};

    fn sample_plan() -> Plan {
        Plan::from_parts(
            "jwt-auth".to_string(),
            "JWT Authentication".to_string(),
            Some("Implement secure tokens".to_string()),
            100,
            200,
            vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Stateless JWT".to_string(),
                details: Some("Use asymmetric ECDSA".to_string()),
                children: vec![],
            }],
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: Some("Claims struct".to_string()),
                    status: PlanItemStatus::Done,
                    blocked_reason: None,
                    commit: None,
                    concept_refs: vec!["c-1".to_string()],
                    files: vec!["crates/core/src/models.rs".to_string()],
                    readonly_files: vec![],
                    outer_docs: vec![],
                    depends_on: vec![],
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    blocked_reason: None,
                    commit: None,
                    concept_refs: vec!["c-1".to_string()],
                    files: vec!["crates/core/src/handler.rs".to_string()],
                    readonly_files: vec!["crates/core/src/contract.rs".to_string()],
                    outer_docs: vec![],
                    depends_on: vec!["1".to_string()],
                    children: vec![],
                },
            ],
        )
    }

    #[test]
    fn test_format_active_plan_context() {
        let formatted = format_active_plan_context(&sample_plan()).unwrap();
        assert!(formatted.contains("<active_plan id=\"jwt-auth\""));
        assert!(formatted.contains("progress=\"1/2 completed\""));
        assert!(formatted.contains("Endpoint (id: \"2\", status: IN_PROGRESS)"));
        assert!(formatted.contains("[concept_refs: c-1]"));
        assert!(formatted.contains("[files: crates/core/src/handler.rs]"));
        assert!(formatted.contains("[depends_on: 1]"));
        assert!(formatted.contains(": Write handler"));
    }

    #[test]
    fn test_format_plan_step_execution_prompt() {
        let plan = sample_plan();

        let prompt =
            format_plan_step_execution_prompt(&plan, "2", EditProtocolKind::Structured, None)
                .unwrap();
        assert!(prompt.contains("Target Plan Step Execution: [jwt-auth:2] Endpoint"));
        assert!(prompt.contains("Already Completed Prerequisites:"));
        assert!(prompt.contains("Architectural Concept (System Invariants & Focus):"));
        assert!(prompt.contains("- #c-1 Stateless JWT: Use asymmetric ECDSA"));
        assert!(prompt.contains("- **Target Files (to modify)**: crates/core/src/handler.rs"));
        assert!(prompt
            .contains("- **Reference Files (read-only context)**: crates/core/src/contract.rs"));
        assert!(prompt.contains("- **Addressed Concept Nodes**: c-1"));
        assert!(prompt.contains("- **Prerequisite Steps (must be DONE)**: 1"));

        assert!(prompt.contains("- [x] #1 Models"));
        assert!(prompt.contains("- **ID**: `2`"));
        assert!(prompt.contains("`changes`"));
        assert!(prompt.contains("commit_message"));
    }
    #[test]
    fn test_format_plan_step_retry_prompt() {
        let prompt = format_plan_step_retry_prompt(
            "I think we are done.",
            Some("No edits were applied"),
            EditProtocolKind::Structured,
            None,
        );
        assert!(prompt.contains("Correction Required"));
        assert!(prompt.contains("<previous_response>"));
        assert!(prompt.contains("I think we are done."));
        assert!(prompt.contains("No edits were applied"));
        assert!(prompt.contains("`reason`"));

        let empty = format_plan_step_retry_prompt("   ", None, EditProtocolKind::Structured, None);
        assert!(!empty.contains("<previous_response>"));
        assert!(!empty.contains("Diagnostic Feedback"));
    }

    #[test]
    fn test_plan_horizon_prompts_and_schema() {
        let plan = sample_plan();
        let sys = build_plan_horizon_system_prompt();
        assert!(sys.contains("Wave Horizon Planning"));
        assert!(sys.contains("Concept Immutability"));
        assert!(sys.contains("Horizon Synthesis Contract"));
        assert!(sys.contains("Strict Blocker Protocol"));
        assert!(sys.contains("Compilability Invariants"));
        assert!(sys.contains("Leaf-Only Files Invariant"));
        assert!(sys.contains("Reference Files Contract"));
        assert!(sys.contains("DAG Dependency Contract"));

        let user_prompt = build_plan_horizon_user_prompt(&plan, None, Some("Focus on EdDSA"));
        assert!(user_prompt.contains("# Plan Horizon Synthesis: [jwt-auth] JWT Authentication"));
        assert!(user_prompt.contains("Immutable Source of Truth"));
        assert!(user_prompt.contains("## Completed Foundation"));
        assert!(user_prompt.contains("- [x] #1 Models"));
        assert!(user_prompt.contains("Focus on EdDSA"));
        assert!(user_prompt.contains("readonly_files"));

        let schema = plan_refine_response_schema();
        assert!(schema.get("properties").is_some());
        assert!(
            !schema.to_string().contains("$ref"),
            "Schema must not contain recursive $ref"
        );
        assert!(schema.to_string().contains("readonly_files"));
        assert!(schema.to_string().contains("depends_on"));
        assert!(schema.to_string().contains("New unfinished horizon"));
        let def = plan_refine_response_schema_definition();
        assert_eq!(def["name"], "plan_refinement");

        let raw_success = r#"{
            "status": "success",
            "explanation": "Decomposed into model and endpoint steps",
            "reason": null,
            "items": [
                {
                    "id": "1",
                    "title": "Model definition",
                    "details": "JWT claims struct",
                    "status": "todo",
                    "files": ["crates/core/src/models.rs"],
                    "readonly_files": ["crates/core/src/contract.rs"],
                    "children": []
                }
            ]
        }"#;
        let parsed = parse_plan_refinement_output(raw_success).unwrap();
        assert_eq!(parsed.status, "success");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].id, "1");
        assert_eq!(
            parsed.items[0].readonly_files,
            vec!["crates/core/src/contract.rs"]
        );

        let raw_blocked = r#"```json
        {
            "status": "blocked",
            "explanation": "Cannot proceed",
            "reason": "Missing cryptographic library in cargo dependencies",
            "context_requests": [],
            "items": []
        }
        ```"#;
        let parsed_blocked = parse_plan_refinement_output(raw_blocked).unwrap();
        assert_eq!(parsed_blocked.status, "blocked");
        assert_eq!(
            parsed_blocked.reason.as_deref(),
            Some("Missing cryptographic library in cargo dependencies")
        );
        assert!(parsed_blocked.items.is_empty());

        let raw_needs_ctx = r#"{
            "status": "needs_context",
            "explanation": "Need to inspect toolchain module",
            "reason": null,
            "context_requests": ["crates/core/src/toolchain.rs"],
            "items": []
        }"#;
        let parsed_ctx = parse_plan_refinement_output(raw_needs_ctx).unwrap();
        assert_eq!(parsed_ctx.status, "needs_context");
        assert_eq!(
            parsed_ctx.context_requests,
            vec!["crates/core/src/toolchain.rs"]
        );
        assert!(parsed_ctx.items.is_empty());

        let raw_deep = r#"{
            "status": "success",
            "explanation": "Deep 5-level decomposition",
            "reason": null,
            "items": [
                {
                    "id": "1",
                    "title": "Level 1",
                    "details": null,
                    "status": "todo",
                    "children": [
                        {
                            "id": "1.1",
                            "title": "Level 2",
                            "details": null,
                            "status": "todo",
                            "children": [
                                {
                                    "id": "1.1.1",
                                    "title": "Level 3",
                                    "details": null,
                                    "status": "todo",
                                    "children": [
                                        {
                                            "id": "1.1.1.1",
                                            "title": "Level 4",
                                            "details": null,
                                            "status": "todo",
                                            "children": [
                                                {
                                                    "id": "1.1.1.1.1",
                                                    "title": "Level 5",
                                                    "details": null,
                                                    "status": "todo"
                                                }
                                            ]
                                        }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        let parsed_deep = parse_plan_refinement_output(raw_deep).unwrap();
        assert_eq!(
            parsed_deep.items[0].children[0].children[0].children[0].children[0].id,
            "1.1.1.1.1"
        );
        assert!(parsed_deep.items[0].children[0].children[0].children[0].children[0].is_leaf());
    }

    #[test]
    fn test_format_plan_discussion_prompt() {
        let plan = sample_plan();

        let prompt =
            format_plan_discussion_prompt(&plan, &["2".to_string()], "Should we use EdDSA?");
        assert!(prompt.contains("# Plan Discussion: [jwt-auth] JWT Authentication"));
        assert!(prompt.contains("Implement secure tokens"));
        assert!(prompt.contains("<active_plan id=\"jwt-auth\""));
        assert!(prompt.contains("Items Focused by User for Discussion:"));
        assert!(prompt.contains("- **#2 Endpoint**"));
        assert!(prompt.contains("Should we use EdDSA?"));
        assert!(prompt.contains("Instructions for Model:"));
    }

    #[test]
    fn test_plan_discussion_prompt_attribute_escaping() {
        let plan = Plan::from_parts(
            "auth-v2".to_string(),
            "OAuth & JWT".to_string(),
            Some("Tokens & Refresh".to_string()),
            0,
            0,
            vec![],
            vec![],
        );
        let prompt = format_plan_discussion_prompt(&plan, &[], "Clarify signing");
        assert!(prompt.contains("<active_plan id=\"auth-v2\" title=\"OAuth &amp; JWT\""));
    }

    #[test]
    fn test_build_plan_horizon_user_prompt_with_diagnostics() {
        let plan = sample_plan();
        let diagnostics = PlanHorizonDiagnostics {
            blocked_step_id: "2",
            blocked_reason: Some("Missing crypto dependency"),
            toolchain_status: Some("cargo check: 1 error"),
        };
        let prompt = build_plan_horizon_user_prompt(&plan, Some(diagnostics), None);
        assert!(prompt.contains("# Plan Horizon Synthesis: [jwt-auth] JWT Authentication"));
        assert!(prompt.contains("## Architectural Concept"));
        assert!(prompt.contains("## Completed Foundation"));
        assert!(prompt.contains("- [x] #1 Models"));
        assert!(prompt.contains("## Blocked Step Diagnostics"));
        assert!(prompt.contains("- **Blocked Step**: #2 Endpoint"));
        assert!(prompt.contains("- **Coder Blocker Report**: Missing crypto dependency"));
        assert!(prompt.contains("- **Toolchain Status**: cargo check: 1 error"));
        assert!(prompt.contains("Architect Instructions"));

        // Without diagnostics the failure section is omitted entirely.
        let plain = build_plan_horizon_user_prompt(&plan, None, None);
        assert!(!plain.contains("Blocked Step Diagnostics"));

        // A plan with no DONE steps reports an empty foundation.
        let mut no_done = sample_plan();
        for item in &mut no_done.implementation.items {
            item.status = PlanItemStatus::Todo;
        }
        let no_done_prompt = build_plan_horizon_user_prompt(&no_done, None, None);
        assert!(no_done_prompt.contains("*(No completed steps yet)*"));
    }

    /// Pins the external-specification contract of a plan to the public
    /// facade: the Architect declares the project `[[outer_docs]]` entries a
    /// leaf must be implemented against, and the executor receives them as
    /// preloaded read-only reference material together with the prefix-bounded
    /// request directive of its contour.
    #[test]
    fn test_plan_prompts_publish_the_external_specification_contract() {
        let mut plan = sample_plan();
        let step = plan
            .implementation
            .items
            .iter_mut()
            .find(|item| item.id.as_str() == "2")
            .expect("the endpoint step");
        step.outer_docs = vec!["vendor/api.md".to_string()];

        let tagged =
            format_plan_step_execution_prompt(&plan, "2", EditProtocolKind::Structured, None)
                .expect("the execution prompt");
        assert!(tagged.contains("Preloaded External Specifications (read-only reference)"));
        assert!(tagged.contains("vendor/api.md"));
        assert!(tagged.contains("`outer_doc_requests`"));
        assert!(tagged.contains("never name a path that climbs above the declared root"));

        // A structured turn emits no command tag, so the same contract arrives
        // as the schema field it decodes into.
        let structured =
            format_plan_step_execution_prompt(&plan, "2", EditProtocolKind::Structured, None)
                .expect("the execution prompt");
        assert!(structured.contains("vendor/api.md"));
        assert!(structured.contains("`outer_doc_requests`"));
        assert!(!structured.contains("<outer_doc_request"));

        // The Architect side publishes the same contract: the system prompt
        // mandates the declaration and the strict schema describes the prefix.
        let horizon_system = build_plan_horizon_system_prompt();
        assert!(horizon_system.contains("External Specification Contract (Outer Docs)"));
        assert!(horizon_system.contains("outer_docs: [\"id\"]"));
        assert!(horizon_system.contains("never climb above it"));

        let horizon_user = build_plan_horizon_user_prompt(&plan, None, None);
        assert!(horizon_user.contains("external specifications the project publishes"));
        assert!(horizon_user.contains("`outer_docs`"));

        let schema = plan_refine_response_schema();
        let outer_docs = &schema["properties"]["items"]["items"]["properties"]["outer_docs"];
        let description = outer_docs["description"]
            .as_str()
            .expect("the outer_docs property declares its contract");
        assert!(description.contains("id/relative.md"));
        assert!(description.contains("parent container items must have an empty outer_docs list"));
    }

    /// Pins the abort escape hatch to the stable `plan::prompt::*` path.
    ///
    /// A reported host failure is part of the refinement contract, so a caller
    /// reaches its payload through the same facade as every other item of the
    /// prompt surface.
    #[test]
    fn test_the_abort_escape_hatch_reaches_callers_through_the_facade() {
        let output = PlanRefinementOutput {
            status: "blocked".to_string(),
            explanation: "The toolchain cannot run".to_string(),
            reason: None,
            abort: Some(PlanRefinementAbort {
                is_aborted: true,
                reason: "clippy is not installed".to_string(),
                recommendation: "rustup component add clippy".to_string(),
            }),
            context_requests: Vec::new(),
            repo_searches: Vec::new(),
            repo_maps: Vec::new(),
            items: Vec::new(),
        };

        let abort = output.abort.expect("an active abort");
        assert!(abort.is_aborted);
        assert_eq!(abort.recommendation, "rustup component add clippy");
    }
}
