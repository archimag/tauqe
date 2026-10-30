use super::*;
use std::path::Path;
use tauqe_protocol::{ContextAccess, ContextLayer, EditOperation, EditProposal};
use tempfile::tempdir;

use crate::context::ContextManager;

fn replace(path: &str, old_text: &str, new_text: &str) -> EditOperation {
    EditOperation::Replace {
        path: path.to_string(),
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }
}

#[tokio::test]
async fn a_failed_member_rolls_back_only_its_own_effect_and_keeps_disk_unchanged() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one").unwrap();
    std::fs::write(root.join("b.rs"), "two").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();
    cm.add_file("b.rs", ContextAccess::Editable).unwrap();
    let before = cm.get_state().revision;
    // The two replacements of `a.rs` are separated by another file, so they form
    // two members. The second member is refused while the first one keeps its own
    // effect: rolling it back would discard a change the model already delivered
    // and would leave the reported change set disagreeing with the staged state.
    let edits = vec![
        replace("a.rs", "one", "changed"),
        replace("b.rs", "two", "success"),
        replace("./a.rs", "missing", "failed"),
    ];
    let result = stage_and_validate_edits(root, &cm, &edits, None);
    assert_eq!(result.successful_paths, vec!["a.rs", "b.rs"]);
    assert_eq!(
        result.successful_edits,
        vec![edits[0].clone(), edits[1].clone()]
    );
    assert_eq!(result.staged_state.staged_files["a.rs"], "changed");
    assert_eq!(result.staged_state.staged_files["b.rs"], "success");
    assert_eq!(result.failed_files.len(), 1);
    assert_eq!(result.failed_files[0].path, "a.rs");
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::NoMatch { .. }
    ));
    assert_eq!(cm.get_state().revision, before);
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");

    let proposal = EditProposal {
        summary: "test".into(),
        edits,
    };
    assert!(apply_edit_proposal(root, &mut cm, &proposal).await.is_err());
    assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
}

#[test]
fn retries_use_staged_content_and_preserve_it_on_failure() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();
    let first = stage_and_validate_edits(root, &cm, &[replace("a.rs", "one", "two")], None);
    let second = stage_and_validate_edits(
        root,
        &cm,
        &[replace("a.rs", "two", "three")],
        Some(&first.staged_state),
    );
    let failed = stage_and_validate_edits(
        root,
        &cm,
        &[replace("a.rs", "missing", "four")],
        Some(&second.staged_state),
    );
    assert!(!failed.is_success());
    assert_eq!(failed.staged_state, second.staged_state);
    assert_eq!(first.staged_state.staged_files["a.rs"], "two");
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
}

#[tokio::test]
async fn create_replace_delete_and_recreate_follow_operation_order() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut cm = ContextManager::new(root.to_path_buf());
    let create = EditOperation::Create {
        path: "new.rs".into(),
        content: "one".into(),
    };
    let delete = EditOperation::Delete {
        path: "new.rs".into(),
    };
    let first = stage_and_validate_edits(root, &cm, std::slice::from_ref(&create), None);
    let second = stage_and_validate_edits(
        root,
        &cm,
        &[replace("new.rs", "one", "two")],
        Some(&first.staged_state),
    );
    assert_eq!(second.staged_state.staged_files["new.rs"], "two");
    let deleted = stage_and_validate_edits(
        root,
        &cm,
        std::slice::from_ref(&delete),
        Some(&second.staged_state),
    );
    assert!(deleted.is_success());
    assert!(deleted.staged_state.deleted_paths.contains("new.rs"));
    assert!(!deleted.staged_state.staged_files.contains_key("new.rs"));
    assert!(!root.join("new.rs").exists());

    let proposal = EditProposal {
        summary: "create".into(),
        edits: vec![
            create.clone(),
            delete,
            create,
            replace("new.rs", "one", "final"),
        ],
    };
    assert_eq!(
        apply_edit_proposal(root, &mut cm, &proposal).await.unwrap(),
        vec!["new.rs"]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.rs")).unwrap(),
        "final"
    );
    assert!(cm.is_editable("new.rs").unwrap());
    // The turn registers a created file in the transient auto layer; the
    // user layer stays reserved for explicit manual additions.
    let state = cm.get_state();
    let new_item = state.items.iter().find(|i| i.path == "new.rs").unwrap();
    assert_eq!(new_item.layer, ContextLayer::Auto);
    let deletion = EditProposal {
        summary: "delete".into(),
        edits: vec![EditOperation::Delete {
            path: "new.rs".into(),
        }],
    };
    apply_edit_proposal(root, &mut cm, &deletion).await.unwrap();
    assert!(!root.join("new.rs").exists());
    assert!(!cm.contains("new.rs").unwrap());
}

#[test]
fn permissions_paths_missing_files_and_ambiguous_matches_are_reported() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("outside.rs"), "one").unwrap();
    std::fs::write(root.join("ambiguous.rs"), "one one").unwrap();
    std::fs::write(root.join("missing.rs"), "one").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("ambiguous.rs", ContextAccess::Editable)
        .unwrap();
    cm.add_file("missing.rs", ContextAccess::Editable).unwrap();
    std::fs::remove_file(root.join("missing.rs")).unwrap();
    for edit in [
        replace("outside.rs", "one", "two"),
        EditOperation::Create {
            path: "outside.rs".into(),
            content: "two".into(),
        },
        EditOperation::Delete {
            path: "outside.rs".into(),
        },
    ] {
        let result = stage_and_validate_edits(root, &cm, &[edit], None);
        assert!(matches!(
            &result.failed_files[0].error,
            EditError::NotInContext(_)
        ));
    }
    let ambiguous =
        stage_and_validate_edits(root, &cm, &[replace("ambiguous.rs", "one", "two")], None);
    assert!(matches!(
        &ambiguous.failed_files[0].error,
        EditError::AmbiguousMatch { count: 2, .. }
    ));
    let missing = stage_and_validate_edits(root, &cm, &[replace("missing.rs", "one", "two")], None);
    assert!(matches!(
        &missing.failed_files[0].error,
        EditError::FileNotFound(_)
    ));
    let invalid =
        stage_and_validate_edits(root, &cm, &[replace("../escape.rs", "one", "two")], None);
    assert!(matches!(
        &invalid.failed_files[0].error,
        EditError::InvalidPath(_, _)
    ));
}

