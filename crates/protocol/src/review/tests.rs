use super::*;

use crate::model::{CancelMode, ModelRef, ModelSelection};

#[test]
fn test_review_execute_params_serde() {
    let single: ReviewExecuteParams =
        serde_json::from_str(r#"{"review_id":"rev-1","item_id":2}"#).unwrap();
    assert_eq!(single.review_id.as_deref(), Some("rev-1"));
    assert_eq!(single.item_id, Some(2));

    let cross_session: ReviewExecuteParams = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(cross_session.review_id, None);
    assert_eq!(cross_session.item_id, None);

    let json = serde_json::to_string(&cross_session).unwrap();
    assert_eq!(json, "{}");

    let single_session_only: ReviewExecuteParams =
        serde_json::from_str(r#"{"review_id":"rev-2"}"#).unwrap();
    assert_eq!(single_session_only.review_id.as_deref(), Some("rev-2"));
    assert_eq!(single_session_only.item_id, None);
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
fn test_review_status_lifecycle() {
    let status = ReviewItemStatus::Discussion;
    assert_eq!(status.next(), ReviewItemStatus::Todo);
    assert_eq!(status.next().next(), ReviewItemStatus::InProgress);
    assert_eq!(status.next().next().next(), ReviewItemStatus::Fixed);
    assert_eq!(status.next().next().next().next(), ReviewItemStatus::Skipped);
    assert_eq!(status.next().next().next().next().next(), ReviewItemStatus::Rejected);
    assert_eq!(status.next().next().next().next().next().next(), ReviewItemStatus::Discussion);
    assert_eq!(status.to_string(), "DISCUSSION");
    assert_eq!(ReviewItemStatus::Skipped.to_string(), "SKIPPED");
    assert_eq!(
        serde_json::to_string(&ReviewItemStatus::Skipped).unwrap(),
        "\"skipped\""
    );
    assert_eq!(
        serde_json::from_str::<ReviewItemStatus>("\"skipped\"").unwrap(),
        ReviewItemStatus::Skipped
    );
}

#[test]
fn test_review_item_serde() {
    let item = ReviewItem {
        id: 1,
        title: "Potential race condition".to_string(),
        model: Some(ModelSelection::Role(crate::model::ModelRole::Architect)),
        severity: ReviewSeverity::Critical,
        status: ReviewItemStatus::Discussion,
        files: vec!["crates/server/src/state.rs".to_string()],
        subplan: Vec::new(),
        resolution_notes: Some("Acquire lock across turn lifetime".to_string()),
        body: "Lock is released before turn concludes".to_string(),
    };

    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"critical\""));
    assert!(json.contains("\"discussion\""));
    assert!(json.contains("\"files\":[\"crates/server/src/state.rs\"]"));
    assert!(json.contains("\"resolution_notes\":\"Acquire lock across turn lifetime\""));

    let de: ReviewItem = serde_json::from_str(&json).unwrap();
    assert_eq!(de, item);

    let update_params = ReviewUpdateItemParams {
        review_id: Some("rev-1".to_string()),
        item_id: 1,
        status: Some(ReviewItemStatus::InProgress),
        resolution_notes: Some("Plan defined".to_string()),
    };
    let update_json = serde_json::to_string(&update_params).unwrap();
    assert!(update_json.contains("\"resolution_notes\":\"Plan defined\""));
    let de_update: ReviewUpdateItemParams = serde_json::from_str(&update_json).unwrap();
    assert_eq!(de_update, update_params);

    // The legacy "done" status alias still maps to Fixed.
    let legacy_json = r#"{"id":2,"title":"Legacy","severity":"info","status":"done","body":"","model":"architect"}"#;
    let de_legacy: ReviewItem = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(de_legacy.status, ReviewItemStatus::Fixed);
    assert_eq!(de_legacy.model, Some(ModelSelection::Role(crate::model::ModelRole::Architect)));
    assert!(de_legacy.files.is_empty());
}

#[test]
fn test_review_item_legacy_file_path_backward_compatibility() {
    let legacy_json = r#"{
        "id": 1,
        "title": "Legacy Finding",
        "severity": "warning",
        "status": "todo",
        "file_path": "crates/core/src/review.rs",
        "line_range": [10, 25],
        "body": "Legacy issue description"
    }"#;

    let item: ReviewItem = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(item.id, 1);
    assert_eq!(item.title, "Legacy Finding");
    assert_eq!(item.severity, ReviewSeverity::Warning);
    assert_eq!(item.status, ReviewItemStatus::Todo);
    assert_eq!(item.files, vec!["crates/core/src/review.rs"]);
    assert_eq!(item.body, "Legacy issue description");

    // Explicit empty files array falls back to file_path
    let empty_files_json = r#"{
        "id": 2,
        "title": "Empty files fallback",
        "severity": "info",
        "status": "discussion",
        "files": [],
        "file_path": "crates/tui/src/views.rs",
        "body": "Empty files details"
    }"#;
    let item_empty: ReviewItem = serde_json::from_str(empty_files_json).unwrap();
    assert_eq!(item_empty.files, vec!["crates/tui/src/views.rs"]);

    // Multi-file finding preserves all paths and Skipped status
    let multi_json = r#"{
        "id": 3,
        "title": "Multi-file Finding",
        "severity": "critical",
        "status": "skipped",
        "files": ["crates/protocol/src/review.rs", "crates/core/src/review.rs"],
        "resolution_notes": "Skipped because already fixed",
        "body": "Details"
    }"#;

    let multi_item: ReviewItem = serde_json::from_str(multi_json).unwrap();
    assert_eq!(multi_item.id, 3);
    assert_eq!(multi_item.status, ReviewItemStatus::Skipped);
    assert_eq!(
        multi_item.files,
        vec![
            "crates/protocol/src/review.rs",
            "crates/core/src/review.rs"
        ]
    );
    assert_eq!(
        multi_item.resolution_notes.as_deref(),
        Some("Skipped because already fixed")
    );
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
fn test_review_start_params_path_pattern_backward_compatible() {
    // A legacy client omits `path_pattern`; it stays absent and is never
    // serialized, preserving the old wire shape.
    let legacy: ReviewStartParams = serde_json::from_str(
        r#"{"model":{"provider":"openrouter","name":"openai/gpt-4o"}}"#,
    )
    .unwrap();
    assert!(legacy.path_pattern.is_none());
    assert_eq!(legacy.model, Some(ModelRef::openrouter("openai/gpt-4o")));

    let with_pattern = ReviewStartParams {
        path_pattern: Some("crates/core/src/**/*.rs".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_string(&with_pattern).unwrap();
    assert!(json.contains("\"path_pattern\":\"crates/core/src/**/*.rs\""));
    let de: ReviewStartParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de.path_pattern.as_deref(), Some("crates/core/src/**/*.rs"));

    assert_eq!(
        serde_json::to_string(&ReviewStartParams::default()).unwrap(),
        "{}"
    );
}

#[test]
fn test_review_item_without_subplan_stays_readable_and_has_none() {
    // A record predating the subplan field deserializes cleanly with an
    // empty subplan, keeping older sessions fully readable.
    let legacy = r#"{
        "id": 7,
        "title": "Legacy finding",
        "severity": "warning",
        "status": "todo",
        "files": ["crates/core/src/review.rs"],
        "body": "Legacy body"
    }"#;
    let item: ReviewItem = serde_json::from_str(legacy).unwrap();
    assert!(!item.has_subplan());
    assert_eq!(item.subplan_stats(), (0, 0));

    // The empty subplan is never serialized back, so the wire stays compact.
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("subplan"));
}

