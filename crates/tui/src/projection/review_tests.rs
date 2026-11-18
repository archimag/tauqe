use super::*;
use tauqe_protocol::model::{ArchitectTier, ModelRef, ReasoningEffort};
use tauqe_protocol::*;
fn finding(id: u32, status: IssueStatus) -> ReviewItem {
    ReviewItem {
        id,
        title: format!("Finding {id}"),
        uuid: None,
        commit: None,
        group: None,
        model: None,
        issue_type: IssueType::Bug,
        severity: ReviewSeverity::Critical,
        status,
        recommendations: Vec::new(),
        files: Vec::new(),
        readonly_files: Vec::new(),
        resolution_notes: None,
        body: String::new(),
    }
}
fn session_with(items: Vec<ReviewItem>) -> ReviewSession {
    ReviewSession {
        id: "rev-progress".to_string(),
        title: "Progress".to_string(),
        created_at: 0,
        model: ModelRef::default(),
        items,
        ..Default::default()
    }
}
fn categorized_finding(id: u32, issue_type: IssueType) -> ReviewItem {
    ReviewItem::from(ReviewIssue {
        id,
        title: format!("Finding {issue_type:?}"),
        uuid: None,
        commit: None,
        group: None,
        model: None,
        issue_type,
        status: IssueStatus::Todo,
        description: String::new(),
        recommendations: Vec::new(),
        files: Vec::new(),
        readonly_files: Vec::new(),
        note: None,
    })
}
const ALL_ISSUE_TYPES: [IssueType; 5] = [
    IssueType::Bug,
    IssueType::Security,
    IssueType::Architecture,
    IssueType::Performance,
    IssueType::Quality,
];
#[test]
fn test_issue_status_lifecycle() {
    let status = IssueStatus::Discussion;
    assert_eq!(status.next(), IssueStatus::Todo);
    assert_eq!(status.next().next(), IssueStatus::InProgress);
    assert_eq!(status.next().next().next(), IssueStatus::Fixed);
    assert_eq!(status.next().next().next().next(), IssueStatus::Skipped);
    assert_eq!(
        status.next().next().next().next().next(),
        IssueStatus::Rejected
    );
    assert_eq!(
        status.next().next().next().next().next().next(),
        IssueStatus::Discussion
    );
    assert_eq!(IssueStatus::InProgress.to_string(), "IN_PROGRESS");
    assert_eq!(
        serde_json::to_string(&IssueStatus::Fixed).unwrap(),
        "\"fixed\""
    );
    assert_eq!(
        serde_json::from_str::<IssueStatus>("\"fixed\"").unwrap(),
        IssueStatus::Fixed
    );
}

#[test]
fn test_review_session_issues_accessor() {
    let raw = r#"{
        "id": "rev-test",
        "title": "Test Review",
        "created_at": 100,
        "items": [
            {
                "id": 1,
                "title": "Bug",
                "type": "bug",
                "status": "todo",
                "description": "Fix bug"
            }
        ]
    }"#;
    let session: ReviewSession = serde_json::from_str(raw).unwrap();
    assert_eq!(session.items.len(), 1);
    let issues = session.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].id, 1);
    assert_eq!(issues[0].issue_type, IssueType::Bug);
    assert_eq!(issues[0].status, IssueStatus::Todo);
    assert_eq!(issues[0].description, "Fix bug");
}

#[test]
fn test_review_session_stats_exclude_terminal_non_actionable_findings() {
    // Skipped and Rejected are terminal, non-actionable outcomes. A session
    // whose findings are all resolved that way must report a full progress
    // bar (`fixed == total`) rather than a fraction that can never reach one.
    let resolved = session_with(vec![
        finding(1, IssueStatus::Fixed),
        finding(2, IssueStatus::Skipped),
        finding(3, IssueStatus::Rejected),
    ]);
    assert_eq!(resolved.stats(), (1, 1));
    let summary = resolved.summary();
    assert_eq!(summary.total_items, 1);
    assert_eq!(summary.fixed_items, 1);

    // Any outstanding finding keeps the denominator above the fixed count, so
    // progress never overstates the remaining engineering work.
    let partial = session_with(vec![
        finding(1, IssueStatus::Fixed),
        finding(2, IssueStatus::Todo),
        finding(3, IssueStatus::InProgress),
        finding(4, IssueStatus::Discussion),
        finding(5, IssueStatus::Skipped),
        finding(6, IssueStatus::Rejected),
    ]);
    assert_eq!(partial.stats(), (4, 1));

    // A session with nothing left to act on is fully resolved (0/0) instead of
    // an unreachable fraction.
    let all_terminal = session_with(vec![
        finding(1, IssueStatus::Skipped),
        finding(2, IssueStatus::Rejected),
    ]);
    assert_eq!(all_terminal.stats(), (0, 0));
}