#[tokio::test]
async fn readonly_file_in_context_can_be_edited_and_promoted_to_editable() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("ref.rs"), "fn original() {}").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("ref.rs", ContextAccess::ReadOnly).unwrap();
    assert!(!cm.is_editable("ref.rs").unwrap());

    let edit = replace("ref.rs", "fn original() {}", "fn updated() {}");
    let stage_res = stage_and_validate_edits(root, &cm, std::slice::from_ref(&edit), None);
    assert!(stage_res.is_success());
    assert_eq!(
        stage_res.staged_state.staged_files["ref.rs"],
        "fn updated() {}"
    );

    let proposal = EditProposal {
        summary: "Update read-only file".into(),
        edits: vec![edit],
    };
    let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert_eq!(changed, vec!["ref.rs"]);
    assert_eq!(
        std::fs::read_to_string(root.join("ref.rs")).unwrap(),
        "fn updated() {}"
    );
    assert!(cm.is_editable("ref.rs").unwrap());
}

#[tokio::test]
async fn overwrite_replaces_existing_file_and_requires_context() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let op = EditOperation::Overwrite {
        path: "a.rs".into(),
        content: "all new".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&op), None);
    assert!(result.is_success());
    assert_eq!(result.staged_state.staged_files["a.rs"], "all new");
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");

    let missing = EditOperation::Overwrite {
        path: "missing.rs".into(),
        content: "x".into(),
    };
    let result = stage_and_validate_edits(root, &cm, &[missing], None);
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::NotInContext(_)
    ));

    let proposal = EditProposal {
        summary: "overwrite".into(),
        edits: vec![op],
    };
    apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "all new"
    );
}

#[tokio::test]
async fn test_move_file_stages_and_applies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("old.rs"), "fn hello() {}").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("old.rs", ContextAccess::Editable).unwrap();

    let move_op = EditOperation::Move {
        from: "old.rs".into(),
        to: "new.rs".into(),
    };

    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&move_op), None);
    assert!(result.is_success());
    assert_eq!(result.staged_state.staged_files["new.rs"], "fn hello() {}");
    assert!(result.staged_state.deleted_paths.contains("old.rs"));

    let proposal = EditProposal {
        summary: "rename".into(),
        edits: vec![move_op],
    };
    apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert!(!root.join("old.rs").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("new.rs")).unwrap(),
        "fn hello() {}"
    );
    assert!(cm.is_editable("new.rs").unwrap());
    assert!(!cm.contains("old.rs").unwrap());
}

#[tokio::test]
async fn chained_moves_leave_only_the_final_destination_in_context() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "fn hello() {}").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let proposal = EditProposal {
        summary: "chained move".into(),
        edits: vec![
            EditOperation::Move {
                from: "a.rs".into(),
                to: "b.rs".into(),
            },
            EditOperation::Move {
                from: "b.rs".into(),
                to: "c.rs".into(),
            },
        ],
    };
    apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();

    assert!(!root.join("a.rs").exists());
    assert!(!root.join("b.rs").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("c.rs")).unwrap(),
        "fn hello() {}"
    );
    assert!(cm.is_editable("c.rs").unwrap());
    assert!(!cm.contains("a.rs").unwrap());
    assert!(!cm.contains("b.rs").unwrap());
}

#[tokio::test]
async fn move_with_recreated_source_keeps_source_and_destination_in_context() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "fn original() {}").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let proposal = EditProposal {
        summary: "move and recreate".into(),
        edits: vec![
            EditOperation::Move {
                from: "a.rs".into(),
                to: "b.rs".into(),
            },
            EditOperation::Create {
                path: "a.rs".into(),
                content: "fn recreated() {}".into(),
            },
        ],
    };
    apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("b.rs")).unwrap(),
        "fn original() {}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "fn recreated() {}"
    );
    assert!(cm.is_editable("a.rs").unwrap());
    assert!(cm.is_editable("b.rs").unwrap());
}

/// A refused member never destroys the effect of an applied member it shares a
/// path with: the rename destination keeps the content the rename staged, so the
/// retry of the refused replacement has a base to build on.
#[test]
fn a_failed_replace_after_a_move_keeps_the_moved_content_at_its_destination() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "fn hello() {}\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let edits = vec![
        EditOperation::Move {
            from: "a.rs".into(),
            to: "b.rs".into(),
        },
        replace("b.rs", "missing text", "irrelevant"),
    ];
    let result = stage_and_validate_edits(root, &cm, &edits, None);

    assert_eq!(result.failed_files.len(), 1);
    assert_eq!(result.failed_files[0].path, "b.rs");
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::NoMatch { .. }
    ));
    // The rename applied cleanly and keeps its effect in both directions: the
    // destination holds the moved content and the source stays marked as moved.
    assert_eq!(result.staged_state.staged_files["b.rs"], "fn hello() {}\n");
    assert!(result.staged_state.deleted_paths.contains("a.rs"));
    assert_eq!(result.successful_paths, vec!["a.rs", "b.rs"]);
    assert_eq!(result.successful_edits, vec![edits[0].clone()]);
    // Nothing above reached disk.
    assert!(root.join("a.rs").exists());
    assert!(!root.join("b.rs").exists());

    // The retry of the replacement converges against the content the rename staged,
    // because a file the attempt created in memory needs no context entry.
    let retry = stage_and_validate_edits(
        root,
        &cm,
        &[replace("b.rs", "fn hello() {}", "fn hello() -> i32 { 42 }")],
        Some(&result.staged_state),
    );
    assert!(retry.is_success(), "{:?}", retry.failed_files);
    assert_eq!(
        retry.staged_state.staged_files["b.rs"],
        "fn hello() -> i32 { 42 }\n"
    );
    assert!(retry.staged_state.deleted_paths.contains("a.rs"));
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "fn hello() {}\n"
    );
}

