use super::*;

use crate::model::{ArchitectTier, CancelMode, ModelRef, ModelSelection, ReasoningEffort};
use crate::plan::{PlanItem, PlanItemStatus};

#[test]
fn test_review_execute_params_serde() {
    let single: ReviewExecuteParams =
        serde_json::from_str(r#"{"review_id":"rev-1","item_id":2}"#).unwrap();
    assert_eq!(single.review_id.as_deref(), Some("rev-1"));
    assert_eq!(single.item_id, Some(2));
    assert_eq!(single.step_id, None);
    assert_eq!(single.model, None);
    assert_eq!(single.instructions, None);

    let cross_session: ReviewExecuteParams = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(cross_session.review_id, None);
    assert_eq!(cross_session.item_id, None);
    assert_eq!(cross_session.step_id, None);
    assert_eq!(cross_session.model, None);
    assert_eq!(cross_session.instructions, None);

    let json = serde_json::to_string(&cross_session).unwrap();
    assert_eq!(json, "{}");

    let single_session_only: ReviewExecuteParams =
        serde_json::from_str(r#"{"review_id":"rev-2"}"#).unwrap();
    assert_eq!(single_session_only.review_id.as_deref(), Some("rev-2"));
    assert_eq!(single_session_only.item_id, None);
    assert_eq!(single_session_only.step_id, None);
    assert_eq!(single_session_only.model, None);
    assert_eq!(single_session_only.instructions, None);

    let full = ReviewExecuteParams {
        review_id: Some("rev-3".to_string()),
        item_id: Some(5),
        step_id: Some("5.1".to_string()),
        group_id: None,
        item_ids: None,
        model: Some(ModelSelection::Role(crate::model::ModelRole::Coder)),
        instructions: Some("focus on unit tests".to_string()),
    };
    let full_json = serde_json::to_string(&full).unwrap();
    assert!(full_json.contains("\"step_id\":\"5.1\""));
    assert!(full_json.contains("\"model\":\"coder\""));
    assert!(full_json.contains("\"instructions\":\"focus on unit tests\""));
    let de_full: ReviewExecuteParams = serde_json::from_str(&full_json).unwrap();
    assert_eq!(de_full, full);

    // A section queue names its findings explicitly: the ids serialize
    // verbatim in client order, and an absent list keeps the whole-session
    // queue, so the wire form stays compact for every other caller.
    let section = ReviewExecuteParams {
        review_id: Some("rev-4".to_string()),
        item_ids: Some(vec![3, 1, 2]),
        ..Default::default()
    };
    let json = serde_json::to_string(&section).unwrap();
    assert!(json.contains("\"item_ids\":[3,1,2]"), "got: {json}");
    let de: ReviewExecuteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, section);
}

#[test]
fn test_review_cancel_params_serde() {
    // Legacy clients send an empty object; the mode stays absent and the
    // server falls back to an immediate abort.
    let legacy: ReviewCancelParams = serde_json::from_str("{}").unwrap();
    assert_eq!(legacy.mode, None);
    assert_eq!(serde_json::to_string(&legacy).unwrap(), "{}");

    let soft = ReviewCancelParams {
        mode: Some(CancelMode::Soft),
    };
    let json = serde_json::to_string(&soft).unwrap();
    assert_eq!(json, "{\"mode\":\"soft\"}");
    let de: ReviewCancelParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de.mode, Some(CancelMode::Soft));
}

