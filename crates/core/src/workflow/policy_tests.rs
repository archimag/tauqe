//! Integration tests for the [`ApplyPolicy`] strategies.
//!
//! Each test drives a policy through a full turn against a temporary Git
//! repository. The two transactional outcomes are covered for both policies:
//! finalization, where a verified change set becomes durable, and rollback,
//! where the working tree returns to its pre-edit state.

use std::path::Path;

use tauqe_protocol::{ContextAccess, ContextLayer, EditOperation, EditProposal, ProjectConfig};

use crate::context::ContextManager;
use crate::git::cmd::run_git;
use crate::workflow::git_policy::GitTransactionPolicy;
use crate::workflow::policy::{ApplyPolicy, DirectApplyPolicy};

/// Project configuration the policy tests stage under: no formatter, no hook.
fn project() -> ProjectConfig {
    ProjectConfig::default()
}

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
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    let changed = policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
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
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    let changed = policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
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
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
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
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "update a.rs".to_string(),
        edits,
    };
    policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
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

/// Returns the context layer that currently owns `path`.
fn layer_of(context: &ContextManager, path: &str) -> Option<ContextLayer> {
    context
        .get_state()
        .items
        .into_iter()
        .find(|item| item.path == path)
        .map(|item| item.layer)
}

/// Collects the paths currently held by the developer-owned user layer.
fn user_layer_paths(context: &ContextManager) -> Vec<String> {
    context
        .get_state()
        .items
        .into_iter()
        .filter(|item| item.layer == ContextLayer::User)
        .map(|item| item.path)
        .collect()
}

/// Creates a repository with one developer-owned file and one file discovered
/// during a turn, and returns a context holding both layers.
fn two_layer_context(root: &Path) -> ContextManager {
    crate::git::init_repository(Some(root), true).unwrap();
    std::fs::write(root.join("user_file.rs"), "one\n").unwrap();
    std::fs::write(root.join("auto_file.rs"), "two\n").unwrap();
    crate::git::cmd::stage_files(
        root,
        &["user_file.rs".to_string(), "auto_file.rs".to_string()],
    )
    .unwrap();
    crate::git::cmd::commit_with_developer_author(root, "Add fixtures", false).unwrap();

    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("user_file.rs", ContextAccess::Editable)
        .unwrap();
    cm.add_auto_file("auto_file.rs", ContextAccess::Editable)
        .unwrap();
    cm
}

#[tokio::test]
async fn applying_a_change_set_preserves_the_owning_layer_of_every_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut cm = two_layer_context(root);

    let edits = vec![
        EditOperation::Replace {
            path: "user_file.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "ONE\n".to_string(),
        },
        EditOperation::Replace {
            path: "auto_file.rs".to_string(),
            old_text: "two\n".to_string(),
            new_text: "TWO\n".to_string(),
        },
    ];

    let mut policy = DirectApplyPolicy::new();
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "update both files".to_string(),
        edits,
    };
    let changed = policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(
        changed,
        vec!["user_file.rs".to_string(), "auto_file.rs".to_string()]
    );

    // Applying a change set widens neither layer: the developer-owned file keeps
    // the user layer and the discovered file keeps the transient auto layer.
    assert_eq!(layer_of(&cm, "user_file.rs"), Some(ContextLayer::User));
    assert_eq!(layer_of(&cm, "auto_file.rs"), Some(ContextLayer::Auto));
    assert_eq!(cm.auto_files(), vec!["auto_file.rs".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("auto_file.rs")).unwrap(),
        "TWO\n"
    );
}

#[tokio::test]
async fn creating_a_file_joins_the_auto_layer_and_never_the_user_layer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut cm = two_layer_context(root);

    let proposal = EditProposal {
        summary: "create generated.rs".to_string(),
        edits: vec![EditOperation::Create {
            path: "generated.rs".to_string(),
            content: "pub fn generated() {}\n".to_string(),
        }],
    };

    let mut policy = DirectApplyPolicy::new();
    let changed = policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
        .await
        .unwrap();
    assert_eq!(changed, vec!["generated.rs".to_string()]);

    // A path the turn created joins the transient auto layer, so the user layer
    // keeps exactly what the developer added by hand.
    assert_eq!(layer_of(&cm, "generated.rs"), Some(ContextLayer::Auto));
    let mut auto_files = cm.auto_files();
    auto_files.sort();
    assert_eq!(
        auto_files,
        vec!["auto_file.rs".to_string(), "generated.rs".to_string()]
    );
    assert_eq!(user_layer_paths(&cm), vec!["user_file.rs".to_string()]);
}

#[tokio::test]
async fn rolling_back_a_change_set_keeps_the_auto_layer_intact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut cm = two_layer_context(root);

    let edits = vec![EditOperation::Replace {
        path: "auto_file.rs".to_string(),
        old_text: "two\n".to_string(),
        new_text: "TWO\n".to_string(),
    }];

    let mut policy = GitTransactionPolicy::new();
    assert!(policy
        .staging(root, &project(), &cm, &edits, None, None)
        .await
        .is_success());

    let proposal = EditProposal {
        summary: "failed update".to_string(),
        edits,
    };
    policy
        .apply_temporary(root, &project(), &mut cm, &proposal)
        .await
        .unwrap();
    assert!(policy.rollback(root).await.unwrap());

    // A rolled-back change set leaves the context exactly as it was: the
    // discovered file stays available for the next attempt.
    assert_eq!(layer_of(&cm, "auto_file.rs"), Some(ContextLayer::Auto));
    assert_eq!(cm.auto_files(), vec!["auto_file.rs".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("auto_file.rs")).unwrap(),
        "two\n"
    );
}