/// A refused rename restores both of its paths: the source content an earlier
/// member created in memory survives, and no deletion marker outlives the attempt.
#[test]
fn a_failed_move_keeps_the_source_content_a_previous_member_created() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // The destination already exists, so the rename is refused.
    std::fs::write(root.join("taken.rs"), "occupied\n").unwrap();
    let cm = ContextManager::new(root.to_path_buf());

    let edits = vec![
        EditOperation::Create {
            path: "draft.rs".into(),
            content: "pub fn draft() {}\n".into(),
        },
        EditOperation::Move {
            from: "draft.rs".into(),
            to: "taken.rs".into(),
        },
    ];
    let result = stage_and_validate_edits(root, &cm, &edits, None);

    assert_eq!(result.failed_files.len(), 1);
    assert_eq!(result.failed_files[0].path, "taken.rs");
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::DestinationExists(path) if path == "taken.rs"
    ));
    // The member that created the source applied cleanly and keeps its effect, and
    // the refused rename leaves no deletion marker behind for the source.
    assert_eq!(
        result.staged_state.staged_files["draft.rs"],
        "pub fn draft() {}\n"
    );
    assert!(!result.staged_state.deleted_paths.contains("draft.rs"));
    assert_eq!(result.successful_paths, vec!["draft.rs"]);
    assert_eq!(result.successful_edits, vec![edits[0].clone()]);
    assert_eq!(
        std::fs::read_to_string(root.join("taken.rs")).unwrap(),
        "occupied\n"
    );
    assert!(!root.join("draft.rs").exists());

    // The retry that renames the draft onto a free destination converges from the
    // state the failed attempt left behind.
    let retry = stage_and_validate_edits(
        root,
        &cm,
        &[EditOperation::Move {
            from: "draft.rs".into(),
            to: "renamed.rs".into(),
        }],
        Some(&result.staged_state),
    );
    assert!(retry.is_success(), "{:?}", retry.failed_files);
    assert_eq!(
        retry.staged_state.staged_files["renamed.rs"],
        "pub fn draft() {}\n"
    );
    assert!(!retry.staged_state.staged_files.contains_key("draft.rs"));
    assert!(retry.staged_state.deleted_paths.contains("draft.rs"));
}

/// A chained rename resolves its source from the state the attempt has built, so a
/// refused second hop must restore that intermediate state instead of the opening
/// one, where the intermediate path held nothing.
#[test]
fn a_refused_second_hop_restores_the_state_the_first_hop_produced() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "fn hello() {}\n").unwrap();
    // The destination of the second hop already exists, so that hop is refused.
    std::fs::write(root.join("c.rs"), "occupied\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let edits = vec![
        EditOperation::Move {
            from: "a.rs".into(),
            to: "b.rs".into(),
        },
        EditOperation::Move {
            from: "b.rs".into(),
            to: "c.rs".into(),
        },
    ];
    let result = stage_and_validate_edits(root, &cm, &edits, None);

    assert_eq!(result.failed_files.len(), 1);
    assert_eq!(result.failed_files[0].path, "c.rs");
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::DestinationExists(path) if path == "c.rs"
    ));
    assert_eq!(result.staged_state.staged_files["b.rs"], "fn hello() {}\n");
    assert!(result.staged_state.deleted_paths.contains("a.rs"));
    assert!(!result.staged_state.deleted_paths.contains("b.rs"));
    assert_eq!(result.successful_paths, vec!["a.rs", "b.rs"]);
    assert_eq!(result.successful_edits, vec![edits[0].clone()]);
    // The first hop lives in memory only, so the working tree is untouched.
    assert!(root.join("a.rs").exists());
    assert!(!root.join("b.rs").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("c.rs")).unwrap(),
        "occupied\n"
    );
}

#[test]
fn staging_preserves_crlf_and_empty_attempt_preserves_state() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "one\r\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();
    let result = stage_and_validate_edits(root, &cm, &[replace("a.rs", "one\n", "two\n")], None);
    assert!(result.is_success());
    assert_eq!(result.staged_state.staged_files["a.rs"], "two\r\n");
    let empty = stage_and_validate_edits(root, &cm, &[], Some(&result.staged_state));
    assert!(empty.is_success());
    assert_eq!(empty.staged_state, result.staged_state);
    assert!(empty.successful_edits.is_empty());
}

#[tokio::test]
async fn created_file_joins_the_auto_layer_not_the_user_layer() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut cm = ContextManager::new(root.to_path_buf());

    let proposal = EditProposal {
        summary: "create".into(),
        edits: vec![EditOperation::Create {
            path: "generated.rs".into(),
            content: "pub fn generated() {}\n".into(),
        }],
    };
    apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert!(cm.contains("generated.rs").unwrap());
    assert!(cm.is_editable("generated.rs").unwrap());

    // The turn only widens the transient auto layer: the user layer holds
    // exactly what the developer added by hand, so a file the model created
    // never appears there.
    assert_eq!(cm.auto_files(), vec!["generated.rs".to_string()]);
    let state = cm.get_state();
    let item = state
        .items
        .iter()
        .find(|item| item.path == "generated.rs")
        .expect("created file is in context");
    assert_eq!(item.layer, ContextLayer::Auto);

    // An explicit manual addition is the only path into the user layer.
    cm.add_file("generated.rs", ContextAccess::Editable)
        .unwrap();
    assert!(cm.auto_files().is_empty());
    let state = cm.get_state();
    let item = state
        .items
        .iter()
        .find(|item| item.path == "generated.rs")
        .expect("promoted file is in context");
    assert_eq!(item.layer, ContextLayer::User);
}