#[test]
fn test_review_item_subplan_roundtrip_and_stats() {
    let raw = r#"{
        "id": 8,
        "title": "Decomposed finding",
        "severity": "info",
        "status": "todo",
        "subplan": [
            {"id": "1.1", "title": "First", "status": "done"},
            {"id": "1.2", "title": "Second", "status": "in_progress"},
            {"id": "1.3", "title": "Dropped", "status": "cancelled"},
            {"id": "1.4", "title": "Group", "status": "todo", "children": [
                {"id": "1.4.1", "title": "Nested", "status": "done"}
            ]}
        ],
        "body": "Body"
    }"#;
    let item: ReviewItem = serde_json::from_str(raw).unwrap();
    assert!(item.has_subplan());
    // Leaves 1.1, 1.2, and 1.4.1 count; the cancelled 1.3 is excluded and
    // the container 1.4 is skipped in favor of its leaves.
    assert_eq!(item.subplan_stats(), (3, 2));

    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"subplan\""));
    let de: ReviewItem = serde_json::from_str(&json).unwrap();
    assert_eq!(de, item);
}

#[test]
fn test_review_refine_item_params_serde() {
    let params = ReviewRefineItemParams {
        review_id: Some("rev-1".to_string()),
        item_id: 3,
        model: Some(ModelSelection::Role(crate::model::ModelRole::Architect)),
        instructions: Some("split into two sub-steps".to_string()),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"item_id\":3"));
    assert!(json.contains("\"instructions\":\"split into two sub-steps\""));
    let de: ReviewRefineItemParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // Only `item_id` is mandatory; the remaining fields default to absent.
    let minimal: ReviewRefineItemParams = serde_json::from_str(r#"{"item_id":1}"#).unwrap();
    assert!(minimal.review_id.is_none());
    assert!(minimal.model.is_none());
    assert!(minimal.instructions.is_none());
}