#[test]
fn test_review_issue_serde() {
    let issue = ReviewIssue {
        id: 1,
        title: "Potential race condition".to_string(),
        uuid: None,
        group: None,
        model: None,
        issue_type: IssueType::Bug,
        status: IssueStatus::Discussion,
        description: "Lock is released before turn concludes".to_string(),
        recommendations: vec!["Acquire lock across turn lifetime".to_string()],
        files: vec!["crates/server/src/state.rs".to_string()],
        readonly_files: vec!["crates/protocol/src/review.rs".to_string()],
        note: Some("Lock design pending".to_string()),
    };

    let json = serde_json::to_string(&issue).unwrap();
    assert!(json.contains("\"type\":\"bug\""));
    assert!(json.contains("\"discussion\""));
    assert!(json.contains("\"files\":[\"crates/server/src/state.rs\"]"));
    assert!(json.contains("\"readonly_files\":[\"crates/protocol/src/review.rs\"]"));
    assert!(json.contains("\"note\":\"Lock design pending\""));

    let de: ReviewIssue = serde_json::from_str(&json).unwrap();
    assert_eq!(de, issue);

    let update_params = ReviewUpdateItemParams {
        review_id: Some("rev-1".to_string()),
        item_id: 1,
        status: Some(IssueStatus::InProgress),
        note: Some("Plan defined".to_string()),
    };
    let update_json = serde_json::to_string(&update_params).unwrap();
    assert!(update_json.contains("\"note\":\"Plan defined\""));
    let de_update: ReviewUpdateItemParams = serde_json::from_str(&update_json).unwrap();
    assert_eq!(de_update, update_params);

    // The renamed `note` field still accepts the legacy `resolution_notes`
    // key on read, so older clients keep updating findings unchanged.
    let legacy_update: ReviewUpdateItemParams =
        serde_json::from_str(r#"{"item_id":1,"resolution_notes":"legacy note"}"#).unwrap();
    assert_eq!(legacy_update.note.as_deref(), Some("legacy note"));

    // Legacy "severity", "body", and "done" aliases still map cleanly.
    let legacy_json = r#"{"id":2,"title":"Legacy","severity":"critical","status":"done","body":"Defect details","model":"architect"}"#;
    let de_legacy: ReviewIssue = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(de_legacy.issue_type, IssueType::Bug);
    assert_eq!(de_legacy.status, IssueStatus::Fixed);
    assert_eq!(de_legacy.description, "Defect details");
    assert_eq!(
        de_legacy.model,
        Some(ModelSelection::Role(crate::model::ModelRole::Architect))
    );
    assert!(de_legacy.files.is_empty());
}

#[test]
fn test_review_session_and_summary_model_ref_lenient_serde() {
    // 1. Structured ModelRef JSON
    let structured_session_json = r#"{
        "id": "rev-1",
        "title": "Structured",
        "created_at": 100,
        "model": {"provider": "openrouter", "name": "anthropic/claude-3.7-sonnet"}
    }"#;
    let session: ReviewSession = serde_json::from_str(structured_session_json).unwrap();
    assert_eq!(
        session.model,
        ModelRef::openrouter("anthropic/claude-3.7-sonnet")
    );

    let structured_summary_json = r#"{
        "id": "rev-1",
        "title": "Structured",
        "created_at": 100,
        "model": {"provider": "openrouter", "name": "anthropic/claude-3.7-sonnet"},
        "total_items": 5,
        "fixed_items": 2
    }"#;
    let summary: ReviewSummary = serde_json::from_str(structured_summary_json).unwrap();
    assert_eq!(
        summary.model,
        ModelRef::openrouter("anthropic/claude-3.7-sonnet")
    );

    // 2. Legacy string JSON (bare model name)
    let legacy_bare_session = r#"{
        "id": "rev-2",
        "title": "Legacy Bare",
        "created_at": 200,
        "model": "anthropic/claude-3.7-sonnet"
    }"#;
    let session: ReviewSession = serde_json::from_str(legacy_bare_session).unwrap();
    assert_eq!(
        session.model,
        ModelRef::openrouter("anthropic/claude-3.7-sonnet")
    );

    let legacy_bare_summary = r#"{
        "id": "rev-2",
        "title": "Legacy Bare",
        "created_at": 200,
        "model": "anthropic/claude-3.7-sonnet",
        "total_items": 3,
        "fixed_items": 1
    }"#;
    let summary: ReviewSummary = serde_json::from_str(legacy_bare_summary).unwrap();
    assert_eq!(
        summary.model,
        ModelRef::openrouter("anthropic/claude-3.7-sonnet")
    );

    // 3. Legacy string JSON with provider prefix
    let legacy_prefixed_session = r#"{
        "id": "rev-3",
        "title": "Legacy Prefixed",
        "created_at": 300,
        "model": "openrouter:openai/gpt-4o"
    }"#;
    let session: ReviewSession = serde_json::from_str(legacy_prefixed_session).unwrap();
    assert_eq!(session.model, ModelRef::openrouter("openai/gpt-4o"));

    // 4. Missing model falls back to default
    let missing_model_session = r#"{
        "id": "rev-4",
        "title": "Missing Model",
        "created_at": 400
    }"#;
    let session: ReviewSession = serde_json::from_str(missing_model_session).unwrap();
    assert_eq!(session.model, ModelRef::default());

    // 5. Summary method derives ModelRef
    let derived_summary = session.summary();
    assert_eq!(derived_summary.model, ModelRef::default());

    // 6. Serialized session roundtrips cleanly
    let serialized = serde_json::to_string(&session).unwrap();
    let de: ReviewSession = serde_json::from_str(&serialized).unwrap();
    assert_eq!(de.model, session.model);
}