#[test]
fn ignored_paths_are_rejected_for_create_overwrite_and_move() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(root.join("scratch.log"), "old\n").unwrap();
    std::fs::write(root.join("source.rs"), "fn hello() {}\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file(".gitignore", ContextAccess::ReadOnly).unwrap();
    cm.add_file("scratch.log", ContextAccess::Editable).unwrap();
    cm.add_file("source.rs", ContextAccess::Editable).unwrap();

    let rejected = vec![
        EditOperation::Create {
            path: "target/generated.rs".into(),
            content: "fn generated() {}".into(),
        },
        EditOperation::Overwrite {
            path: "scratch.log".into(),
            content: "new\n".into(),
        },
        EditOperation::Move {
            from: "source.rs".into(),
            to: "target/moved.rs".into(),
        },
    ];
    for operation in rejected {
        let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&operation), None);
        assert!(
            !result.is_success(),
            "an ignored path must be rejected: {operation:?}"
        );
        assert!(matches!(
            &result.failed_files[0].error,
            EditError::IgnoredByGit(_)
        ));
        assert!(result.staged_state.staged_files.is_empty());
        assert!(result.staged_state.deleted_paths.is_empty());
    }

    // A path no ignore rule matches stages exactly as before.
    let allowed = EditOperation::Create {
        path: "src/generated.rs".into(),
        content: "fn generated() {}".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&allowed), None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["src/generated.rs"],
        "fn generated() {}"
    );

    // Nothing above reached disk.
    assert!(!root.join("target").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("scratch.log")).unwrap(),
        "old\n"
    );
    assert!(root.join("source.rs").exists());
}

#[test]
fn the_sandbox_admits_creates_and_move_destinations_git_ignores() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    // The harness keeps `.tauqe/` out of Git, and that exclusion is the
    // precondition of the sandbox: one ignore answer keeps a mockup out of
    // the change set and admits the write that creates it.
    crate::git::checkpoint::ensure_tauqe_ignored(root).unwrap();
    std::fs::write(root.join("source.rs"), "fn hello() {}\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("source.rs", ContextAccess::Editable).unwrap();

    let created = EditOperation::Create {
        path: ".tauqe/mockups/prototype.rs".into(),
        content: "pub fn prototype() {}\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&created), None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files[".tauqe/mockups/prototype.rs"],
        "pub fn prototype() {}\n"
    );

    let moved = EditOperation::Move {
        from: "source.rs".into(),
        to: ".tauqe/mockups/copied.rs".into(),
    };
    let result = stage_and_validate_edits(
        root,
        &cm,
        std::slice::from_ref(&moved),
        Some(&result.staged_state),
    );
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files[".tauqe/mockups/copied.rs"],
        "fn hello() {}\n"
    );
    assert!(result.staged_state.deleted_paths.contains("source.rs"));

    // A mockup is staged in memory like every other file.
    assert!(!root.join(".tauqe/mockups/prototype.rs").exists());
    assert!(!root.join(".tauqe/mockups/copied.rs").exists());
}

#[test]
fn a_mockup_git_does_not_ignore_is_refused() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    // No ignore rule covers `.tauqe/`, so Git would track the sandbox file
    // and the commit of the change set would fail on it.
    let cm = ContextManager::new(root.to_path_buf());

    let created = EditOperation::Create {
        path: ".tauqe/mockups/prototype.rs".into(),
        content: "pub fn prototype() {}\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&created), None);
    assert!(!result.is_success());
    assert_eq!(result.failed_files.len(), 1);
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::MockupsNotIgnored(path) if path == ".tauqe/mockups/prototype.rs"
    ));
    assert!(result.staged_state.staged_files.is_empty());
    assert!(!root.join(".tauqe/mockups/prototype.rs").exists());
}

#[test]
fn an_existing_mockup_is_edited_under_the_ordinary_context_rules() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    crate::git::checkpoint::ensure_tauqe_ignored(root).unwrap();
    std::fs::create_dir_all(root.join(".tauqe/mockups")).unwrap();
    std::fs::write(
        root.join(".tauqe/mockups/prototype.rs"),
        "pub fn prototype() -> i32 { 0 }\n",
    )
    .unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file(".tauqe/mockups/prototype.rs", ContextAccess::Editable)
        .unwrap();

    let replaced = stage_and_validate_edits(
        root,
        &cm,
        &[replace(
            ".tauqe/mockups/prototype.rs",
            "pub fn prototype() -> i32 { 0 }",
            "pub fn prototype() -> i32 { 42 }",
        )],
        None,
    );
    assert!(replaced.is_success(), "{:?}", replaced.failed_files);
    assert_eq!(
        replaced.staged_state.staged_files[".tauqe/mockups/prototype.rs"],
        "pub fn prototype() -> i32 { 42 }\n"
    );

    let rewritten = stage_and_validate_edits(
        root,
        &cm,
        &[EditOperation::Overwrite {
            path: ".tauqe/mockups/prototype.rs".into(),
            content: "pub fn prototype() -> i32 { 7 }\n".into(),
        }],
        Some(&replaced.staged_state),
    );
    assert!(rewritten.is_success(), "{:?}", rewritten.failed_files);
    assert_eq!(
        rewritten.staged_state.staged_files[".tauqe/mockups/prototype.rs"],
        "pub fn prototype() -> i32 { 7 }\n"
    );
    // The sandbox lives in memory until the turn commits, like every file.
    assert_eq!(
        std::fs::read_to_string(root.join(".tauqe/mockups/prototype.rs")).unwrap(),
        "pub fn prototype() -> i32 { 0 }\n"
    );
}

