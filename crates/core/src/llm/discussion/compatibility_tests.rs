//! Legacy model payload behavior retained after removing unused RPC exports.
use super::*;
use crate::llm::discussion::ReviewDecisionStatus;
use crate::plan_item::{ConceptItem, PlanItem, PlanItemStatus};
#[test]
fn test_discussion_plan_action_display_and_serde() {
    assert_eq!(DiscussionPlanAction::Update.to_string(), "update");
    assert_eq!(DiscussionPlanAction::Save.to_string(), "save");

    let json = serde_json::to_string(&DiscussionPlanAction::Save).unwrap();
    assert_eq!(json, "\"save\"");
    let de: DiscussionPlanAction = serde_json::from_str(&json).unwrap();
    assert_eq!(de, DiscussionPlanAction::Save);
}

#[test]
fn test_discussion_response_minimal_serde() {
    let raw = r#"{"message":"Hello developer!"}"#;
    let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(resp.message, "Hello developer!");
    assert!(resp.plan_update.is_none());
    assert!(resp.context_requests.is_empty());
    assert!(resp.repo_searches.is_empty());
    assert!(resp.repo_dirs.is_none());
    assert!(resp.repo_maps.is_empty());
    assert!(resp.dir_lists.is_empty());
    assert!(resp.review_update.is_none());
}

#[test]
fn test_discussion_response_review_update_roundtrip() {
    let response =
        DiscussionResponse::new("Decision recorded").with_review_update(ReviewUpdateProposal {
            item_id: 7,
            group_id: Some("arch-group".to_string()),
            status: ReviewDecisionStatus::Rejected,
            note: "The reported race cannot happen: the guard is released after the await"
                .to_string(),
            recommendations: Vec::new(),
        });

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"review_update\":{"), "got: {json}");
    assert!(json.contains("\"status\":\"rejected\""), "got: {json}");
    assert!(json.contains("\"item_id\":7"), "got: {json}");
    assert!(json.contains("\"group_id\":\"arch-group\""), "got: {json}");
    let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(de, response);

    // A turn that records no decision states it explicitly and keeps the
    // field out of the wire form, exactly like an absent plan update.
    let idle = serde_json::to_string(&DiscussionResponse::new("Hello")).unwrap();
    assert!(!idle.contains("review_update"), "got: {idle}");
}

#[test]
fn test_discussion_response_plan_update_error_is_a_decode_diagnostic() {
    // The field records why a present `plan_update` could not be honored, so
    // the diagnosis travels with the response instead of disappearing.
    let mut response = DiscussionResponse::new("Partial decode");
    response.plan_update_error = Some("invalid plan_update: missing field `action`".to_string());

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"plan_update_error\""), "got: {json}");
    let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(de, response);

    // An absent diagnosis declares nothing and stays out of the wire form.
    let idle = serde_json::to_string(&DiscussionResponse::new("Hello")).unwrap();
    assert!(!idle.contains("plan_update_error"), "got: {idle}");
    let de: DiscussionResponse = serde_json::from_str(&idle).unwrap();
    assert!(de.plan_update_error.is_none());
}

#[test]
fn test_discussion_response_with_nulls_serde() {
    let raw = r#"{
        "message": "Acknowledged",
        "plan_update": null,
        "review_update": null,
        "context_requests": null,
        "repo_searches": null,
        "repo_dirs": null,
        "repo_maps": null,
        "dir_lists": null
    }"#;
    let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(resp.message, "Acknowledged");
    assert!(resp.plan_update.is_none());
    assert!(resp.review_update.is_none());
    assert!(resp.context_requests.is_empty());
    assert!(resp.repo_searches.is_empty());
    assert!(resp.repo_dirs.is_none());
    assert!(resp.repo_maps.is_empty());
    assert!(resp.dir_lists.is_empty());
}

