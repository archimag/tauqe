//! Integration tests for the [`ApplyPolicy`] strategies.
//!
//! Each test drives a policy through a full turn against a temporary Git
//! repository. The two transactional outcomes are covered for both policies:
//! finalization, where a verified change set becomes durable, and rollback,
//! where the working tree returns to its pre-edit state.

use crate::config::project::ProjectConfig;
use crate::context::{ContextAccess, ContextLayer};
use std::path::Path;

use crate::edits::EditCreate;
use crate::edits::EditOperation;
use crate::edits::EditProposal;
use crate::edits::EditReplace;

use crate::context::ContextManager;
use crate::edits::transaction::RollbackOutcome;
use crate::edits::VerificationVerdict;
use crate::git::cmd::run_git;
use crate::stop::StopSource;
use crate::workflow::git_policy::GitTransactionPolicy;
use crate::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
use crate::workflow::policy::{ApplyPolicy, DirectApplyPolicy, PolicyRollback};

#[tokio::test]
async fn retrieving_without_admission_does_not_commit_even_without_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let transaction = OwnedApplyPolicy::git(root, stop.token().hard_cancellation());
    let batch = replace_edit();
    transaction
        .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
        .await
        .unwrap();
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredByPolicy)
    ));
    assert_eq!(head_subject(root), "Initial commit");
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
}

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
    vec![EditOperation::Replace(EditReplace {
        path: "a.rs".to_string(),
        old_text: "one\n".to_string(),
        new_text: "two\n".to_string(),
    })]
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
    assert_eq!(
        policy.rollback(root).await.unwrap(),
        PolicyRollback::NoCheckpoint
    );

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

    assert_eq!(
        policy.rollback(root).await.unwrap(),
        PolicyRollback::RestoredCheckpoint
    );

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

    assert_eq!(
        policy.rollback(root).await.unwrap(),
        PolicyRollback::RestoredCheckpoint
    );

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
        EditOperation::Replace(EditReplace {
            path: "user_file.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "ONE\n".to_string(),
        }),
        EditOperation::Replace(EditReplace {
            path: "auto_file.rs".to_string(),
            old_text: "two\n".to_string(),
            new_text: "TWO\n".to_string(),
        }),
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
        edits: vec![EditOperation::Create(EditCreate {
            path: "generated.rs".to_string(),
            content: "pub fn generated() {}\n".to_string(),
        })],
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

    let edits = vec![EditOperation::Replace(EditReplace {
        path: "auto_file.rs".to_string(),
        old_text: "two\n".to_string(),
        new_text: "TWO\n".to_string(),
    })];

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
    assert_eq!(
        policy.rollback(root).await.unwrap(),
        PolicyRollback::RestoredCheckpoint
    );

    // A rolled-back change set leaves the context exactly as it was: the
    // discovered file stays available for the next attempt.
    assert_eq!(layer_of(&cm, "auto_file.rs"), Some(ContextLayer::Auto));
    assert_eq!(cm.auto_files(), vec!["auto_file.rs".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("auto_file.rs")).unwrap(),
        "two\n"
    );
}

#[tokio::test]
async fn owned_direct_transaction_restores_first_touch_and_rejects_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\n").unwrap();
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let transaction = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    for (old, new) in [("one\n", "two\n"), ("two\n", "three\n")] {
        let batch = [EditOperation::Replace(EditReplace {
            path: "a.rs".into(),
            old_text: old.into(),
            new_text: new.into(),
        })];
        transaction
            .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
            .await
            .unwrap();
    }
    assert_eq!(
        transaction.original_bytes().await.get("a.rs"),
        Some(&Some(b"one\n".to_vec()))
    );
    assert_eq!(transaction.changed_files().await, ["a.rs"]);
    let mut paths = transaction.changed_files().await;
    paths.push("unrelated.rs".into());
    let mut originals = transaction.original_bytes().await;
    originals.insert("a.rs".into(), Some(b"wrong\n".to_vec()));
    assert_eq!(transaction.changed_files().await, ["a.rs"]);
    assert_eq!(
        transaction.original_bytes().await.get("a.rs"),
        Some(&Some(b"one\n".to_vec()))
    );
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert!(transaction
        .apply(&project(), &mut context, &[], &[])
        .await
        .is_err());
}