#[tokio::test]
async fn a_mockup_only_change_set_reaches_disk_without_joining_the_change_set() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    crate::git::checkpoint::ensure_tauqe_ignored(root).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());

    let proposal = EditProposal {
        summary: "Draft a prototype".into(),
        edits: vec![EditOperation::Create {
            path: ".tauqe/mockups/prototype.rs".into(),
            content: "pub fn prototype() -> i32 { 42 }\n".into(),
        }],
    };
    let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();

    assert!(
        changed.is_empty(),
        "the sandbox never joins the reported change set: {changed:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".tauqe/mockups/prototype.rs")).unwrap(),
        "pub fn prototype() -> i32 { 42 }\n",
        "the draft must still reach its sandbox file"
    );
    // The context reconciliation runs over the complete staged state, so the
    // draft stays visible to the model even though it never joins the change
    // set.
    assert!(cm.contains(".tauqe/mockups/prototype.rs").unwrap());
    assert!(cm.is_editable(".tauqe/mockups/prototype.rs").unwrap());
    assert_eq!(
        cm.auto_files(),
        vec![".tauqe/mockups/prototype.rs".to_string()]
    );
}

#[tokio::test]
async fn a_mixed_change_set_reports_only_the_tracked_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    crate::git::run_git(Some(root), &["init"]).unwrap();
    crate::git::checkpoint::ensure_tauqe_ignored(root).unwrap();
    std::fs::write(root.join("a.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let proposal = EditProposal {
        summary: "Prototype and edit".into(),
        edits: vec![
            EditOperation::Create {
                path: ".tauqe/mockups/prototype.rs".into(),
                content: "pub fn prototype() {}\n".into(),
            },
            replace(
                "a.rs",
                "pub fn value() -> i32 { 0 }",
                "pub fn value() -> i32 { 42 }",
            ),
        ],
    };
    let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();

    assert_eq!(changed, vec!["a.rs".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "pub fn value() -> i32 { 42 }\n"
    );
    assert!(
        root.join(".tauqe/mockups/prototype.rs").exists(),
        "the draft is written even though it stays out of the change set"
    );
}

/// Valid project configuration used by the configuration-validation tests.
const VALID_CONFIG: &str = "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"cargo\"\nargs = [\"test\"]\n";

#[test]
fn staged_project_config_syntax_error_is_rejected() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("tauqe.toml"), VALID_CONFIG).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("tauqe.toml", ContextAccess::Editable).unwrap();

    let broken = EditOperation::Overwrite {
        path: "tauqe.toml".into(),
        content: "[models\nprimary = ]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&broken), None);
    assert!(!result.is_success());
    match &result.failed_files[0].error {
        EditError::InvalidConfig { path, message } => {
            assert_eq!(path, "tauqe.toml");
            assert!(
                message.contains("not valid TOML"),
                "a syntactic rejection must name the TOML syntax: {message}"
            );
        }
        other => panic!("expected a syntactic configuration rejection, got {other:?}"),
    }
    assert!(
        result.staged_state.staged_files.is_empty(),
        "a rejected configuration must not reach the staged state"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tauqe.toml")).unwrap(),
        VALID_CONFIG
    );
}

#[test]
fn staged_project_config_without_verification_is_rejected() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("tauqe.toml"), VALID_CONFIG).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("tauqe.toml", ContextAccess::Editable).unwrap();

    // Syntactically valid, but the mandatory verification command is gone.
    let dropped_verify = EditOperation::Overwrite {
        path: "tauqe.toml".into(),
        content: "[models]\nprimary = [\"openrouter:a/b\"]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&dropped_verify), None);
    assert!(!result.is_success());
    match &result.failed_files[0].error {
        EditError::InvalidConfig { message, .. } => {
            assert!(
                message.contains("verify"),
                "a semantic rejection must name the missing invariant: {message}"
            );
        }
        other => panic!("expected a semantic configuration rejection, got {other:?}"),
    }
}

