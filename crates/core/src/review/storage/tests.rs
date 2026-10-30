use super::*;
use std::path::Path;
use tauqe_protocol::{
    ArchitectTier, IssueStatus, IssueType, PlanItem, ReviewGroup, ReviewGroupKind, ReviewIssue,
    ReviewItem, ReviewSession, ReviewSeverity, DEFAULT_REVIEW_GROUP_ID,
};
use tempfile::tempdir;

/// Canonical finding identifier exercised by the identity assertions.
const FINDING_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";

/// Architect provenance recorded by the synthesis primitive under test.
fn test_architect() -> ArchitectTier {
    ArchitectTier::new(tauqe_protocol::ModelRef::openrouter("test/architect"), None)
}

#[test]
fn test_review_storage_crud_and_active() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "rev-sec-1".to_string(),
        title: "Security Audit".to_string(),
        created_at: 1000,
        model: tauqe_protocol::ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
        description: Some("Initial security scan".to_string()),
        user_prompt: Some("focus on security".to_string()),
        target_files: vec!["crates/core/src/lib.rs".to_string()],
        items: vec![ReviewItem {
            id: 1,
            title: "Security finding".to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: IssueType::Security,
            severity: ReviewSeverity::Critical,
            status: IssueStatus::Discussion,
            recommendations: vec![],
            files: vec!["crates/core/src/lib.rs".to_string()],
            readonly_files: vec![],
            resolution_notes: None,
            body: "Check input validation".to_string(),
        }],
        raw_markdown: "## [CRITICAL] Security finding\n...".to_string(),
        ..Default::default()
    };

    // 1. Save
    ReviewStorage::save_session(root, &session).unwrap();

    // 2. List
    let list = ReviewStorage::list_reviews(root).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rev-sec-1");

    // 3. Load by ID and Load Latest
    let loaded = ReviewStorage::load_review(root, "rev-sec-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.title, "Security Audit");
    // Persisting and reloading a finding keeps its exact category instead
    // of collapsing it into the coarse severity class.
    assert_eq!(loaded.items[0].issue_type, IssueType::Security);
    assert_eq!(loaded.issues()[0].issue_type, IssueType::Security);

    let latest = ReviewStorage::load_latest(root).unwrap().unwrap();
    assert_eq!(latest.id, "rev-sec-1");

    // 4. Update item status to Fixed
    let updated =
        ReviewStorage::update_session_item_status(root, "rev-sec-1", 1, IssueStatus::Fixed)
            .unwrap()
            .unwrap();
    assert_eq!(updated.status, IssueStatus::Fixed);

    // 5. Active marker
    assert_eq!(
        ReviewStorage::load_active_id(root).unwrap(),
        Some("rev-sec-1".to_string())
    );
    ReviewStorage::save_active_id(root, None).unwrap();
    assert_eq!(ReviewStorage::load_active_id(root).unwrap(), None);

    // 6. Delete
    let deleted = ReviewStorage::delete_review(root, "rev-sec-1").unwrap();
    assert!(deleted);
    assert_eq!(ReviewStorage::list_reviews(root).unwrap().len(), 0);
}

#[test]
fn test_legacy_migration() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let reviews_dir = ReviewStorage::reviews_dir(root);
    std::fs::create_dir_all(&reviews_dir).unwrap();

    let legacy_file = reviews_dir.join("latest.json");
    let legacy_raw_json = r#"{
            "id": "legacy-session-9",
            "title": "Legacy Audit",
            "created_at": 12345,
            "model": "test-model",
            "target_files": [],
            "items": [],
            "raw_markdown": ""
        }"#;
    std::fs::write(&legacy_file, legacy_raw_json).unwrap();

    // Migration is a startup-only operation: it no longer runs on reads.
    ReviewStorage::migrate_legacy_if_needed(root);

    let loaded = ReviewStorage::load_latest(root).unwrap().unwrap();
    assert_eq!(loaded.id, "legacy-session-9");
    assert!(!legacy_file.is_file());
    assert!(reviews_dir.join("legacy-session-9.json").is_file());
}

