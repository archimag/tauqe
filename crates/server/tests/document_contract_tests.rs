//! Persistence and client-projection compatibility for owned documents.
use tauqe_tui::projection::{PlanImplementationProjection, ReviewProjection};
#[path = "../src/contracts.rs"]
mod contracts;
use contracts::Project;
use tauqe_core::{models, plan, review};
use tauqe_protocol as wire;

fn leaf(id: &str, status: plan::PlanItemStatus) -> plan::PlanItem {
    plan::PlanItem {
        id: id.into(),
        title: format!("Step {id}"),
        status,
        files: vec![
            "./src//file.rs".into(),
            "src\\file.rs".into(),
            r"src\\literal.rs".into(),
            "../outside".into(),
            "./C:/absolute".into(),
        ],
        readonly_files: vec!["src/file.rs".into(), "README.md".into()],
        ..Default::default()
    }
}

#[test]
fn plan_documents_round_trip_without_losing_provenance_or_graph_fields() {
    let mut step = leaf("2", plan::PlanItemStatus::Blocked);
    step.blocked_reason = Some("Needs a decision".into());
    step.depends_on = vec!["1".into()];
    step.concept_refs = vec!["c1".into()];
    step.outer_docs = vec!["architecture".into()];
    let mut done = leaf("1", plan::PlanItemStatus::Done);
    done.commit = Some("abc123".into());
    let mut document = plan::Plan::from_parts(
        "plan-1".into(),
        "Plan".into(),
        Some("Concept".into()),
        1,
        2,
        vec![plan::ConceptItem {
            id: "c1".into(),
            title: "Invariant".into(),
            ..Default::default()
        }],
        vec![done, step],
    );
    document.implementation.architect = Some(models::ArchitectTier {
        model: models::ModelRef::openrouter("vendor/architect"),
        effort: Some(models::ReasoningEffort::XHigh),
        attempts: Some(4),
        level_up: Some(false),
    });
    document.implementation.last_replanned_step = Some("2".into());
    let saved = serde_json::to_value(&document).unwrap();
    let projected: wire::Plan = document.clone().project();
    assert_eq!(serde_json::to_value(&projected).unwrap(), saved);
    let restored: plan::Plan = projected.project();
    assert_eq!(restored, document);
    let reread: plan::Plan = serde_json::from_value(saved).unwrap();
    assert_eq!(reread, document);
    let summary: wire::PlanSummary = document.summary().project();
    assert_eq!(
        serde_json::to_value(summary).unwrap(),
        serde_json::to_value(document.summary()).unwrap()
    );
}

#[test]
fn client_graph_queries_match_authoritative_queries_for_every_leaf_status() {
    let statuses = [
        plan::PlanItemStatus::Discussion,
        plan::PlanItemStatus::Todo,
        plan::PlanItemStatus::InProgress,
        plan::PlanItemStatus::Done,
        plan::PlanItemStatus::Blocked,
        plan::PlanItemStatus::Cancelled,
    ];
    for first in statuses {
        for second in statuses {
            let mut dependent = leaf("2", second);
            dependent.depends_on = vec![" 1 ".into()];
            let container = plan::PlanItem {
                id: "group".into(),
                title: "Group".into(),
                children: vec![leaf("1", first), dependent],
                ..Default::default()
            };
            let document = plan::Plan::from_parts(
                "p".into(),
                "Projection".into(),
                None,
                1,
                2,
                vec![],
                vec![container],
            );
            let mut domain = document.implementation.clone();
            let mut client = wire::Plan::from_parts(
                "p".into(),
                "Projection".into(),
                None,
                1,
                2,
                vec![],
                document
                    .implementation
                    .items
                    .clone()
                    .into_iter()
                    .map(Project::project)
                    .collect(),
            )
            .implementation;
            assert_eq!(
                format!("{:?}", domain.status()),
                format!("{:?}", client.status())
            );
            assert_eq!(domain.count_stats(), client.count_stats());
            for scope in [None, Some("group"), Some("1"), Some("2"), Some("missing")] {
                assert_eq!(
                    format!("{:?}", domain.evaluate_scope(scope)),
                    format!("{:?}", client.evaluate_scope(scope)),
                    "scope {scope:?}, statuses {first:?}/{second:?}"
                );
            }
            domain.normalize_leaf_files();
            client.normalize_leaf_files();
            domain.refresh_parent_statuses();
            client.refresh_parent_statuses();
            assert_eq!(
                serde_json::to_value(domain).unwrap(),
                serde_json::to_value(client).unwrap()
            );
        }
    }
}