#[test]
fn staged_dot_prefixed_project_config_is_validated_too() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // The repository declares its configuration through the dot-prefixed
    // spelling that discovery also accepts.
    std::fs::write(root.join(".tauqe.toml"), VALID_CONFIG).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file(".tauqe.toml", ContextAccess::Editable).unwrap();

    let broken = EditOperation::Overwrite {
        path: ".tauqe.toml".into(),
        content: "[models\nprimary = ]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&broken), None);
    assert!(!result.is_success());
    match &result.failed_files[0].error {
        EditError::InvalidConfig { path, message } => {
            assert_eq!(path, ".tauqe.toml");
            assert!(
                message.contains("not valid TOML"),
                "a syntactic rejection must name the TOML syntax: {message}"
            );
        }
        other => panic!("expected a syntactic configuration rejection, got {other:?}"),
    }
    assert!(
        result.staged_state.staged_files.is_empty(),
        "a rejected configuration must not reach the staged state"
    );

    // The semantic layer guards the second spelling as well.
    let dropped_verify = EditOperation::Overwrite {
        path: ".tauqe.toml".into(),
        content: "[models]\nprimary = [\"openrouter:a/b\"]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&dropped_verify), None);
    assert!(!result.is_success());
    match &result.failed_files[0].error {
        EditError::InvalidConfig { message, .. } => {
            assert!(
                message.contains("verify"),
                "a semantic rejection must name the missing invariant: {message}"
            );
        }
        other => panic!("expected a semantic configuration rejection, got {other:?}"),
    }

    // A rewrite that preserves the invariants stages normally, and the
    // check itself never writes the configuration to disk.
    let extended = EditOperation::Overwrite {
        path: ".tauqe.toml".into(),
        content: "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"cargo\"\nargs = [\"test\", \"--workspace\"]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&extended), None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert!(result.staged_state.staged_files.contains_key(".tauqe.toml"));
    assert_eq!(
        std::fs::read_to_string(root.join(".tauqe.toml")).unwrap(),
        VALID_CONFIG,
        "staging must never write the configuration to disk"
    );
}

#[test]
fn staged_project_config_preserving_invariants_is_accepted() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("tauqe.toml"), VALID_CONFIG).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("tauqe.toml", ContextAccess::Editable).unwrap();

    let extended = EditOperation::Overwrite {
        path: "tauqe.toml".into(),
        content: "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"cargo\"\nargs = [\"test\", \"--workspace\"]\n".into(),
    };
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&extended), None);
    assert!(result.is_success());
    assert!(result.staged_state.staged_files.contains_key("tauqe.toml"));
    assert_eq!(
        std::fs::read_to_string(root.join("tauqe.toml")).unwrap(),
        VALID_CONFIG,
        "staging must never write the configuration to disk"
    );
}

/// Declares an in-memory project formatter running `script` under `sh`.
#[cfg(unix)]
fn declare_format_filter(root: &Path, script: &str) {
    std::fs::write(
        root.join("tauqe.toml"),
        format!(
            "[project.format]\ncommand = \"sh\"\nargs = [\"-c\", \"{}\"]\n",
            script
        ),
    )
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_formats_staged_content_without_touching_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    declare_format_filter(root, "tr a-z A-Z");
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let result = prepare_staged_edits(
        root,
        &cm,
        &[replace(
            "a.rs",
            "pub fn value() -> i32 { 0 }",
            "pub fn value() -> i32 { 42 }",
        )],
        None,
    )
    .await;

    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"],
        "PUB FN VALUE() -> I32 { 42 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "pub fn value() -> i32 { 0 }\n",
        "the staged content is formatted in memory, never on disk"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_keeps_the_authored_content_when_the_filter_fails() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    declare_format_filter(root, "printf 'formatter failed' >&2; exit 3");
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let result = prepare_staged_edits(
        root,
        &cm,
        &[replace(
            "a.rs",
            "pub fn value() -> i32 { 0 }",
            "pub fn value() -> i32 { 42 }",
        )],
        None,
    )
    .await;

    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"], "pub fn value() -> i32 { 42 }\n",
        "a failed filter must fall back to the authored content"
    );
}

#[tokio::test]
async fn prepare_keeps_the_authored_content_without_a_declared_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let result = prepare_staged_edits(
        root,
        &cm,
        &[replace(
            "a.rs",
            "pub fn value() -> i32 { 0 }",
            "pub fn value() -> i32 { 42 }",
        )],
        None,
    )
    .await;

    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"], "pub fn value() -> i32 { 42 }\n",
        "a repository without a filter keeps the authored content"
    );
}

/// Builds an anchored replacement of an inclusive line range.
fn anchored_range(path: &str, start: u16, end: u16, content: &str) -> EditOperation {
    EditOperation::LineAnchor {
        path: path.to_string(),
        start: anchor::encode_anchor(path, start),
        end: anchor::encode_anchor(path, end),
        content: content.to_string(),
    }
}

#[test]
fn anchored_ranges_stage_bottom_to_top_without_touching_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\ngamma\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    // The proposal names the lower range first, yet both ranges resolve
    // against one snapshot and apply from the bottom of the file upwards.
    let edits = vec![
        anchored_range("a.rs", 0, 0, "ALPHA\n"),
        anchored_range("a.rs", 2, 2, "GAMMA\n"),
    ];
    let result = stage_and_validate_edits(root, &cm, &edits, None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"],
        "ALPHA\nbeta\nGAMMA\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nbeta\ngamma\n",
        "staging must never write the anchored change set to disk"
    );
}

#[test]
fn anchored_range_collapses_into_a_single_replacement() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let edits = vec![anchored_range("a.rs", 1, 2, "middle\n")];
    let result = stage_and_validate_edits(root, &cm, &edits, None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"],
        "alpha\nmiddle\ndelta\n"
    );
}

#[test]
fn malformed_anchor_ranges_are_reported_for_the_retry_loop() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let beyond = vec![anchored_range("a.rs", 0, 5, "x\n")];
    let result = stage_and_validate_edits(root, &cm, &beyond, None);
    assert!(!result.is_success());
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::AnchorOutOfRange { lines: 2, .. }
    ));

    let overlapping = vec![
        anchored_range("a.rs", 0, 1, "x\n"),
        anchored_range("a.rs", 1, 1, "y\n"),
    ];
    let result = stage_and_validate_edits(root, &cm, &overlapping, None);
    assert!(!result.is_success());
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::OverlappingAnchorRanges { .. }
    ));
    assert!(result.staged_state.staged_files.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nbeta\n"
    );
}