#[test]
fn test_review_model_ref_lenient_is_total_and_keeps_precise_errors() {
    // A legacy string the strict parser now rejects must not make the
    // record unreadable: the raw value is preserved on read.
    let mistyped = r#"{
        "id": "rev-5",
        "title": "Mistyped legacy model",
        "created_at": 500,
        "model": "anthropc:claude"
    }"#;
    let session: ReviewSession = serde_json::from_str(mistyped).unwrap();
    assert_eq!(session.model, ModelRef::openrouter("anthropc:claude"));

    // An empty legacy string degrades to the neutral default reference.
    let empty = r#"{
        "id": "rev-6",
        "title": "Empty legacy model",
        "created_at": 600,
        "model": ""
    }"#;
    let session: ReviewSession = serde_json::from_str(empty).unwrap();
    assert_eq!(session.model, ModelRef::default());

    // An explicit `null` model degrades to the neutral default, exactly
    // like an absent field, so the record stays readable.
    let null_model = r#"{
        "id": "rev-null",
        "title": "Null legacy model",
        "created_at": 700,
        "model": null
    }"#;
    let session: ReviewSession = serde_json::from_str(null_model).unwrap();
    assert_eq!(session.model, ModelRef::default());

    // A structured object with an unknown provider surfaces the precise
    // provider diagnostic, not the opaque untagged-enum message.
    let bad_provider = r#"{
        "id": "rev-7",
        "title": "Unknown provider",
        "created_at": 700,
        "model": {"provider": "anthropic", "name": "claude"}
    }"#;
    let err = serde_json::from_str::<ReviewSession>(bad_provider)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown variant"), "got: {err}");
    assert!(!err.contains("did not match any variant"), "got: {err}");
}

#[test]
fn test_review_start_params_path_patterns_backward_compatible() {
    // A legacy client omits the target paths; the list stays empty and is
    // never serialized, preserving the old wire shape.
    let legacy: ReviewStartParams =
        serde_json::from_str(r#"{"model":{"provider":"openrouter","name":"openai/gpt-4o"}}"#)
            .unwrap();
    assert!(legacy.path_patterns.is_empty());
    // A legacy client that names only a model keeps decoding: the untagged
    // selection form maps the model reference to an explicit `Specific`
    // selection, so no reasoning depth is implied.
    assert_eq!(
        legacy.model,
        Some(ModelSelection::Specific(ModelRef::openrouter(
            "openai/gpt-4o"
        )))
    );

    // Coverage travels as one ordered list, so several audit lines reach the
    // server as a single typed field instead of competing scalar keys (c-1).
    let with_patterns = ReviewStartParams {
        path_patterns: vec![
            "crates/core/src/**/*.rs".to_string(),
            "crates/server/src/**/*.rs".to_string(),
        ],
        ..Default::default()
    };
    let json = serde_json::to_string(&with_patterns).unwrap();
    assert!(json.contains("\"path_patterns\""));
    assert!(json.contains("crates/core/src/**/*.rs"));
    assert!(json.contains("crates/server/src/**/*.rs"));
    let de: ReviewStartParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de.path_patterns, with_patterns.path_patterns);

    assert_eq!(
        serde_json::to_string(&ReviewStartParams::default()).unwrap(),
        "{}"
    );

    // A configured role rung travels as one value: the model together with the
    // reasoning depth of the entry the developer picked, so the server reads the
    // depth straight from the request instead of re-deriving it from the
    // configured reviewer entries.
    let tier = ReviewStartParams {
        model: Some(ModelSelection::Tier {
            model: ModelRef::openrouter("openai/gpt-4o"),
            effort: Some(ReasoningEffort::High),
        }),
        ..Default::default()
    };
    let json = serde_json::to_string(&tier).unwrap();
    assert!(json.contains("\"effort\":\"high\""), "got: {json}");
    assert!(json.contains("\"name\":\"openai/gpt-4o\""), "got: {json}");
    let de_tier: ReviewStartParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de_tier.model, tier.model);
}