#[test]
fn test_concurrent_review_updates_are_serialized_without_lost_writes() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let items: Vec<ReviewItem> = (0..16)
        .map(|i| ReviewItem {
            id: i,
            title: format!("Item {i}"),
            uuid: None,
            group: None,
            model: None,
            issue_type: tauqe_protocol::IssueType::Quality,
            severity: tauqe_protocol::ReviewSeverity::Suggestion,
            status: IssueStatus::Todo,
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            resolution_notes: None,
            body: "Check".to_string(),
        })
        .collect();

    let session = ReviewSession {
        id: "concurrent-review".to_string(),
        title: "Concurrent Review".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items,
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(&root, &session).unwrap();

    let handles: Vec<_> = (0..16)
        .map(|i| {
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = ReviewStorage::update_session_item_status(
                    &root,
                    "concurrent-review",
                    i,
                    IssueStatus::Fixed,
                );
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    let loaded = ReviewStorage::load_review(&root, "concurrent-review")
        .unwrap()
        .unwrap();
    let fixed = loaded
        .items
        .iter()
        .filter(|item| item.status == IssueStatus::Fixed)
        .count();
    assert_eq!(fixed, 16);
}

#[test]
fn test_load_all_skips_corrupted_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "valid-review".to_string(),
        title: "Valid Review".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let corrupt_path = ReviewStorage::reviews_dir(root).join("broken.json");
    std::fs::write(&corrupt_path, b"{ not a valid review session }").unwrap();

    let all = ReviewStorage::load_all(root).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "valid-review");
}

#[test]
fn test_save_session_reuses_differing_filename_instead_of_duplicating() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let first = ReviewSession {
        id: "Review One".to_string(),
        title: "First".to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &first).unwrap();
    assert!(ReviewStorage::reviews_dir(root)
        .join("Review-One.json")
        .is_file());

    // A casing change yields a different sanitized file name; the existing file
    // must be reused instead of writing a second `review-one.json`.
    let second = ReviewSession {
        id: "review one".to_string(),
        title: "Second".to_string(),
        created_at: 2,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &second).unwrap();

    let json_files: Vec<_> = std::fs::read_dir(ReviewStorage::reviews_dir(root))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    assert_eq!(
        json_files.len(),
        1,
        "expected a single review file, found {json_files:?}"
    );

    let loaded = ReviewStorage::load_review(root, "review one")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.created_at, 2);
}

#[test]
fn test_delete_review_resolves_differing_filename() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "Legacy Review".to_string(),
        title: "Legacy".to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();
    let stored = ReviewStorage::reviews_dir(root).join("Legacy-Review.json");
    assert!(stored.is_file());

    let deleted = ReviewStorage::delete_review(root, "legacy review").unwrap();
    assert!(deleted);
    assert!(!stored.is_file());
}

#[test]
fn test_save_session_detects_sanitized_id_collision() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let first = ReviewSession {
        id: "team/auth".to_string(),
        title: "Team Auth".to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &first).unwrap();

    // "team-auth" sanitizes to the same storage name as "team/auth".
    let colliding = ReviewSession {
        id: "team-auth".to_string(),
        title: "Team Auth Alternate".to_string(),
        created_at: 2,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    let err = ReviewStorage::save_session(root, &colliding).unwrap_err();
    assert!(err
        .to_string()
        .contains("already used by review session 'team/auth'"));

    // The original session is preserved unchanged.
    let loaded = ReviewStorage::load_review(root, "team/auth")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.id, "team/auth");
    assert_eq!(loaded.title, "Team Auth");
}

#[test]
fn test_load_and_delete_respect_sanitized_id_collision() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "team/auth".to_string(),
        title: "Team Auth".to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    // Loading a non-existent colliding ID must return None rather than the colliding session.
    let result = ReviewStorage::load_review(root, "team-auth").unwrap();
    assert!(result.is_none());

    // Deleting a non-existent colliding ID must return false and not delete the existing session.
    let deleted = ReviewStorage::delete_review(root, "team-auth").unwrap();
    assert!(!deleted);

    let still_exists = ReviewStorage::load_review(root, "team/auth").unwrap();
    assert!(still_exists.is_some());
}