#[tokio::test]
async fn anchored_change_set_applies_to_disk_and_stays_editable() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\ngamma\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let proposal = EditProposal {
        summary: "anchored edit".into(),
        edits: vec![anchored_range("a.rs", 1, 1, "BETA\n")],
    };
    let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert_eq!(changed, vec!["a.rs"]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );
    assert!(cm.is_editable("a.rs").unwrap());
}
/// A replacement that spans the whole file narrows to the lines it changes.
///
/// A model that rewrites a file from its first anchor to its last one resends
/// every unchanged line around the edit. Staging compares the shared leading and
/// trailing lines byte for byte and drops them, so the change set keeps the lines
/// that really differ instead of rewriting the addressed range verbatim.
#[tokio::test]
async fn a_whole_file_anchored_range_narrows_to_the_changed_lines() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let original = "alpha\nbeta\ngamma\ndelta\n";
    std::fs::write(root.join("a.rs"), original).unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    // The range names the whole file, and only `beta` and `delta` differ from
    // the snapshot both anchors address.
    let widened = anchored_range("a.rs", 0, 3, "alpha\nBETA\ngamma\nDELTA\n");
    let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&widened), None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"],
        "alpha\nBETA\ngamma\nDELTA\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        original,
        "staging must never write the narrowed change set to disk"
    );

    let proposal = EditProposal {
        summary: "Rewrite the file".into(),
        edits: vec![widened],
    };
    let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
    assert_eq!(changed, vec!["a.rs"]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nBETA\ngamma\nDELTA\n"
    );
    assert!(cm.is_editable("a.rs").unwrap());
}

/// Trimming narrows every range before the change set is checked for overlap.
///
/// The first range changes `beta` and resends `gamma` byte for byte, while the
/// second range replaces `gamma`. Without trimming the two ranges would overlap
/// and the whole change set would be rejected; after it they are adjacent.
#[test]
fn a_padded_range_never_collides_with_a_range_it_only_resent_unchanged() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    let padded = anchored_range("a.rs", 0, 2, "alpha\nBETA\ngamma\n");
    let tail = anchored_range("a.rs", 2, 2, "GAMMA\n");
    let result = stage_and_validate_edits(root, &cm, &[padded, tail], None);
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["a.rs"],
        "alpha\nBETA\nGAMMA\ndelta\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nbeta\ngamma\ndelta\n",
        "staging must never write the narrowed change set to disk"
    );
}
#[test]
fn replace_without_a_needle_is_reported_as_a_typed_rejection() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    // A replacement that echoes no text addresses no line of the file, so
    // the failure is carried as the typed rejection instead of a bare match
    // miss: the active patch strategy decides how the correction is
    // explained to the model.
    for needle in ["", "  \n\t "] {
        let result = stage_and_validate_edits(root, &cm, &[replace("a.rs", needle, "x")], None);
        assert!(!result.is_success());
        assert!(matches!(
            &result.failed_files[0].error,
            EditError::ReplaceWithoutRange { path } if path == "a.rs"
        ));
    }
    assert_eq!(
        std::fs::read_to_string(root.join("a.rs")).unwrap(),
        "alpha\nbeta\n",
        "a rejected replacement must never reach disk"
    );
}

/// Every surface that reports a rejected change set speaks the dialect of
/// the declared patch strategy.
///
/// A replacement that echoes no text is refused by staging as the typed
/// [`EditError::ReplaceWithoutRange`], so the retry reason — the text the
/// retrying event shows and the retry request repeats — describes it in the
/// vocabulary the turn actually uses. An anchored project is therefore
/// never told about `old_text`, while a search/replace project keeps the
/// field name its own instructions declare.
#[test]
fn rejected_change_set_reaches_the_model_in_the_active_strategy_dialect() {
    use std::collections::{HashMap, HashSet};
    use tauqe_protocol::PatchStrategy;

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "alpha\nbeta\n").unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("a.rs", ContextAccess::Editable).unwrap();

    // A replacement that echoes no text addresses no line of the file, so
    // the failure is the typed rejection instead of a bare match miss.
    let result = stage_and_validate_edits(root, &cm, &[replace("a.rs", "  \n\t ", "x")], None);
    let Some(FailedFileEdit { path, error }) = result.failed_files.into_iter().next() else {
        panic!("a rangeless replacement must be rejected");
    };
    assert!(
        matches!(&error, EditError::ReplaceWithoutRange { path: rejected } if rejected == &path),
        "{error:?}"
    );

    let failed = HashMap::from([(path.clone(), error)]);
    let anchored_reason = crate::workflow::retry::format_patch_retry_reason(
        &failed[&path],
        PatchStrategy::LineAnchor,
    );
    assert!(
        anchored_reason.contains("anchors"),
        "the anchored rejection must point at the anchors: {anchored_reason}"
    );
    assert!(
        anchored_reason.contains("start"),
        "the anchored rejection must offer the whole-file range: {anchored_reason}"
    );
    assert!(
        !anchored_reason.contains("overwrite"),
        "the anchored rejection must not name the withdrawn operation: {anchored_reason}"
    );
    assert!(
        !anchored_reason.contains("old_text"),
        "the anchored rejection named a foreign field: {anchored_reason}"
    );

    let echoed_reason = crate::workflow::retry::format_patch_retry_reason(
        &failed[&path],
        PatchStrategy::SearchReplace,
    );
    assert!(
        echoed_reason.contains("old_text"),
        "the search/replace rejection must keep its own field name: {echoed_reason}"
    );

    // The retry request repeats the very reason the retrying event shows.
    let anchored_prompt = crate::workflow::retry::build_patch_retry_prompt(
        &HashSet::new(),
        &failed,
        &[],
        None,
        PatchStrategy::LineAnchor,
    );
    assert!(
        anchored_prompt.contains(&anchored_reason),
        "{anchored_prompt}"
    );
    for foreign in ["old_text", "new_text", "Search block"] {
        assert!(
            !anchored_prompt.contains(foreign),
            "the anchored retry request named {foreign}: {anchored_prompt}"
        );
    }

    let echoed_prompt = crate::workflow::retry::build_patch_retry_prompt(
        &HashSet::new(),
        &failed,
        &[],
        None,
        PatchStrategy::SearchReplace,
    );
    assert!(echoed_prompt.contains(&echoed_reason), "{echoed_prompt}");
}