#[test]
fn test_review_session_preserves_every_issue_type() {
    let items = ALL_ISSUE_TYPES
        .iter()
        .enumerate()
        .map(|(index, issue_type)| categorized_finding(index as u32 + 1, *issue_type))
        .collect::<Vec<_>>();
    let session = session_with(items);

    // Reading a session must keep `Security` distinct from `Bug` and
    // `Performance` distinct from `Architecture`.
    let issues = session.issues();
    assert_eq!(issues.len(), ALL_ISSUE_TYPES.len());
    let read_back: Vec<IssueType> = issues.iter().map(|issue| issue.issue_type).collect();
    assert_eq!(read_back, ALL_ISSUE_TYPES.to_vec());

    // The category is the persisted classification; the derived severity is
    // never written, so a stored session cannot desynchronize the two.
    let serialized = serde_json::to_string(&session).unwrap();
    assert!(serialized.contains("\"type\":\"security\""));
    assert!(serialized.contains("\"type\":\"performance\""));
    assert!(!serialized.contains("\"severity\""));

    let restored: ReviewSession = serde_json::from_str(&serialized).unwrap();
    let restored_types: Vec<IssueType> = restored
        .issues()
        .iter()
        .map(|issue| issue.issue_type)
        .collect();
    assert_eq!(restored_types, ALL_ISSUE_TYPES.to_vec());
    assert_eq!(restored.items[0].severity, ReviewSeverity::Critical);
    assert_eq!(restored.items[3].severity, ReviewSeverity::Warning);
}

#[test]
fn test_review_item_accepts_legacy_category_spellings() {
    // Sessions written before the category was stored verbatim describe it with
    // a coarse severity, and older clients also used the `type` and
    // `issue_type` keys directly.
    let raw = r#"{
        "id": "rev-legacy-categories",
        "created_at": 1,
        "items": [
            {"id": 1, "title": "Critical", "severity": "critical", "description": "b1"},
            {"id": 2, "title": "Warning", "severity": "warning", "description": "b2"},
            {"id": 3, "title": "Suggestion", "severity": "suggestion", "description": "b3"},
            {"id": 4, "title": "Info", "severity": "info", "description": "b4"},
            {"id": 5, "title": "Typed", "type": "security", "description": "b5"},
            {"id": 6, "title": "Aliased", "issue_type": "performance", "description": "b6"},
            {"id": 7, "title": "Unclassified", "description": "b7"}
        ]
    }"#;

    let session: ReviewSession = serde_json::from_str(raw).unwrap();
    let issues = session.issues();
    let categories: Vec<IssueType> = issues.iter().map(|issue| issue.issue_type).collect();
    assert_eq!(
        categories,
        vec![
            IssueType::Bug,
            IssueType::Architecture,
            IssueType::Quality,
            IssueType::Quality,
            IssueType::Security,
            IssueType::Performance,
            IssueType::Bug,
        ]
    );

    // The canonical description key carries the finding body.
    assert_eq!(issues[5].description, "b6");
    assert_eq!(issues[6].description, "b7");

    // The presentation severity is derived from the category, so it can never
    // disagree with the stored classification.
    assert_eq!(session.items[0].severity, ReviewSeverity::Critical);
    assert_eq!(session.items[1].severity, ReviewSeverity::Warning);
    assert_eq!(session.items[2].severity, ReviewSeverity::Suggestion);
    assert_eq!(session.items[3].severity, ReviewSeverity::Suggestion);
    assert_eq!(session.items[4].severity, ReviewSeverity::Critical);
    assert_eq!(session.items[6].severity, ReviewSeverity::Critical);
}

#[test]
fn test_review_legacy_session_without_groups_or_post_analysis() {
    // A session written before grouping existed carries none of the new
    // fields and still reads, because every one of them defaults.
    let legacy = r#"{
        "id": "rev-legacy",
        "title": "Legacy",
        "created_at": 10,
        "user_prompt": "audit the engine",
        "target_files": ["crates/core/src/lib.rs"]
    }"#;
    let session: ReviewSession = serde_json::from_str(legacy).unwrap();
    assert!(session.groups.is_empty());
    assert!(session.post_analysis.is_none());
    assert!(session.launch_config.is_none());

    // Such a session is the degenerate case of a single audit: one implicit
    // Default group covering the session target files.
    let groups = session.effective_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].id, "default");
    assert_eq!(groups[0].title, "Default");
    assert_eq!(groups[0].paths, vec!["crates/core/src/lib.rs".to_string()]);
    assert!(groups[0].prompt.is_none());

    // None of the new fields is written back, so the record keeps its old
    // wire shape.
    let json = serde_json::to_string(&session).unwrap();
    assert!(!json.contains("\"groups\""), "got: {json}");
    assert!(!json.contains("post_analysis"), "got: {json}");
    assert!(!json.contains("launch_config"), "got: {json}");
}