#[test]
fn test_review_refine_item_result_serde_and_helpers() {
    let item = ReviewItem {
        id: 3,
        title: "Finding".to_string(),
        model: None,
        severity: ReviewSeverity::Info,
        status: ReviewItemStatus::Todo,
        files: Vec::new(),
        subplan: Vec::new(),
        resolution_notes: None,
        body: "Body".to_string(),
    };

    let success = ReviewRefineItemResult::Success {
        item: item.clone(),
        explanation: Some("Decomposed".to_string()),
    };
    assert!(success.is_success());
    assert!(!success.is_blocked());
    assert_eq!(success.item(), Some(&item));
    assert!(success.reason().is_none());
    let json = serde_json::to_string(&success).unwrap();
    assert!(json.contains("\"status\":\"success\""));
    let de: ReviewRefineItemResult = serde_json::from_str(&json).unwrap();
    assert_eq!(de, success);

    let blocked = ReviewRefineItemResult::Blocked {
        reason: "cannot decompose".to_string(),
    };
    assert!(blocked.is_blocked());
    assert!(!blocked.is_success());
    assert!(blocked.item().is_none());
    assert_eq!(blocked.reason(), Some("cannot decompose"));
    let json = serde_json::to_string(&blocked).unwrap();
    assert!(json.contains("\"status\":\"blocked\""));
    let de: ReviewRefineItemResult = serde_json::from_str(&json).unwrap();
    assert_eq!(de, blocked);
}

fn finding(id: u32, status: ReviewItemStatus) -> ReviewItem {
    ReviewItem {
        id,
        title: format!("Finding {id}"),
        model: None,
        severity: ReviewSeverity::Info,
        status,
        files: Vec::new(),
        subplan: Vec::new(),
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
        finding(1, ReviewItemStatus::Fixed),
        finding(2, ReviewItemStatus::Skipped),
        finding(3, ReviewItemStatus::Rejected),
    ]);
    assert_eq!(resolved.stats(), (1, 1));
    let summary = resolved.summary();
    assert_eq!(summary.total_items, 1);
    assert_eq!(summary.fixed_items, 1);

    // Any outstanding finding keeps the denominator above the fixed count, so
    // progress never overstates the remaining engineering work.
    let partial = session_with(vec![
        finding(1, ReviewItemStatus::Fixed),
        finding(2, ReviewItemStatus::Todo),
        finding(3, ReviewItemStatus::InProgress),
        finding(4, ReviewItemStatus::Discussion),
        finding(5, ReviewItemStatus::Skipped),
        finding(6, ReviewItemStatus::Rejected),
    ]);
    assert_eq!(partial.stats(), (4, 1));

    // A session with nothing left to act on is fully resolved (0/0) instead of
    // an unreachable fraction.
    let all_terminal = session_with(vec![
        finding(1, ReviewItemStatus::Skipped),
        finding(2, ReviewItemStatus::Rejected),
    ]);
    assert_eq!(all_terminal.stats(), (0, 0));
}