#[test]
fn test_issue_type_methods_and_serde() {
    assert_eq!(IssueType::Bug.as_str(), "bug");
    assert_eq!(IssueType::Bug.display_badge(), "BUG");
    assert_eq!(IssueType::Bug.to_string(), "BUG");
    assert_eq!(serde_json::to_string(&IssueType::Bug).unwrap(), "\"bug\"");
    assert_eq!(
        serde_json::from_str::<IssueType>("\"bug\"").unwrap(),
        IssueType::Bug
    );
    assert_eq!(
        serde_json::from_str::<IssueType>("\"critical\"").unwrap(),
        IssueType::Bug
    );

    assert_eq!(IssueType::Security.as_str(), "security");
    assert_eq!(IssueType::Security.display_badge(), "SEC");

    assert_eq!(IssueType::Architecture.as_str(), "architecture");
    assert_eq!(IssueType::Architecture.display_badge(), "ARCH");
    assert_eq!(
        serde_json::from_str::<IssueType>("\"architecture\"").unwrap(),
        IssueType::Architecture
    );
    assert_eq!(
        serde_json::from_str::<IssueType>("\"warning\"").unwrap(),
        IssueType::Architecture
    );

    assert_eq!(IssueType::Performance.as_str(), "performance");
    assert_eq!(IssueType::Performance.display_badge(), "PERF");

    assert_eq!(IssueType::Quality.as_str(), "quality");
    assert_eq!(IssueType::Quality.display_badge(), "QUAL");
    assert_eq!(IssueType::Quality.to_string(), "QUAL");
    assert_eq!(
        serde_json::to_string(&IssueType::Quality).unwrap(),
        "\"quality\""
    );
    assert_eq!(
        serde_json::from_str::<IssueType>("\"quality\"").unwrap(),
        IssueType::Quality
    );
    assert_eq!(
        serde_json::from_str::<IssueType>("\"style\"").unwrap(),
        IssueType::Quality
    );
    assert_eq!(
        serde_json::from_str::<IssueType>("\"suggestion\"").unwrap(),
        IssueType::Quality
    );
}

#[test]
fn test_issue_status_lifecycle_and_legacy_refine_alias() {
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

    // A finding written before the status was retired keeps decoding: the
    // legacy `refine` spelling migrates to `todo` on read and is never written
    // back under its retired spelling.
    assert_eq!(
        serde_json::from_str::<IssueStatus>("\"refine\"").unwrap(),
        IssueStatus::Todo
    );
    assert_eq!(
        serde_json::to_string(&IssueStatus::Todo).unwrap(),
        "\"todo\""
    );
}

#[test]
fn test_review_issue_serde_carries_its_classification() {
    let issue = ReviewIssue {
        id: 42,
        title: "Memory leak in cache".to_string(),
        uuid: None,
        group: None,
        model: None,
        issue_type: IssueType::Bug,
        status: IssueStatus::InProgress,
        description: "Cache entries are never pruned".to_string(),
        recommendations: vec!["Implement LRU eviction".to_string()],
        files: vec!["crates/core/src/cache.rs".to_string()],
        readonly_files: vec!["crates/core/src/traits.rs".to_string()],
        note: Some("Under active review".to_string()),
    };

    let json = serde_json::to_string(&issue).unwrap();
    assert!(json.contains("\"type\":\"bug\""));
    assert!(json.contains("\"status\":\"in_progress\""));
    assert!(json.contains("\"description\":\"Cache entries are never pruned\""));
    assert!(json.contains("\"recommendations\":[\"Implement LRU eviction\"]"));
    assert!(json.contains("\"files\":[\"crates/core/src/cache.rs\"]"));
    assert!(json.contains("\"readonly_files\":[\"crates/core/src/traits.rs\"]"));
    assert!(json.contains("\"note\":\"Under active review\""));

    let de: ReviewIssue = serde_json::from_str(&json).unwrap();
    assert_eq!(de, issue);
}