#[test]
fn review_storage_keeps_legacy_model_names_aliases_and_group_horizons() {
    let legacy = serde_json::json!({
        "id":"review-1", "created_at":1, "model":"old-provider:model", "raw_markdown":"raw",
        "items":[{"id":1,"title":"Finding","severity":"performance","status":"todo","description":"Detail","note":"Decision"}],
        "groups":[{"id":"group-1","title":"Cause","paths":["src/**"],"kind":"architectural", "implementation":{"items":[{"id":"1","title":"Fix","files":["src/file.rs"],"status":"done","commit":"abc123"}]}}]
    });
    let domain: review::ReviewSession = serde_json::from_value(legacy.clone()).unwrap();
    let client: wire::ReviewSession = serde_json::from_value(legacy).unwrap();
    assert_eq!(domain.model.name, "old-provider:model");
    assert_eq!(domain.items[0].issue_type, review::IssueType::Performance);
    assert_eq!(domain.items[0].severity, review::ReviewSeverity::Warning);
    assert_eq!(
        serde_json::to_value(&domain).unwrap(),
        serde_json::to_value(&client).unwrap()
    );
    let projected: wire::ReviewSession = domain.clone().project();
    assert_eq!(projected, client);
    let restored: review::ReviewSession = projected.project();
    assert_eq!(restored, domain);
    assert_eq!(domain.stats(), client.stats());
}

#[test]
fn review_projection_preserves_post_analysis_and_frozen_launch_configuration() {
    let mut issue = review::ReviewItem::from(review::ReviewIssue {
        id: 7,
        title: "Security".into(),
        uuid: Some("finding-uuid".into()),
        group: Some("g".into()),
        model: Some(models::ModelSelection::Tier {
            model: models::ModelRef::openrouter("reviewer"),
            effort: Some(models::ReasoningEffort::Max),
        }),
        issue_type: review::IssueType::Security,
        status: review::IssueStatus::Fixed,
        description: "Fixed issue".into(),
        recommendations: vec!["Verify".into()],
        files: vec!["src/file.rs".into()],
        readonly_files: vec!["README.md".into()],
        commit: Some("abc123".into()),
        note: Some("Resolved".into()),
    });
    issue.resolution_notes = Some("Decision".into());
    let group = review::ReviewGroup {
        id: "g".into(),
        title: "Root cause".into(),
        paths: vec!["src/**".into()],
        kind: review::ReviewGroupKind::Architectural,
        resolution_notes: Some("Decision".into()),
        ..Default::default()
    };
    let domain = review::ReviewSession {
        id: "r".into(),
        title: "Review".into(),
        items: vec![issue],
        groups: vec![group.clone()],
        post_analysis: Some(review::ReviewPostAnalysis {
            created_at: 3,
            summary: "Summary".into(),
            independent_issues: vec![],
            architectural_groups: vec![review::ArchitecturalIssueGroup {
                id: "a".into(),
                title: "Cause".into(),
                description: "Why".into(),
                issue_ids: vec!["finding-uuid".into()],
                implementation: plan::PlanImplementation::default(),
            }],
        }),
        launch_config: Some(review::ReviewLaunchConfig {
            preset: Some("audit".into()),
            mode: review::PostAnalysisMode::Automatic,
            groups: vec![group],
        }),
        ..Default::default()
    };
    let projected: wire::ReviewSession = domain.clone().project();
    assert_eq!(
        serde_json::to_value(&domain).unwrap(),
        serde_json::to_value(&projected).unwrap()
    );
    let restored: review::ReviewSession = projected.project();
    assert_eq!(restored, domain);
}