#[tokio::test]
async fn owned_transaction_rolls_back_multiple_healing_passes_to_first_touch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"original\n").unwrap();
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let transaction = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    for (old, new) in [("original\n", "broken\n"), ("broken\n", "repaired\n")] {
        let batch = [EditOperation::Replace(EditReplace {
            path: "a.rs".into(),
            old_text: old.into(),
            new_text: new.into(),
        })];
        transaction
            .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
            .await
            .unwrap();
    }
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"repaired\n");
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"original\n");
}

#[tokio::test]
async fn interrupted_first_touch_keeps_original_bytes_and_absence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"original\n").unwrap();
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let transaction = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    let first = [EditOperation::Replace(EditReplace {
        path: "a.rs".into(),
        old_text: "original\n".into(),
        new_text: "healing\n".into(),
    })];
    transaction
        .apply(&project(), &mut context, &["a.rs".into()], &[&first])
        .await
        .unwrap();
    let second = [EditOperation::Create(EditCreate {
        path: "new.rs".into(),
        content: "new\n".into(),
    })];
    transaction
        .apply(&project(), &mut context, &["new.rs".into()], &[&second])
        .await
        .unwrap();
    let mut snapshot = transaction.original_bytes().await;
    snapshot.insert("a.rs".into(), Some(b"incorrect".to_vec()));
    assert_eq!(
        transaction.original_bytes().await.get("a.rs"),
        Some(&Some(b"original\n".to_vec()))
    );
    assert_eq!(
        transaction.original_bytes().await.get("new.rs"),
        Some(&None)
    );
    assert_eq!(transaction.changed_files().await, ["a.rs", "new.rs"]);
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"original\n");
    assert!(!root.join("new.rs").exists());
}

#[tokio::test]
async fn owned_finalization_is_terminal_without_a_git_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\n").unwrap();
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let transaction = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    let batch = [EditOperation::Replace(EditReplace {
        path: "a.rs".into(),
        old_text: "one\n".into(),
        new_text: "two\n".into(),
    })];
    transaction
        .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
        .await
        .unwrap();
    let Settlement::Finalized {
        edit,
        cancelled: false,
    } = transaction
        .finalize("update", VerificationVerdict::Passed)
        .await
        .unwrap()
    else {
        panic!("expected finalization")
    };
    assert!(edit.applied);
    assert!(edit.commit_hash.is_none());
    assert_eq!(edit.changed_files, ["a.rs"]);
    let Settlement::Finalized {
        edit: repeated,
        cancelled: false,
    } = transaction
        .finalize("update", VerificationVerdict::Passed)
        .await
        .unwrap()
    else {
        panic!("expected the already finalized result")
    };
    assert_eq!(repeated.changed_files, edit.changed_files);
    assert_eq!(repeated.commit_hash, edit.commit_hash);
    assert!(matches!(
        transaction.finish_interrupted(&mut context).await.unwrap(),
        Settlement::Finalized { .. }
    ));
    assert!(transaction
        .apply(&project(), &mut context, &[], &[])
        .await
        .is_err());
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"two\n");
}

#[tokio::test]
async fn cancellation_before_finalization_restores_without_committing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let policy = OwnedApplyPolicy::git(root, stop.token().hard_cancellation());
    let batch = replace_edit();
    policy
        .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
        .await
        .unwrap();
    stop.request_hard();
    assert!(policy
        .finalize("update", VerificationVerdict::Passed)
        .await
        .is_err());
    assert!(matches!(
        policy.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredByPolicy)
    ));
    assert_eq!(head_subject(root), "Initial commit");
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
}