#[test]
fn test_review_session_issues_accessor_and_alias() {
    let raw = r#"{
        "id": "rev-test",
        "title": "Test Review",
        "created_at": 100,
        "issues": [
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

fn finding(id: u32, status: IssueStatus) -> ReviewItem {
    ReviewItem {
        id,
        title: format!("Finding {id}"),
        uuid: None,
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

const ALL_ISSUE_TYPES: [IssueType; 5] = [
    IssueType::Bug,
    IssueType::Security,
    IssueType::Architecture,
    IssueType::Performance,
    IssueType::Quality,
];

fn categorized_finding(id: u32, issue_type: IssueType) -> ReviewItem {
    ReviewItem::from(ReviewIssue {
        id,
        title: format!("Finding {}", issue_type.as_str()),
        uuid: None,
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
            {"id": 1, "title": "Critical", "severity": "critical", "body": "b1"},
            {"id": 2, "title": "Warning", "severity": "warning", "body": "b2"},
            {"id": 3, "title": "Suggestion", "severity": "suggestion", "body": "b3"},
            {"id": 4, "title": "Info", "severity": "info", "body": "b4"},
            {"id": 5, "title": "Typed", "type": "security", "body": "b5"},
            {"id": 6, "title": "Aliased", "issue_type": "performance", "description": "b6"},
            {"id": 7, "title": "Unclassified", "body": "b7"}
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

    // The description alias still feeds the finding body.
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
fn test_review_post_analysis_mode_defaults_to_none() {
    // A session that never asked for a classification keeps the flat list.
    assert_eq!(PostAnalysisMode::default(), PostAnalysisMode::None);
    assert_eq!(
        serde_json::to_string(&PostAnalysisMode::Automatic).unwrap(),
        "\"automatic\""
    );
    assert_eq!(
        serde_json::to_string(&PostAnalysisMode::Manual).unwrap(),
        "\"manual\""
    );
    assert_eq!(
        serde_json::to_string(&PostAnalysisMode::None).unwrap(),
        "\"none\""
    );
    assert_eq!(
        serde_json::from_str::<PostAnalysisMode>("\"automatic\"").unwrap(),
        PostAnalysisMode::Automatic
    );
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
            subplan: vec![PlanItem {
                id: "1.1".to_string(),
                title: "Introduce one shared retry budget".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["crates/core/src/providers/retry.rs".to_string()],
                ..Default::default()
            }],
            subplan_architect: Some(ArchitectTier::new(
                ModelRef::openrouter("test/architect"),
                Some(ReasoningEffort::High),
            )),
            subplan_last_replanned_step: Some("1.1".to_string()),
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
    // A freshly classified group carries no horizon, so the new fields stay
    // absent from the wire form instead of being written as empty values.
    let fresh = ArchitecturalIssueGroup {
        id: "arch-2".to_string(),
        title: "Fresh root cause".to_string(),
        description: String::new(),
        ..Default::default()
    };
    let fresh_json = serde_json::to_string(&fresh).unwrap();
    assert!(!fresh_json.contains("subplan"), "got: {fresh_json}");
}

#[test]
fn test_review_item_uuid_and_group_survive_issue_roundtrip() {
    let mut item = finding(1, IssueStatus::Todo);
    item.uuid = Some("0192f0aa-0000-7000-8000-00000000000a".to_string());
    item.group = Some("engine".to_string());

    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"uuid\":\"0192f0aa-0000-7000-8000-00000000000a\""));
    assert!(json.contains("\"group\":\"engine\""));
    let restored: ReviewItem = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.uuid, item.uuid);
    assert_eq!(restored.group, item.group);

    // The analytical view carries both identifiers, so a finding rebuilt from
    // it keeps the identity the post-analysis layer refers to.
    let issue = ReviewIssue::from(item.clone());
    assert_eq!(issue.uuid, item.uuid);
    assert_eq!(issue.group, item.group);
    let rebuilt = ReviewItem::from(issue);
    assert_eq!(rebuilt.uuid, item.uuid);
    assert_eq!(rebuilt.group, item.group);

    // A finding written before the identifiers existed stays readable and
    // keeps them absent from the wire form.
    let legacy = r#"{"id": 9, "title": "Legacy", "type": "bug", "status": "todo"}"#;
    let legacy_item: ReviewItem = serde_json::from_str(legacy).unwrap();
    assert!(legacy_item.uuid.is_none());
    assert!(legacy_item.group.is_none());
    let legacy_json = serde_json::to_string(&legacy_item).unwrap();
    assert!(!legacy_json.contains("uuid"), "got: {legacy_json}");
    assert!(!legacy_json.contains("\"group\""), "got: {legacy_json}");
}

#[test]
fn test_review_start_params_preset_backward_compatible() {
    // A legacy client omits `preset`: it stays absent and is never serialized,
    // so a single-audit or one-off custom launch keeps its old wire shape.
    let legacy: ReviewStartParams = serde_json::from_str("{}").unwrap();
    assert!(legacy.preset.is_none());
    assert_eq!(serde_json::to_string(&legacy).unwrap(), "{}");

    let preset = ReviewStartParams {
        preset: Some("deep-audit".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_string(&preset).unwrap();
    assert!(json.contains("\"preset\":\"deep-audit\""), "got: {json}");
    let de: ReviewStartParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de.preset.as_deref(), Some("deep-audit"));
}

#[test]
fn test_review_analyze_params_and_result_serde() {
    let params = ReviewAnalyzeParams {
        review_id: "rev-1".to_string(),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert_eq!(json, "{\"review_id\":\"rev-1\"}");
    let de: ReviewAnalyzeParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // The session identifier is mandatory: post-analysis classifies exactly one
    // stored session and never guesses which one the caller meant.
    let err = serde_json::from_str::<ReviewAnalyzeParams>("{}")
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing field"), "got: {err}");

    let session = ReviewSession {
        id: "rev-1".to_string(),
        title: "Audit".to_string(),
        created_at: 7,
        ..Default::default()
    };
    let result = ReviewAnalyzeResult {
        session: session.clone(),
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("\"session\":{"), "got: {json}");
    let de: ReviewAnalyzeResult = serde_json::from_str(&json).unwrap();
    assert_eq!(de.session, session);
}

#[test]
fn test_review_group_events_roundtrip() {
    let started = ReviewGroupStartedEvent {
        operation_id: "op-1".to_string(),
        group_id: "engine".to_string(),
        group_title: "Engine".to_string(),
        files_count: 12,
        estimated_tokens: 4096,
    };
    let json = serde_json::to_string(&started).unwrap();
    assert!(json.contains("\"group_id\":\"engine\""), "got: {json}");
    let de: ReviewGroupStartedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(de.operation_id, started.operation_id);
    assert_eq!(de.group_title, "Engine");
    assert_eq!(de.files_count, 12);
    assert_eq!(de.estimated_tokens, 4096);

    // A successful group carries no reason and stays compact on the wire.
    let finished = ReviewGroupFinishedEvent {
        operation_id: "op-1".to_string(),
        group_id: "engine".to_string(),
        findings_count: 3,
        success: true,
        error: None,
    };
    let json = serde_json::to_string(&finished).unwrap();
    assert!(!json.contains("error"), "got: {json}");
    let de: ReviewGroupFinishedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(de.findings_count, 3);
    assert!(de.success);
    assert!(de.error.is_none());

    let legacy = r#"{"operation_id":"op-1","group_id":"engine","findings_count":1,"success":true}"#;
    let de: ReviewGroupFinishedEvent = serde_json::from_str(legacy).unwrap();
    assert!(de.error.is_none());

    // A failed group reports its reason locally: the findings the other groups
    // already produced stay intact.
    let failed = ReviewGroupFinishedEvent {
        operation_id: "op-1".to_string(),
        group_id: "frontend".to_string(),
        findings_count: 0,
        success: false,
        error: Some("provider timed out".to_string()),
    };
    let json = serde_json::to_string(&failed).unwrap();
    assert!(json.contains("\"success\":false"), "got: {json}");
    let de: ReviewGroupFinishedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(de.error.as_deref(), Some("provider timed out"));
}

#[test]
fn test_review_analyze_events_roundtrip() {
    let started = ReviewAnalyzeStartedEvent {
        operation_id: "op-2".to_string(),
        model: ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
        findings_count: 5,
    };
    let json = serde_json::to_string(&started).unwrap();
    assert!(json.contains("\"findings_count\":5"), "got: {json}");
    let de: ReviewAnalyzeStartedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(de.model, started.model);
    assert_eq!(de.findings_count, 5);

    // A legacy string model reference still decodes, exactly like every other
    // review record carrying a model.
    let legacy = r#"{"operation_id":"op-2","model":"openai/gpt-4o","findings_count":2}"#;
    let de: ReviewAnalyzeStartedEvent = serde_json::from_str(legacy).unwrap();
    assert_eq!(de.model, ModelRef::openrouter("openai/gpt-4o"));

    // The finished event carries the session whose post-analysis layer was
    // replaced, so the client renders the new classification without a refetch.
    let session = ReviewSession {
        id: "rev-1".to_string(),
        created_at: 9,
        ..Default::default()
    };
    let finished = ReviewAnalyzeFinishedEvent {
        operation_id: "op-2".to_string(),
        session: session.clone(),
    };
    let json = serde_json::to_string(&finished).unwrap();
    let de: ReviewAnalyzeFinishedEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(de.operation_id, "op-2");
    assert_eq!(de.session, session);
}

#[test]
fn test_review_preview_window_reports_truncation_only_from_the_token_budget() {
    // A window that drops nothing is intact, so the client skips the consent
    // step and audits exactly what the developer selected.
    let intact = ReviewPreviewWindow {
        matched_files: 3,
        selected_files: 3,
        selected_tokens: 120,
        files_token_budget: 10_000,
        ..Default::default()
    };
    assert!(!intact.is_truncated());

    // The budget is the only knife: the files it stops are reported by the
    // aggregate and by the budget breakdown, so the two counters agree.
    let truncated = ReviewPreviewWindow {
        matched_files: 3,
        selected_files: 1,
        selected_tokens: 120,
        omitted_files: 2,
        omitted_by_budget: 2,
        files_token_budget: 120,
        ..Default::default()
    };
    assert!(truncated.is_truncated());
    assert_eq!(truncated.omitted_files, truncated.omitted_by_budget);

    // The verdict is read from the budget breakdown alone, so no count of files
    // can become a second truncation cause.
    let budget_only = ReviewPreviewWindow {
        omitted_by_budget: 1,
        ..Default::default()
    };
    assert!(budget_only.is_truncated());
}

#[test]
fn test_review_preview_window_decodes_without_a_batch_counter() {
    // Counts of files are never limits, so the wire contract carries no batch
    // counter. A peer that still names the retired key decodes unchanged: the
    // unknown field is ignored and the verdict stays with the budget.
    let legacy = r#"{
        "matched_files": 4,
        "selected_files": 2,
        "selected_tokens": 900,
        "omitted_files": 2,
        "omitted_by_budget": 2,
        "omitted_by_batch": 2,
        "files_token_budget": 900
    }"#;
    let window: ReviewPreviewWindow = serde_json::from_str(legacy).unwrap();
    assert_eq!(window.selected_files, 2);
    assert_eq!(window.omitted_by_budget, 2);
    assert!(window.is_truncated());

    // Every counter defaults, so a minimal payload keeps decoding.
    let minimal: ReviewPreviewWindow = serde_json::from_str("{}").unwrap();
    assert_eq!(minimal.omitted_by_budget, 0);
    assert!(!minimal.is_truncated());

    // The window writes the counter a client reads, and the retired key never
    // returns to the wire.
    let json = serde_json::to_string(&minimal).unwrap();
    assert!(json.contains("\"omitted_by_budget\":0"), "got: {json}");
    assert!(!json.contains("omitted_by_batch"), "got: {json}");
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

    // A group written before the field existed carries no class and keeps
    // decoding as a simple group.
    let legacy = r#"{"id":"local","title":"Local Issues"}"#;
    let group: ReviewGroup = serde_json::from_str(legacy).unwrap();
    assert_eq!(group.kind, ReviewGroupKind::Simple);

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

#[test]
fn test_review_execute_params_group_target() {
    // A group run names the group itself: `step_id` then addresses a step of
    // the group subplan, and the finding targets stay absent.
    let group_run = ReviewExecuteParams {
        review_id: Some("rev-1".to_string()),
        group_id: Some("arch-1".to_string()),
        step_id: Some("1.1".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_string(&group_run).unwrap();
    assert!(json.contains("\"group_id\":\"arch-1\""), "got: {json}");
    let de: ReviewExecuteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, group_run);
    assert!(de.item_id.is_none());
    assert!(de.item_ids.is_none());

    // A finding run keeps the old wire shape: the group target stays absent.
    let finding_run: ReviewExecuteParams =
        serde_json::from_str(r#"{"review_id":"rev-1","item_id":2}"#).unwrap();
    assert!(finding_run.group_id.is_none());
    let json = serde_json::to_string(&finding_run).unwrap();
    assert!(!json.contains("group_id"), "got: {json}");
}