#[test]
fn test_discussion_response_full_roundtrip() {
    let resp = DiscussionResponse::new("Proceed with the protocol update")
        .with_repo_dirs(true)
        .with_repo_maps(vec!["./crates/core/".to_string()])
        .with_dir_lists(vec![DirListRequest::new("./crates/core/", true)])
        .with_context_requests(vec![
            DiscussionContextRequest::read_only("crates/core/src/lib.rs"),
            DiscussionContextRequest::editable("crates/protocol/src/lib.rs"),
        ])
        .with_repo_searches(vec![RepoSearchRequest::new(
            "crates/core",
            r"pub fn run_\w+",
        )])
        .with_plan_update(DiscussionPlanUpdate {
            action: DiscussionPlanAction::Update,
            id: Some("structured-output".to_string()),
            title: Some("Structured Output".to_string()),
            description: Some("Implement JSON schema protocol".to_string()),
            concept: vec![ConceptItem {
                id: "c-1".to_string(),
                title: "One wire contract".to_string(),
                details: Some("Schemas mirror the Rust data model".to_string()),
                children: vec![],
            }],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Protocol types".to_string(),
                details: Some("Add discussion schemas".to_string()),
                status: PlanItemStatus::Done,
                blocked_reason: None,
                commit: None,
                files: vec!["crates/protocol/src/discussion.rs".to_string()],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec!["c-1".to_string()],
                depends_on: vec![],
                children: vec![],
            }],
        });

    let json = serde_json::to_string_pretty(&resp).unwrap();
    let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(de, resp);
}

#[test]
fn test_context_requests_builder_applies_repository_path_rules() {
    // The builder shares the boundary of its sibling builders: an entry that
    // the decode path drops never reaches the response through this
    // construction path either, so a directly built response carries only
    // repository-relative paths.
    let response = DiscussionResponse::new("Hello").with_context_requests(vec![
        DiscussionContextRequest::read_only("./crates/core/src/lib.rs"),
        DiscussionContextRequest::editable("/etc/passwd"),
        DiscussionContextRequest::read_only("../escape.rs"),
        DiscussionContextRequest::editable("   "),
        DiscussionContextRequest::editable("crates/core/src/lib.rs"),
    ]);

    // The two reachable requests keep their order, normalized path, and
    // requested access level.
    assert_eq!(response.context_requests.len(), 2);
    assert_eq!(response.context_requests[0].path, "crates/core/src/lib.rs");
    assert_eq!(response.context_requests[0].access, ContextAccess::ReadOnly);
    assert_eq!(response.context_requests[1].path, "crates/core/src/lib.rs");
    assert_eq!(response.context_requests[1].access, ContextAccess::Editable);

    // An empty request list stays empty instead of defaulting to anything.
    assert!(DiscussionResponse::new("Hello")
        .with_context_requests(Vec::new())
        .context_requests
        .is_empty());
}

#[test]
fn test_discovery_builders_keep_dot_compositions_as_the_repository_root() {
    // The whole-repository decision follows the decoded segment set, so a
    // composition of dot and separator segments reaches the same root as
    // the empty path instead of being dropped by the builder.
    let response = DiscussionResponse::new("Root")
        .with_repo_maps(vec![
            "./.".to_string(),
            "././".to_string(),
            ".//".to_string(),
            "./crates/core/".to_string(),
        ])
        .with_dir_lists(vec![
            DirListRequest::new("./.", true),
            DirListRequest::new(".//", false),
            DirListRequest::new("./crates/protocol/", true),
        ])
        .with_repo_searches(vec![
            RepoSearchRequest::new("././", "pub fn"),
            RepoSearchRequest::new("./crates/core/", "fn run"),
        ]);

    // Every dot composition survives as the whole-repository root in the
    // order it was declared, beside a real subtree that keeps its path.
    assert_eq!(response.repo_maps.len(), 4);
    assert!(response.repo_maps[..3].iter().all(|path| path.is_empty()));
    assert_eq!(response.repo_maps[3], "crates/core");

    assert_eq!(response.dir_lists.len(), 3);
    assert!(response.dir_lists[0].path.is_empty());
    assert!(response.dir_lists[0].recursive);
    assert!(response.dir_lists[1].path.is_empty());
    assert!(!response.dir_lists[1].recursive);
    assert_eq!(response.dir_lists[2].path, "crates/protocol");
    assert!(response.dir_lists[2].recursive);

    assert_eq!(response.repo_searches.len(), 2);
    assert!(response.repo_searches[0].path.is_empty());
    assert_eq!(response.repo_searches[0].regex, "pub fn");
    assert_eq!(response.repo_searches[1].path, "crates/core");
    assert_eq!(response.repo_searches[1].regex, "fn run");
}