#[tokio::test]
async fn direct_policy_clones_observe_owner_replacement_after_rollback() {
    let mut policy = DirectApplyPolicy::new();
    let mut clone = policy.clone();
    let dir = tempfile::tempdir().unwrap();
    policy.rollback(dir.path()).await.unwrap();
    assert_eq!(
        clone.rollback(dir.path()).await.unwrap(),
        PolicyRollback::NoCheckpoint
    );
}

#[tokio::test]
async fn owned_policy_retains_first_touch_across_healing_and_refuses_reuse() {
    use crate::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\n").unwrap();
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let policy = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    for (old, new) in [("one\n", "two\n"), ("two\n", "three\n")] {
        let batch = [EditOperation::Replace(EditReplace {
            path: "a.rs".into(),
            old_text: old.into(),
            new_text: new.into(),
        })];
        policy
            .apply(&project(), &mut context, &["a.rs".into()], &[&batch])
            .await
            .unwrap();
    }
    assert_eq!(
        policy.original_bytes().await.get("a.rs"),
        Some(&Some(b"one\n".to_vec()))
    );
    assert_eq!(policy.changed_files().await, ["a.rs"]);
    assert!(matches!(
        policy.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
    assert!(policy
        .apply(&project(), &mut context, &[], &[])
        .await
        .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn lost_command_wait_records_created_and_deleted_artifacts_before_rollback() {
    use crate::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let policy = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    let operations = policy.begin_command().await.unwrap();
    let project: ProjectConfig = serde_json::from_str(
        r#"{"post_hook":{"command":"sh","args":["-c","rm a.rs; echo generated > created.rs"]}}"#,
    )
    .unwrap();
    let result =
        crate::toolchain::run_post_hooks_with_operations(root, &project, &[], None, &operations)
            .await
            .unwrap();
    assert!(result.success);
    // Simulate a lost caller after the process finished but before accounting.
    assert!(!root.join("a.rs").exists());
    assert!(root.join("created.rs").exists());
    assert!(matches!(
        policy.finish_interrupted(&mut context).await.unwrap(),
        Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
    ));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
    assert!(!root.join("created.rs").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn command_accounting_preserves_first_touch_across_healing_passes() {
    use crate::workflow::policy::owned::OwnedApplyPolicy;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let policy = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    for content in ["first", "second"] {
        let operations = policy.begin_command().await.unwrap();
        let project: ProjectConfig = serde_json::from_str(&format!(
            r#"{{"post_hook":{{"command":"sh","args":["-c","echo {content} > a.rs"]}}}}"#,
        ))
        .unwrap();
        assert!(
            crate::toolchain::run_post_hooks_with_operations(
                root,
                &project,
                &[],
                None,
                &operations,
            )
            .await
            .unwrap()
            .success
        );
        assert_eq!(policy.complete_command().await.unwrap(), ["a.rs"]);
    }
    assert_eq!(
        policy.original_bytes().await.get("a.rs"),
        Some(&Some(b"one\n".to_vec()))
    );
    policy.finish_interrupted(&mut context).await.unwrap();
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"one\n");
}

#[tokio::test]
async fn unconfirmed_command_shutdown_refuses_restoration() {
    use crate::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    let mut context = editable_context(root);
    let stop = StopSource::new();
    let policy = OwnedApplyPolicy::direct(root, stop.token().hard_cancellation());
    let operations = policy.begin_command().await.unwrap();
    let handle = operations.start(|| (), None).unwrap();
    handle.mark_unconfirmed();
    std::fs::write(root.join("a.rs"), b"uncertain\n").unwrap();
    let Settlement::RolledBack(RollbackOutcome::Incomplete { backup_error, .. }) =
        policy.finish_interrupted(&mut context).await.unwrap()
    else {
        panic!("uncertain shutdown must not claim restoration");
    };
    assert!(backup_error.contains("shutdown could not be confirmed"));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"uncertain\n");
}