#[test]
fn test_update_session_item_targets_only_the_named_session() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let make_session = |id: &str| ReviewSession {
        id: id.to_string(),
        title: id.to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![ReviewItem {
            id: 1,
            title: "Finding".to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: IssueType::Quality,
            severity: ReviewSeverity::Suggestion,
            status: IssueStatus::Todo,
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            resolution_notes: None,
            body: String::new(),
        }],
        raw_markdown: String::new(),
        ..Default::default()
    };

    ReviewStorage::save_session(root, &make_session("rev-a")).unwrap();
    ReviewStorage::save_session(root, &make_session("rev-b")).unwrap();
    // The active marker points at `rev-a`, but it is a UI pointer only and
    // must never steer a mutation.
    ReviewStorage::save_active_id(root, Some("rev-a")).unwrap();

    let updated = ReviewStorage::update_session_item_status(root, "rev-b", 1, IssueStatus::Fixed)
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, IssueStatus::Fixed);

    let a = ReviewStorage::load_review(root, "rev-a").unwrap().unwrap();
    assert_eq!(a.find_item(1).unwrap().status, IssueStatus::Todo);
    let b = ReviewStorage::load_review(root, "rev-b").unwrap().unwrap();
    assert_eq!(b.find_item(1).unwrap().status, IssueStatus::Fixed);

    // A non-existent id resolves to nothing: there is no implicit fallback
    // to the active or latest session.
    let missing =
        ReviewStorage::update_session_item_status(root, "rev-missing", 1, IssueStatus::Fixed)
            .unwrap();
    assert!(missing.is_none());
}

#[test]
fn test_atomic_writes_leave_no_temp_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "atomic-review".to_string(),
        title: "Atomic".to_string(),
        created_at: 1,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();
    ReviewStorage::save_active_id(root, Some("atomic-review")).unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(ReviewStorage::reviews_dir(root))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");

    assert_eq!(
        ReviewStorage::load_active_id(root).unwrap(),
        Some("atomic-review".to_string())
    );
}

// Review storage performs blocking file I/O and waits on a synchronous mutex.
// Driving the full lifecycle from an async task on a multi-threaded runtime
// must neither panic nor stall the executor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_review_storage_lifecycle_on_multi_thread_runtime() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let session = ReviewSession {
        id: "runtime-review".to_string(),
        title: "Runtime Review".to_string(),
        created_at: 500,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![ReviewItem {
            id: 1,
            title: "Finding".to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::Warning,
            status: IssueStatus::Discussion,
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            resolution_notes: None,
            body: "Body".to_string(),
        }],
        raw_markdown: String::new(),
        ..Default::default()
    };

    ReviewStorage::save_session(&root, &session).unwrap();
    assert!(ReviewStorage::reviews_dir(&root)
        .join("runtime-review.json")
        .is_file());
    assert_eq!(
        ReviewStorage::load_active_id(&root).unwrap(),
        Some("runtime-review".to_string())
    );

    let loaded = ReviewStorage::load_review(&root, "runtime-review")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.items.len(), 1);
    assert_eq!(loaded.items[0].status, IssueStatus::Discussion);

    let updated =
        ReviewStorage::update_session_item_status(&root, "runtime-review", 1, IssueStatus::Todo)
            .unwrap()
            .unwrap();
    assert_eq!(updated.status, IssueStatus::Todo);

    assert_eq!(ReviewStorage::load_all(&root).unwrap().len(), 1);
    assert_eq!(
        ReviewStorage::load_latest(&root).unwrap().unwrap().id,
        "runtime-review"
    );

    ReviewStorage::save_active_id(&root, None).unwrap();
    assert_eq!(ReviewStorage::load_active_id(&root).unwrap(), None);

    assert!(ReviewStorage::delete_review(&root, "runtime-review").unwrap());
    assert!(ReviewStorage::load_all(&root).unwrap().is_empty());
}

