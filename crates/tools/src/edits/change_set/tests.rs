//! Behavioural coverage of the virtual-staging state machine.
//!
//! The cases concentrate on the admission answers a change set receives while it
//! is still purely in memory, and on the state its members leave behind once they
//! applied. Every fixture is a real temporary repository, because the batched
//! ignore probe and the directory-move expansion both read the repository through
//! Git.

use std::path::Path;

use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

use super::*;

/// Creates a temporary repository the batched ignore probe can inspect.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(dir.path())
        .status()
        .expect("git is available");
    assert!(status.success(), "git init failed");
    dir
}

/// Writes `content` to the repository-relative `path`, creating its parents.
fn write(repo_root: &Path, path: &str, content: &str) {
    let full = repo_root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

/// Records every file of the working tree in the Git index.
///
/// A directory move expands into the tracked files of its source, so a fixture
/// has to reach the index before the change set stages.
fn track_all(repo_root: &Path) {
    let status = std::process::Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(repo_root)
        .status()
        .expect("git is available");
    assert!(status.success(), "git add -A failed");
}

/// Builds the context snapshot that holds `paths` as editable files.
fn context_state(paths: &[&str]) -> ContextState {
    ContextState {
        revision: 1,
        items: paths
            .iter()
            .enumerate()
            .map(|(index, path)| ContextItem {
                path: (*path).to_string(),
                access: ContextAccess::Editable,
                size_bytes: 0,
                estimated_tokens: 1,
                layer: ContextLayer::User,
                insertion_seq: index as u64 + 1,
            })
            .collect(),
        total_estimated_tokens: paths.len() as u64,
    }
}

/// A destination the same change set deletes is free, so the coherent sequence
/// "delete `b`, then move `a` to `b`" stages instead of being refused on a file
/// only the working tree still holds.
#[test]
fn a_rename_takes_a_destination_the_same_change_set_deletes() {
    let dir = repository();
    let root = dir.path();
    write(root, "a.txt", "from a\n");
    write(root, "b.txt", "from b\n");

    let edits = [
        EditOperation::Delete {
            path: "b.txt".to_string(),
        },
        EditOperation::Move {
            from: "a.txt".to_string(),
            to: "b.txt".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["a.txt", "b.txt"]),
        &edits,
        None,
    );

    assert!(
        result.is_success(),
        "the change set stages: {:?}",
        result.failed_files
    );
    assert_eq!(
        result
            .staged_state
            .staged_files
            .get("b.txt")
            .map(String::as_str),
        Some("from a\n"),
        "the destination carries the moved content"
    );
    assert!(!result.staged_state.deleted_paths.contains("b.txt"));
    assert!(
        result.staged_state.deleted_paths.contains("a.txt"),
        "the source removal is recorded"
    );
    assert!(!result.staged_state.staged_files.contains_key("a.txt"));
    // Staging is memory only, so the working tree keeps what it held.
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "from b\n"
    );
}

/// A directory move asks the same question about every destination it expands
/// into, so a destination the change set deletes is free for it as well.
#[test]
fn a_directory_move_takes_a_destination_the_same_change_set_deletes() {
    let dir = repository();
    let root = dir.path();
    write(root, "src/old/moved.rs", "fn moved() {}\n");
    write(root, "src/new/moved.rs", "fn stale() {}\n");
    track_all(root);

    let edits = [
        EditOperation::Delete {
            path: "src/new/moved.rs".to_string(),
        },
        EditOperation::MoveDir {
            from: "src/old".to_string(),
            to: "src/new".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["src/new/moved.rs"]),
        &edits,
        None,
    );

    assert!(
        result.is_success(),
        "the change set stages: {:?}",
        result.failed_files
    );
    assert_eq!(
        result
            .staged_state
            .staged_files
            .get("src/new/moved.rs")
            .map(String::as_str),
        Some("fn moved() {}\n"),
        "the destination carries the moved content"
    );
    assert!(!result
        .staged_state
        .deleted_paths
        .contains("src/new/moved.rs"));
    assert!(
        result
            .staged_state
            .deleted_paths
            .contains("src/old/moved.rs"),
        "the source removal is recorded"
    );
}

/// A rename still never overwrites: a destination the change set leaves alone is
/// refused even though the working tree is the only place that holds it.
#[test]
fn a_rename_onto_an_undeleted_existing_path_is_refused() {
    let dir = repository();
    let root = dir.path();
    write(root, "a.txt", "from a\n");
    write(root, "b.txt", "from b\n");

    let edits = [EditOperation::Move {
        from: "a.txt".to_string(),
        to: "b.txt".to_string(),
    }];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["a.txt"]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    let failure = result.failed_files.first().expect("the rename is refused");
    assert_eq!(failure.path, "b.txt");
    match &failure.error {
        EditError::DestinationExists(path) => assert_eq!(path, "b.txt"),
        other => panic!("expected a destination rejection, got {other:?}"),
    }
    // The refused member restores the state it found.
    assert!(!result.staged_state.deleted_paths.contains("a.txt"));
    assert!(!result.staged_state.staged_files.contains_key("b.txt"));
}

/// A directory move over a large subtree stages in one pass per member and
/// reports every path it relocated exactly once. The member's path list is
/// resolved once for the attempt, so the accounting answers for it from that
/// list instead of rescanning everything it already reported.
#[test]
fn a_large_directory_move_reports_every_relocated_path_once() {
    let dir = repository();
    let root = dir.path();
    const FILES: usize = 400;
    for index in 0..FILES {
        write(
            root,
            &format!("src/old/module_{index:03}.rs"),
            "fn moved() {}\n",
        );
    }
    track_all(root);

    let edits = [EditOperation::MoveDir {
        from: "src/old".to_string(),
        to: "src/new".to_string(),
    }];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        &edits,
        None,
    );

    assert!(
        result.is_success(),
        "the move stages: {:?}",
        result.failed_files
    );
    assert_eq!(result.staged_state.staged_files.len(), FILES);
    assert_eq!(
        result.staged_state.deleted_paths.len(),
        FILES,
        "every source removal is recorded"
    );
    // The member addresses both ends of every pair it relocates, yet each path
    // reaches the report exactly once.
    let reported: HashSet<&str> = result.successful_paths.iter().map(String::as_str).collect();
    assert_eq!(reported.len(), result.successful_paths.len());
    assert_eq!(result.successful_paths.len(), FILES * 2);

    for index in 0..FILES {
        let source = format!("src/old/module_{index:03}.rs");
        let destination = format!("src/new/module_{index:03}.rs");
        assert_eq!(
            result
                .staged_state
                .staged_files
                .get(&destination)
                .map(String::as_str),
            Some("fn moved() {}\n"),
            "the destination carries the moved content"
        );
        assert!(result.staged_state.deleted_paths.contains(&source));
        assert!(reported.contains(destination.as_str()));
    }
}