/// Declares the protected patterns of the repository under test.
fn declare_protected_patterns(root: &Path, patterns: &[&str]) {
    let declared = patterns
        .iter()
        .map(|pattern| format!("\"{pattern}\""))
        .collect::<Vec<String>>()
        .join(", ");
    std::fs::write(
        root.join("tauqe.toml"),
        format!("[project.boundaries]\nprotected = [{declared}]\n"),
    )
    .unwrap();
}

#[test]
fn every_operation_on_a_protected_path_is_rejected() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Justfile"), "check:\n\tcargo check\n").unwrap();
    std::fs::create_dir_all(root.join("docs/specs")).unwrap();
    std::fs::write(root.join("docs/specs/auth.md"), "# Auth spec\n").unwrap();
    std::fs::write(root.join("src.rs"), "pub fn value() {}\n").unwrap();
    declare_protected_patterns(root, &["Justfile", "docs/specs/**"]);
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("Justfile", ContextAccess::Editable).unwrap();
    cm.add_file("docs/specs/auth.md", ContextAccess::Editable)
        .unwrap();
    cm.add_file("src.rs", ContextAccess::Editable).unwrap();

    let rejected = vec![
        replace("Justfile", "cargo check", "cargo test"),
        EditOperation::Create {
            path: "docs/specs/new.md".into(),
            content: "# New spec\n".into(),
        },
        EditOperation::Overwrite {
            path: "docs/specs/auth.md".into(),
            content: "# Rewritten spec\n".into(),
        },
        EditOperation::Delete {
            path: "Justfile".into(),
        },
        anchored_range("docs/specs/auth.md", 0, 0, "# Rewritten spec\n"),
        EditOperation::Move {
            from: "Justfile".into(),
            to: "Justfile.bak".into(),
        },
        EditOperation::Move {
            from: "src.rs".into(),
            to: "docs/specs/src.rs".into(),
        },
    ];

    for operation in rejected {
        let result = stage_and_validate_edits(root, &cm, std::slice::from_ref(&operation), None);
        assert!(
            !result.is_success(),
            "a protected path must be rejected: {operation:?}"
        );
        assert_eq!(result.failed_files.len(), 1, "{operation:?}");
        assert!(
            matches!(&result.failed_files[0].error, EditError::ProtectedPath(_)),
            "{operation:?} produced {:?}",
            result.failed_files[0].error
        );
        assert!(result.staged_state.staged_files.is_empty());
        assert!(result.staged_state.deleted_paths.is_empty());
    }

    // Nothing above reached disk.
    assert_eq!(
        std::fs::read_to_string(root.join("Justfile")).unwrap(),
        "check:\n\tcargo check\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("docs/specs/auth.md")).unwrap(),
        "# Auth spec\n"
    );
    assert!(root.join("src.rs").exists());
    assert!(!root.join("Justfile.bak").exists());
    assert!(!root.join("docs/specs/new.md").exists());
    assert!(!root.join("docs/specs/src.rs").exists());
}

#[test]
fn paths_outside_the_declared_boundaries_stage_normally() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Justfile"), "check:\n").unwrap();
    std::fs::write(root.join("src.rs"), "pub fn value() {}\n").unwrap();
    declare_protected_patterns(root, &["Justfile", "docs/specs/**"]);
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("Justfile", ContextAccess::Editable).unwrap();
    cm.add_file("src.rs", ContextAccess::Editable).unwrap();

    let result = stage_and_validate_edits(
        root,
        &cm,
        &[replace(
            "src.rs",
            "pub fn value() {}",
            "pub fn value() -> i32 { 0 }",
        )],
        None,
    );
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        result.staged_state.staged_files["src.rs"],
        "pub fn value() -> i32 { 0 }\n"
    );
}

#[test]
fn a_repository_without_a_boundary_declaration_protects_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Justfile"), "check:\n").unwrap();
    // Verification is declared, the boundary subsection is not: every path
    // stays editable.
    std::fs::write(
        root.join("tauqe.toml"),
        "[project.verify]\ncommand = \"cargo\"\nargs = [\"test\"]\n",
    )
    .unwrap();
    let mut cm = ContextManager::new(root.to_path_buf());
    cm.add_file("Justfile", ContextAccess::Editable).unwrap();

    let result = stage_and_validate_edits(
        root,
        &cm,
        &[replace("Justfile", "check:", "check: fmt")],
        None,
    );
    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(result.staged_state.staged_files["Justfile"], "check: fmt\n");
}

#[test]
fn the_boundary_rejection_reaches_the_retry_request_verbatim() {
    use std::collections::{HashMap, HashSet};
    use tauqe_protocol::PatchStrategy;

    let error = EditError::ProtectedPath("docs/specs/auth.md".to_string());
    let message = error.to_string();
    assert!(
        message.contains("[project.boundaries.protected]"),
        "{message}"
    );
    assert!(message.contains("docs/specs/auth.md"), "{message}");

    let failed = HashMap::from([("docs/specs/auth.md".to_string(), error)]);
    for strategy in [PatchStrategy::SearchReplace, PatchStrategy::LineAnchor] {
        // A boundary rejection carries no strategy vocabulary, so both
        // dialects repeat the message unchanged.
        let reason = crate::workflow::retry::format_patch_retry_reason(
            &failed["docs/specs/auth.md"],
            strategy,
        );
        assert_eq!(reason, message, "{strategy} reworded a boundary rejection");
        let prompt = crate::workflow::retry::build_patch_retry_prompt(
            &HashSet::new(),
            &failed,
            &[],
            None,
            strategy,
        );
        assert!(prompt.contains(&message), "{strategy}: {prompt}");
    }
}