#[test]
fn test_review_storage_extended_fields_persistence() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "rev-ext-1".to_string(),
        title: "Extended Fields Audit".to_string(),
        created_at: 2000,
        model: tauqe_protocol::ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
        description: Some("Persistence check".to_string()),
        user_prompt: None,
        target_files: vec!["crates/core/src/lib.rs".to_string()],
        items: vec![ReviewItem {
            id: 1,
            title: "Finding with extended fields".to_string(),
            uuid: Some(FINDING_UUID.to_string()),
            group: Some("core".to_string()),
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::Warning,
            status: IssueStatus::Discussion,
            recommendations: vec!["Step 1: refactor lock".to_string()],
            files: vec!["crates/core/src/state.rs".to_string()],
            readonly_files: vec!["crates/protocol/src/lib.rs".to_string()],
            resolution_notes: None,
            body: "Root cause analysis".to_string(),
        }],
        raw_markdown: String::new(),
        ..Default::default()
    };

    ReviewStorage::save_session(root, &session).unwrap();

    let updated = ReviewStorage::update_session_item(root, "rev-ext-1", 1, |issue| {
        issue.status = IssueStatus::Fixed;
        issue
            .recommendations
            .push("Step 2: add regression test".to_string());
        issue
            .readonly_files
            .push("crates/core/src/config.rs".to_string());
        issue.note = Some("Resolved in PR #42".to_string());
    })
    .unwrap()
    .unwrap();

    assert_eq!(updated.status, IssueStatus::Fixed);
    assert_eq!(
        updated.recommendations,
        vec!["Step 1: refactor lock", "Step 2: add regression test"]
    );
    assert_eq!(
        updated.readonly_files,
        vec!["crates/protocol/src/lib.rs", "crates/core/src/config.rs"]
    );
    assert_eq!(updated.note.as_deref(), Some("Resolved in PR #42"));

    let loaded = ReviewStorage::load_review(root, "rev-ext-1")
        .unwrap()
        .unwrap();
    let issues = loaded.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].status, IssueStatus::Fixed);
    assert_eq!(
        issues[0].recommendations,
        vec!["Step 1: refactor lock", "Step 2: add regression test"]
    );
    assert_eq!(
        issues[0].readonly_files,
        vec!["crates/protocol/src/lib.rs", "crates/core/src/config.rs"]
    );
    assert_eq!(issues[0].note.as_deref(), Some("Resolved in PR #42"));
    // The finding identity and the group that produced it are the
    // references the post-analysis layer and the grouped view rely on, so
    // the issue round-trip must carry both unchanged.
    assert_eq!(updated.uuid.as_deref(), Some(FINDING_UUID));
    assert_eq!(updated.group.as_deref(), Some("core"));
    assert_eq!(loaded.items[0].uuid.as_deref(), Some(FINDING_UUID));
    assert_eq!(loaded.items[0].group.as_deref(), Some("core"));
}

