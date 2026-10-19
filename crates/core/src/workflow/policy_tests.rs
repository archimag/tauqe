//! Integration tests for the [`ApplyPolicy`] strategies.
//!
//! Each test drives a policy through a full turn against a temporary Git
//! repository. The two transactional outcomes are covered for both policies:
//! finalization, where a verified change set becomes durable, and rollback,
//! where the working tree returns to its pre-edit state.

use std::path::Path;

use tauqe_protocol::{ContextAccess, EditOperation, EditProposal};

use crate::context::ContextManager;
use crate::git::cmd::run_git;
use crate::workflow::git_policy::GitTransactionPolicy;
use crate::workflow::policy::{ApplyPolicy, DirectApplyPolicy};

/// Initializes a temporary repository with a single tracked file `a.rs`.
fn init_repo(root: &Path) {
    crate::git::init_repository(Some(root), true).unwrap();
    std::fs::write(root.join("a.rs"), "one\n").unwrap();
    crate::git::cmd::stage_files(root, &["a.rs".to_string()]).unwrap();
    crate::git::cmd::commit_with_developer_author(root, "Initial commit", false).unwrap();
}

/// Builds a context whose single file is editable.
fn editable_context(root: &Path) -> ContextManager {
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();
    cm
}

/// A staged replace that rewrites `a.rs` in memory.
fn replace_edit() -> Vec<EditOperation> {
    vec![EditOperation::Replace {
        path: "a.rs".to_string(),
        old_text: "one\n".to_string(),
        new_text: "two\n".to_string(),
    }]
}

fn head_subject(root: &Path) -> String {
    run_git(Some(root), &["log", "-1", "--format=%s"])
        .unwrap()
        .trim()
        .to_string()
}

#[tokio::test]
async fn direct_policy_rollback_and_finalize_are_noops() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut cm = editable_context(root);

    let edits = replace_edit();
    let mut policy = DirectApplyPolicy::new();
    assert!(policy.staging(root, &cm, &edits, None).await.is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    let changed = policy
        .apply_temporary(root, &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(changed, vec!["a.rs".to_string()]);
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

    // The direct policy keeps the write on disk without recording a commit and
    // reports that nothing could be rolled back.
    assert!(policy
        .apply_final(root, &changed, "update a.rs")
        .await
        .unwrap()
        .is_none());
    assert!(!policy.rollback(root).await.unwrap());

    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    assert_eq!(head_subject(root), "Initial commit");
}

#[tokio::test]
async fn git_policy_finalize_creates_ai_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut cm = editable_context(root);

    let edits = replace_edit();
    let mut policy = GitTransactionPolicy::new();
    assert!(policy.staging(root, &cm, &edits, None).await.is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    let changed = policy
        .apply_temporary(root, &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(changed, vec!["a.rs".to_string()]);

    let commit = policy
        .apply_final(root, &changed, "update a.rs")
        .await
        .unwrap();
    assert!(commit.is_some());

    // Finalization squashes the temporary step commit into a single AI commit
    // that carries the requested summary.
    assert_eq!(head_subject(root), "update a.rs");
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
}

#[tokio::test]
async fn git_policy_rollback_restores_pre_edit_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut cm = editable_context(root);

    let edits = replace_edit();
    let mut policy = GitTransactionPolicy::new();
    assert!(policy.staging(root, &cm, &edits, None).await.is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    policy
        .apply_temporary(root, &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

    assert!(policy.rollback(root).await.unwrap());

    // Rollback reverts the model edit and the temporary commits, leaving HEAD
    // at its pre-turn revision.
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
    assert_eq!(head_subject(root), "Initial commit");
}

#[tokio::test]
async fn git_policy_rollback_preserves_uncommitted_developer_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    // The developer has uncommitted work in a separate file before the turn.
    std::fs::write(root.join("notes.txt"), "uncommitted work\n").unwrap();

    let mut cm = editable_context(root);
    let edits = replace_edit();
    let mut policy = GitTransactionPolicy::new();
    assert!(policy.staging(root, &cm, &edits, None).await.is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    policy
        .apply_temporary(root, &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

    assert!(policy.rollback(root).await.unwrap());

    // The model edit is gone, but the developer's uncommitted work survives.
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "uncommitted work\n"
    );
    assert_eq!(head_subject(root), "Initial commit");
}