#[test]
fn test_review_launch_snapshot_and_post_analysis_roundtrip() {
    let config = ReviewLaunchConfig {
        preset: Some("deep-audit".to_string()),
        mode: PostAnalysisMode::Automatic,
        groups: vec![
            ReviewGroup {
                id: "engine".to_string(),
                title: "Engine".to_string(),
                paths: vec!["crates/core/src".to_string()],
                prompt: Some("focus on the turn FSM".to_string()),
                ..Default::default()
            },
            ReviewGroup {
                id: "frontend".to_string(),
                title: "Frontend".to_string(),
                paths: vec!["crates/tui/src".to_string()],
                prompt: None,
                ..Default::default()
            },
        ],
    };
    let analysis = ReviewPostAnalysis {
        created_at: 42,
        summary: "One shared root cause".to_string(),
        independent_issues: vec!["0192f0aa-0000-7000-8000-000000000001".to_string()],
        architectural_groups: vec![ArchitecturalIssueGroup {
            id: "arch-1".to_string(),
            title: "Unbounded retry".to_string(),
            description: "Retries lack a shared budget".to_string(),
            issue_ids: vec![
                "0192f0aa-0000-7000-8000-000000000002".to_string(),
                "0192f0aa-0000-7000-8000-000000000003".to_string(),
            ],
            implementation: PlanImplementation {
                items: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Introduce one shared retry budget".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["crates/core/src/providers/retry.rs".to_string()],
                    ..Default::default()
                }],
                architect: Some(ArchitectTier::new(
                    ModelRef::openrouter("test/architect"),
                    Some(ReasoningEffort::High),
                )),
                last_replanned_step: Some("1.1".to_string()),
            },
        }],
    };

    let session = ReviewSession {
        id: "rev-grouped".to_string(),
        title: "Grouped".to_string(),
        created_at: 1,
        groups: config.groups.clone(),
        launch_config: Some(config.clone()),
        post_analysis: Some(analysis.clone()),
        ..Default::default()
    };

    let json = serde_json::to_string(&session).unwrap();
    let restored: ReviewSession = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.groups, config.groups);
    assert_eq!(restored.launch_config, Some(config));
    assert_eq!(restored.post_analysis, Some(analysis));

    // Explicit groups win over the implicit single-audit group.
    assert_eq!(restored.effective_groups(), restored.groups);
    // A freshly classified group carries no horizon yet, but the mandatory
    // nested implementation field is always part of the canonical wire form.
    let fresh = ArchitecturalIssueGroup {
        id: "arch-2".to_string(),
        title: "Fresh root cause".to_string(),
        description: String::new(),
        ..Default::default()
    };
    let fresh_json = serde_json::to_string(&fresh).unwrap();
    assert!(
        fresh_json.contains("\"implementation\":{}"),
        "got: {fresh_json}"
    );
    assert!(!fresh_json.contains("subplan"), "got: {fresh_json}");
}

#[test]
fn test_review_group_kind_is_a_stored_dichotomy() {
    // The class is stored instead of being derived from the group identifier,
    // so the wire form names it verbatim.
    assert_eq!(ReviewGroupKind::default(), ReviewGroupKind::Simple);
    assert_eq!(
        serde_json::to_string(&ReviewGroupKind::Simple).unwrap(),
        "\"simple\""
    );
    assert_eq!(
        serde_json::to_string(&ReviewGroupKind::Architectural).unwrap(),
        "\"architectural\""
    );
    assert_eq!(
        serde_json::from_str::<ReviewGroupKind>("\"architectural\"").unwrap(),
        ReviewGroupKind::Architectural
    );

    // A group written before the class field existed carries no `kind` and
    // keeps decoding as a simple group. The nested `implementation` stays
    // mandatory, so the record still names its horizon explicitly.
    let legacy = r#"{"id":"local","title":"Local Issues","implementation":{}}"#;
    let group: ReviewGroup = serde_json::from_str(legacy).unwrap();
    assert_eq!(group.kind, ReviewGroupKind::Simple);
    assert!(group.implementation.items.is_empty());

    let architectural = ReviewGroup {
        id: "arch-1".to_string(),
        title: "Unbounded retry".to_string(),
        kind: ReviewGroupKind::Architectural,
        ..Default::default()
    };
    let json = serde_json::to_string(&architectural).unwrap();
    assert!(json.contains("\"kind\":\"architectural\""), "got: {json}");
    let de: ReviewGroup = serde_json::from_str(&json).unwrap();
    assert_eq!(de.kind, ReviewGroupKind::Architectural);

    // The implicit group of a single-audit session is simple by construction.
    assert_eq!(
        ReviewGroup::single_default(Vec::new()).kind,
        ReviewGroupKind::Simple
    );
}