/// A finding update round-trips through `ReviewIssue` and back into
/// `ReviewItem`. The semantic category is the single source of truth and
/// must survive that trip verbatim: the pairs the coarse severity
/// projection merges (`Bug`/`Security`, `Architecture`/`Performance`) must
/// never collapse into one another, and the derived severity must be
/// recomputed from the stored category on the next read.
#[test]
fn test_update_session_item_preserves_the_exact_issue_category() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let make_item = |id: u32, issue_type: IssueType, severity: ReviewSeverity| ReviewItem {
        id,
        title: format!("Finding {id}"),
        uuid: None,
        group: None,
        model: None,
        issue_type,
        severity,
        status: IssueStatus::Todo,
        recommendations: vec![],
        files: vec![],
        readonly_files: vec![],
        resolution_notes: None,
        body: String::new(),
    };

    let session = ReviewSession {
        id: "rev-category-fidelity".to_string(),
        title: "Category Fidelity".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec![],
        items: vec![
            make_item(1, IssueType::Security, ReviewSeverity::Critical),
            make_item(2, IssueType::Performance, ReviewSeverity::Warning),
        ],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let updated = ReviewStorage::update_session_item(root, "rev-category-fidelity", 1, |issue| {
        issue.status = IssueStatus::Fixed;
    })
    .unwrap()
    .unwrap();
    assert_eq!(updated.issue_type, IssueType::Security);

    let loaded = ReviewStorage::load_review(root, "rev-category-fidelity")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.find_item(1).unwrap().issue_type, IssueType::Security);
    assert_eq!(
        loaded.find_item(2).unwrap().issue_type,
        IssueType::Performance
    );
    // The presentation severity is derived from the stored category.
    assert_eq!(
        loaded.find_item(1).unwrap().severity,
        ReviewSeverity::Critical
    );
    assert_eq!(
        loaded.find_item(2).unwrap().severity,
        ReviewSeverity::Warning
    );

    let issues = loaded.issues();
    assert_eq!(issues[0].issue_type, IssueType::Security);
    assert_eq!(issues[1].issue_type, IssueType::Performance);
}
/// Re-running the post-analysis replaces the whole grouping of the session
/// while the findings it references stay untouched.
#[test]
fn test_regroup_session_replaces_groups_and_keeps_findings() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "rev-post-analysis".to_string(),
        title: "Post Analysis".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        items: vec![ReviewItem {
            id: 1,
            title: "Finding".to_string(),
            uuid: Some(FINDING_UUID.to_string()),
            group: Some("core".to_string()),
            model: None,
            issue_type: IssueType::Quality,
            severity: ReviewSeverity::Suggestion,
            status: IssueStatus::Discussion,
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            resolution_notes: None,
            body: "Body".to_string(),
        }],
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    // The classification is the session itself: the first pass introduces
    // the group list every finding is assigned to.
    let local_only = vec![ReviewGroup {
        id: "local".to_string(),
        title: "Local Issues".to_string(),
        ..Default::default()
    }];
    let first = ReviewStorage::regroup_session(
        root,
        "rev-post-analysis",
        "first pass".to_string(),
        local_only.clone(),
        vec![(FINDING_UUID.to_string(), "local".to_string())],
    )
    .unwrap()
    .unwrap();
    assert_eq!(first.description.as_deref(), Some("first pass"));
    assert_eq!(first.groups, local_only);
    assert_eq!(first.items[0].group.as_deref(), Some("local"));

    // A second pass substitutes the grouping wholesale: no part of the
    // previous one survives, while the finding keeps its identity, its
    // content, and its lifecycle status.
    let architectural = vec![ReviewGroup {
        id: "arch-1".to_string(),
        title: "Shared root cause".to_string(),
        description: Some("One lock explains both symptoms".to_string()),
        ..Default::default()
    }];
    let replaced = ReviewStorage::regroup_session(
        root,
        "rev-post-analysis",
        "second pass".to_string(),
        architectural.clone(),
        vec![(FINDING_UUID.to_string(), "arch-1".to_string())],
    )
    .unwrap()
    .unwrap();
    assert_eq!(replaced.description.as_deref(), Some("second pass"));
    assert_eq!(replaced.groups, architectural);
    assert_eq!(replaced.items[0].group.as_deref(), Some("arch-1"));
    assert_eq!(replaced.items[0].title, session.items[0].title);
    assert_eq!(replaced.items[0].uuid.as_deref(), Some(FINDING_UUID));
    assert_eq!(replaced.items[0].status, IssueStatus::Discussion);

    let loaded = ReviewStorage::load_review(root, "rev-post-analysis")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.groups, architectural);
    assert_eq!(loaded.description.as_deref(), Some("second pass"));

    // A finding the assignment does not name keeps the group it already
    // belongs to, so a partial classification never strands it outside the
    // rendered tree.
    let untouched = ReviewStorage::regroup_session(
        root,
        "rev-post-analysis",
        "third pass".to_string(),
        architectural,
        Vec::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(untouched.items[0].group.as_deref(), Some("arch-1"));

    // A missing session resolves to nothing instead of creating one.
    let missing = ReviewStorage::regroup_session(
        root,
        "rev-missing",
        "no session".to_string(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert!(missing.is_none());
    let missing_loaded = ReviewStorage::load_review(root, "rev-missing").unwrap();
    assert!(missing_loaded.is_none());
}

/// Every finding a group produces is appended with a fresh identity and the
/// provenance of the group that produced it.
#[test]
fn test_append_group_findings_stamps_uuid_and_group() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = ReviewSession {
        id: "rev-group-append".to_string(),
        title: "Group Append".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        items: Vec::new(),
        raw_markdown: String::new(),
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let finding = |id: u32| ReviewIssue {
        id,
        title: format!("Finding {id}"),
        uuid: None,
        group: None,
        model: None,
        issue_type: IssueType::Architecture,
        status: IssueStatus::Discussion,
        description: "Body".to_string(),
        recommendations: vec![],
        files: vec!["src/a.rs".to_string()],
        readonly_files: vec![],
        note: None,
    };

    let stored = ReviewStorage::append_group_findings(
        root,
        "rev-group-append",
        "core",
        vec![finding(1), finding(2)],
    )
    .unwrap()
    .unwrap();

    assert_eq!(stored.items.len(), 2);
    for item in &stored.items {
        let uuid = item.uuid.as_deref().expect("identity assigned");
        assert!(crate::review::uuid::is_valid_uuid_v7(uuid), "uuid: {uuid}");
        assert_eq!(item.group.as_deref(), Some("core"));
    }
    assert_ne!(stored.items[0].uuid, stored.items[1].uuid);

    // A second group appends to the same session without rewriting the
    // findings the first one produced.
    let after_second = ReviewStorage::append_group_findings(
        root,
        "rev-group-append",
        "protocol",
        vec![finding(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after_second.items.len(), 3);
    assert_eq!(after_second.items[0].uuid, stored.items[0].uuid);
    assert_eq!(after_second.items[2].group.as_deref(), Some("protocol"));

    // A missing session resolves to nothing instead of creating one.
    let missing =
        ReviewStorage::append_group_findings(root, "rev-missing", "core", vec![finding(9)])
            .unwrap();
    assert!(missing.is_none());
}
/// Session carrying one classified group with the given remediation
/// horizon.
fn grouped_layer_session(review_id: &str, subplan: Vec<PlanItem>) -> ReviewSession {
    ReviewSession {
        id: review_id.to_string(),
        title: "Grouped Audit".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: Some("One shared root cause.".to_string()),
        groups: vec![ReviewGroup {
            id: "arch-1".to_string(),
            title: "Shared root cause".to_string(),
            description: Some("Both findings manifest one defect.".to_string()),
            subplan,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn test_update_group_subplan_normalizes_derived_container_statuses() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let review = "rev-group-subplan";
    ReviewStorage::save_session(root, &grouped_layer_session(review, Vec::new())).unwrap();

    // The container arrives as `Done` while one of its leaves is still
    // `Todo`; the persisted status must be recomputed from the leaves.
    let subplan = vec![PlanItem {
        id: "phase-1".to_string(),
        title: "Phase".to_string(),
        status: tauqe_protocol::PlanItemStatus::Done,
        children: vec![
            PlanItem {
                id: "p-1".to_string(),
                title: "Finished step".to_string(),
                status: tauqe_protocol::PlanItemStatus::Done,
                ..Default::default()
            },
            PlanItem {
                id: "p-2".to_string(),
                title: "Pending step".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            },
        ],
        ..Default::default()
    }];

    let updated = ReviewStorage::update_group_subplan(root, review, "arch-1", subplan).unwrap();
    let updated = updated.expect("the group accepts a horizon");
    assert_eq!(updated.subplan.len(), 1);
    assert_eq!(
        updated.subplan[0].status,
        tauqe_protocol::PlanItemStatus::InProgress
    );

    // The horizon belongs to the group, so it survives a reload with the
    // container status already derived from its leaves.
    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    assert_eq!(stored.groups[0].subplan.len(), 1);
    assert_eq!(
        stored.groups[0].subplan[0].status,
        tauqe_protocol::PlanItemStatus::InProgress
    );

    // An unknown group resolves to nothing and never rewrites the layer.
    let missing = ReviewStorage::update_group_subplan(root, review, "arch-9", Vec::new());
    assert!(missing.unwrap().is_none());
    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    assert_eq!(stored.groups[0].subplan.len(), 1);
}

/// One root cause is fixed by one horizon: a synthesized wave preserves the
/// finished steps of the group and records how the horizon was produced.
#[test]
fn test_replace_uncompleted_group_subplan_records_provenance() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let review = "rev-group-horizon";

    let existing = vec![
        PlanItem {
            id: "s-1".to_string(),
            title: "Finished step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Done,
            ..Default::default()
        },
        PlanItem {
            id: "s-2".to_string(),
            title: "Old step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            ..Default::default()
        },
    ];
    ReviewStorage::save_session(root, &grouped_layer_session(review, existing)).unwrap();

    let new_horizon = vec![
        PlanItem {
            id: "s-1".to_string(),
            title: "Stale duplicate".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            ..Default::default()
        },
        PlanItem {
            id: "s-3".to_string(),
            title: "New step".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            files: vec!["src/new_step.rs".to_string()],
            ..Default::default()
        },
    ];

    let updated = ReviewStorage::replace_uncompleted_group_subplan(
        root,
        review,
        "arch-1",
        new_horizon,
        test_architect(),
        Some("s-2".to_string()),
    )
    .unwrap();
    let updated = updated.expect("the group accepts a horizon");
    assert_eq!(updated.subplan.len(), 2);
    let finished = &updated.subplan[0];
    let synthesized = &updated.subplan[1];
    assert_eq!(finished.id, "s-1");
    assert_eq!(finished.status, tauqe_protocol::PlanItemStatus::Done);
    assert_eq!(synthesized.id, "s-3");
    assert_eq!(updated.subplan_architect, Some(test_architect()));
    assert_eq!(updated.subplan_last_replanned_step.as_deref(), Some("s-2"));

    // The provenance and the preserved foundation survive a reload, so a
    // later wave derives escalation from disk state.
    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    let group = &stored.groups[0];
    assert_eq!(group.subplan.len(), 2);
    assert_eq!(group.subplan[0].id, "s-1");
    assert_eq!(group.subplan_architect, Some(test_architect()));
    assert_eq!(group.subplan_last_replanned_step.as_deref(), Some("s-2"));

    // An unknown group resolves to nothing instead of accepting a horizon.
    let missing = ReviewStorage::replace_uncompleted_group_subplan(
        root,
        review,
        "arch-9",
        Vec::new(),
        test_architect(),
        None,
    )
    .unwrap();
    assert!(missing.is_none());
}

/// Finding of a grouped session, carrying every field the mass-status
/// primitive must preserve intact.
fn group_finding(id: u32, group: Option<&str>) -> ReviewItem {
    ReviewItem {
        id,
        title: format!("Finding {id}"),
        uuid: Some(format!("018f9c1e-6b3a-7c1e-8b3a-0000000000{id:02}")),
        group: group.map(str::to_string),
        model: None,
        issue_type: IssueType::Architecture,
        severity: ReviewSeverity::Warning,
        status: IssueStatus::Todo,
        recommendations: vec![format!("Apply change {id}")],
        files: vec![format!("src/file_{id}.rs")],
        readonly_files: vec!["Conventions.md".to_string()],
        resolution_notes: None,
        body: format!("Body {id}"),
    }
}

/// Architectural group carrying the remediation horizon its findings belong to.
fn architectural_group(id: &str) -> ReviewGroup {
    ReviewGroup {
        id: id.to_string(),
        title: format!("Root cause {id}"),
        description: Some("One defect explains every member finding.".to_string()),
        kind: ReviewGroupKind::Architectural,
        subplan: vec![PlanItem {
            id: "step-1".to_string(),
            title: "Fix the root cause".to_string(),
            status: tauqe_protocol::PlanItemStatus::Todo,
            files: vec!["src/root_cause.rs".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Group of independent findings, which owns no remediation horizon.
fn simple_group(id: &str) -> ReviewGroup {
    ReviewGroup {
        id: id.to_string(),
        title: format!("Group {id}"),
        kind: ReviewGroupKind::Simple,
        ..Default::default()
    }
}

/// Applies the mass-status primitive with one fixed transition.
fn mass_status_update(root: &Path, review_id: &str, group_id: &str) -> Option<usize> {
    ReviewStorage::update_session_group_findings(root, review_id, group_id, |issue| {
        issue.status = IssueStatus::Fixed;
    })
    .unwrap()
}

/// The unit of completion of an architectural group is its whole horizon, so
/// its findings must transition together: one atomic write moves every member
/// to `Fixed` while the findings of other groups stay untouched.
#[test]
fn test_update_session_group_findings_transitions_the_whole_group() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let review = "rev-group-mass-status";

    let session = ReviewSession {
        id: review.to_string(),
        title: "Grouped Lifecycle".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        groups: vec![architectural_group("arch-1"), simple_group("local")],
        items: vec![
            group_finding(1, Some("arch-1")),
            group_finding(2, Some("arch-1")),
            group_finding(3, Some("local")),
        ],
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let mut visited: Vec<u32> = Vec::new();
    let affected = ReviewStorage::update_session_group_findings(root, review, "arch-1", |issue| {
        visited.push(issue.id);
        issue.status = IssueStatus::Fixed;
        issue.note = Some("Resolved by the group horizon `step-1`".to_string());
    })
    .unwrap();

    assert_eq!(
        affected,
        Some(2),
        "every member of the group must be reached by one update"
    );
    assert_eq!(
        visited,
        vec![1, 2],
        "the updater observes exactly the findings of the named group"
    );

    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    for item in stored.items.iter().filter(|item| item.id != 3) {
        assert_eq!(item.status, IssueStatus::Fixed);
        assert_eq!(
            item.resolution_notes.as_deref(),
            Some("Resolved by the group horizon `step-1`"),
            "the note referencing the completed horizon must persist"
        );
        // The round-trip through the canonical `ReviewIssue` model must not
        // rewrite the content of a finding.
        assert_eq!(item.title, format!("Finding {}", item.id));
        assert_eq!(item.body, format!("Body {}", item.id));
        assert_eq!(item.files, vec![format!("src/file_{}.rs", item.id)]);
        assert_eq!(item.readonly_files, vec!["Conventions.md".to_string()]);
        assert_eq!(
            item.recommendations,
            vec![format!("Apply change {}", item.id)]
        );
    }

    let untouched = stored.find_item(3).unwrap();
    assert_eq!(
        untouched.status,
        IssueStatus::Todo,
        "a finding of another group is never updated"
    );
    assert!(untouched.resolution_notes.is_none());
    // The horizon belongs to the group and stays out of scope of the primitive.
    assert_eq!(stored.groups[0].subplan.len(), 1);
}

/// A finding that stores no group belongs to the implicit default group, so the
/// primitive addresses it by the default group identifier.
#[test]
fn test_update_session_group_findings_resolves_the_implicit_default_group() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let review = "rev-group-implicit-default";

    let session = ReviewSession {
        id: review.to_string(),
        title: "Implicit Default".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        groups: vec![
            simple_group(DEFAULT_REVIEW_GROUP_ID),
            architectural_group("arch-1"),
        ],
        items: vec![group_finding(1, None), group_finding(2, Some("arch-1"))],
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let affected = mass_status_update(root, review, DEFAULT_REVIEW_GROUP_ID);

    assert_eq!(
        affected,
        Some(1),
        "a finding without a group belongs to the implicit default group"
    );

    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
    assert_eq!(stored.find_item(2).unwrap().status, IssueStatus::Todo);
}

/// An unknown group or session changes nothing, while a declared group without
/// member findings is a valid no-op target.
#[test]
fn test_update_session_group_findings_rejects_unknown_targets() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let review = "rev-group-unknown";

    // `local` is declared but collects no finding, so it is a valid target with
    // an empty membership.
    let session = ReviewSession {
        id: review.to_string(),
        title: "Unknown Targets".to_string(),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        groups: vec![architectural_group("arch-1"), simple_group("local")],
        items: vec![group_finding(1, Some("arch-1"))],
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();

    let unknown_group = mass_status_update(root, review, "arch-9");
    assert!(
        unknown_group.is_none(),
        "an undeclared group changes nothing"
    );

    let unknown_session = mass_status_update(root, "rev-missing", "arch-1");
    assert!(
        unknown_session.is_none(),
        "a missing session changes nothing"
    );
    assert!(ReviewStorage::load_review(root, "rev-missing")
        .unwrap()
        .is_none());

    let empty_group = mass_status_update(root, review, "local");
    assert_eq!(empty_group, Some(0), "a group with no finding is a no-op");

    let stored = ReviewStorage::load_review(root, review).unwrap().unwrap();
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Todo);
}
