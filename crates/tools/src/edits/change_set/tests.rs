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
    write_bytes(repo_root, path, content.as_bytes());
}

/// Writes raw bytes to the repository-relative `path`, creating its parents.
///
/// A payload that does not decode as UTF-8 proves that a relocation carries
/// bytes verbatim, because no text pipeline stage can read such a file.
fn write_bytes(repo_root: &Path, path: &str, content: &[u8]) {
    let full = repo_root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

/// Creates a symbolic link whose target does not exist.
///
/// Git records a symbolic link as the text of its target, so the link is a
/// tracked file of the subtree while the path it names does not exist. Such a
/// source cannot be relocated, which makes it the deterministic refusal a
/// directory move raises after it already transferred an earlier file.
#[cfg(unix)]
fn write_dangling_symlink(repo_root: &Path, path: &str, target: &str) {
    let full = repo_root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::os::unix::fs::symlink(target, full).unwrap();
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

/// Returns the exact staged bytes of `path`.
///
/// Staging keeps bytes, so a fixture compares the payload it expects verbatim
/// instead of decoding the staged content as text.
fn staged_bytes<'a>(result: &'a StageEditsResult, path: &str) -> &'a [u8] {
    result
        .staged_state
        .staged_files
        .get(path)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("'{path}' is not staged"))
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
        staged_bytes(&result, "b.txt"),
        &b"from a\n"[..],
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
        staged_bytes(&result, "src/new/moved.rs"),
        &b"fn moved() {}\n"[..],
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

/// A refused directory move inverts every pair it already relocated, so the
/// attempt leaves exactly the state its member found: no content stranded at a
/// destination, no source left recorded as deleted, and a destination the same
/// change set deleted keeps its deletion.
#[cfg(unix)]
#[test]
fn a_refused_directory_move_restores_exactly_the_addressed_paths() {
    let dir = repository();
    let root = dir.path();
    write(root, "src/old/a.rs", "fn a() {}\n");
    // Git tracks a symbolic link as the text of its target, so this source is
    // part of the subtree while the path it names does not exist. It refuses
    // after `src/old/a.rs` already moved to the new directory.
    write_dangling_symlink(root, "src/old/b.rs", "../missing.rs");
    write(root, "src/new/a.rs", "fn stale() {}\n");
    track_all(root);

    let edits = [
        EditOperation::Delete {
            path: "src/new/a.rs".to_string(),
        },
        EditOperation::MoveDir {
            from: "src/old".to_string(),
            to: "src/new".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["src/new/a.rs"]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    let failure = result.failed_files.first().expect("the move is refused");
    assert_eq!(failure.path, "src/old/b.rs");
    assert!(
        matches!(&failure.error, EditError::FileNotFound(path) if path == "src/old/b.rs"),
        "unexpected rejection: {:?}",
        failure.error
    );
    assert!(
        result.staged_state.staged_files.is_empty(),
        "no content stays at a destination"
    );
    assert_eq!(
        result.staged_state.deleted_paths,
        HashSet::from(["src/new/a.rs".to_string()]),
        "only the deletion of the earlier member survives"
    );
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
            staged_bytes(&result, &destination),
            &b"fn moved() {}\n"[..],
            "the destination carries the moved content"
        );
        assert!(result.staged_state.deleted_paths.contains(&source));
        assert!(
            !result.staged_state.staged_files.contains_key(&source),
            "the source keeps no staged content of its own"
        );
        assert!(reported.contains(destination.as_str()));
    }
}

/// A member refused while it applied claims every path it addressed, so a later
/// member touching one of those paths is refused for the same reason and
/// reported instead of vanishing from the result.
#[test]
fn a_member_touching_a_refused_path_is_reported_with_the_earlier_reason() {
    let dir = repository();
    let root = dir.path();
    write(root, "a.txt", "from a\n");
    write(root, "b.txt", "from b\n");

    let edits = [
        // `b.txt` is taken, so the rename is refused and claims both sides.
        EditOperation::Move {
            from: "a.txt".to_string(),
            to: "b.txt".to_string(),
        },
        // The later member addresses the refused source, so it is refused too.
        EditOperation::Replace {
            path: "a.txt".to_string(),
            old_text: "from a\n".to_string(),
            new_text: "edited\n".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["a.txt", "b.txt"]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        2,
        "the suppressed member is reported: {:?}",
        result.failed_files
    );
    let failure = result.failed_files.first().expect("the rename is refused");
    assert_eq!(failure.path, "b.txt");
    match &failure.error {
        EditError::DestinationExists(path) => assert_eq!(path, "b.txt"),
        other => panic!("expected a destination rejection, got {other:?}"),
    }
    let suppressed = result
        .failed_files
        .get(1)
        .expect("the suppressed member is reported");
    assert_eq!(suppressed.path, "a.txt");
    match &suppressed.error {
        EditError::InvalidPath(path, message) => {
            assert_eq!(path, "a.txt");
            assert!(message.contains("was not applied"), "{message}");
            assert!(
                message.contains("already exists"),
                "the reason names the earlier rejection: {message}"
            );
        }
        other => panic!("expected the suppression to be reported, got {other:?}"),
    }
    // The refused rename left no trace in the staged state.
    assert!(!result.staged_state.deleted_paths.contains("a.txt"));
    assert!(!result.staged_state.staged_files.contains_key("b.txt"));
    assert!(result.successful_paths.is_empty());
}

/// A member refused before the attempt starts claims the same paths as one the
/// state machine rejects, so a later member touching the side that did
/// normalize is refused instead of applying against a half-read change set.
#[test]
fn a_refused_rename_claims_the_side_that_normalized() {
    let dir = repository();
    let root = dir.path();
    write(root, "a.txt", "from a\n");

    let edits = [
        // The destination escapes the repository, so the rename never becomes
        // a member, yet both of its sides are claimed.
        EditOperation::Move {
            from: "a.txt".to_string(),
            to: "../outside.txt".to_string(),
        },
        EditOperation::Replace {
            path: "a.txt".to_string(),
            old_text: "from a\n".to_string(),
            new_text: "edited\n".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["a.txt"]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        2,
        "the suppressed member is reported: {:?}",
        result.failed_files
    );
    let failure = result.failed_files.first().expect("the rename is refused");
    assert_eq!(failure.path, "../outside.txt");
    assert!(
        matches!(&failure.error, EditError::InvalidPath(path, _) if path == "../outside.txt"),
        "unexpected rejection: {:?}",
        failure.error
    );
    let suppressed = result
        .failed_files
        .get(1)
        .expect("the suppressed member is reported");
    assert_eq!(suppressed.path, "a.txt");
    assert!(
        matches!(
            &suppressed.error,
            EditError::InvalidPath(path, message)
                if path == "a.txt" && message.contains("was not applied")
        ),
        "unexpected rejection: {:?}",
        suppressed.error
    );
    assert!(result.staged_state.staged_files.is_empty());
    assert!(result.successful_paths.is_empty());
}

/// A move carries the payload of a source this attempt has not staged, so a
/// file that does not decode as text relocates byte for byte instead of being
/// refused on every retry.
#[test]
fn a_move_carries_a_binary_source_verbatim() {
    let dir = repository();
    let root = dir.path();
    let binary: &[u8] = &[0x89, b'P', b'N', b'G', 0x00, 0xff];
    write_bytes(root, "assets/logo.png", binary);

    let edits = [EditOperation::Move {
        from: "assets/logo.png".to_string(),
        to: "assets/brand/logo.png".to_string(),
    }];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["assets/logo.png"]),
        &edits,
        None,
    );

    assert!(
        result.is_success(),
        "the move stages: {:?}",
        result.failed_files
    );
    assert_eq!(
        staged_bytes(&result, "assets/brand/logo.png"),
        binary,
        "the destination carries the moved bytes"
    );
    assert!(!result
        .staged_state
        .staged_files
        .contains_key("assets/logo.png"));
    assert!(
        result
            .staged_state
            .deleted_paths
            .contains("assets/logo.png"),
        "the source removal is recorded"
    );
    // Staging is memory only, so the working tree keeps what it held.
    assert_eq!(std::fs::read(root.join("assets/logo.png")).unwrap(), binary);
    assert!(!root.join("assets/brand/logo.png").exists());
}

/// A directory move relocates every tracked file of its subtree, the members
/// that do not decode as text included.
#[test]
fn a_directory_move_relocates_a_binary_member_verbatim() {
    let dir = repository();
    let root = dir.path();
    let binary: &[u8] = &[0xff, 0x00, 0xfe, b'a'];
    write(root, "src/old/a.rs", "fn a() {}\n");
    write_bytes(root, "src/old/logo.bin", binary);
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
    assert_eq!(staged_bytes(&result, "src/new/a.rs"), &b"fn a() {}\n"[..]);
    assert_eq!(
        staged_bytes(&result, "src/new/logo.bin"),
        binary,
        "the relocated payload keeps its bytes"
    );
    assert!(result.staged_state.deleted_paths.contains("src/old/a.rs"));
    assert!(result
        .staged_state
        .deleted_paths
        .contains("src/old/logo.bin"));
    assert_eq!(result.staged_state.staged_files.len(), 2);
    // The subtree is relocated in memory only, so the working tree keeps both
    // sources.
    assert!(root.join("src/old/logo.bin").exists());
    assert!(!root.join("src/new").exists());
}
/// A directory move relocates every tracked member of its subtree, including
/// the names Git escapes in a line-oriented listing.
#[test]
fn a_directory_move_relocates_a_quoting_sensitive_member() {
    let dir = repository();
    let root = dir.path();
    let names = ["plain.rs", "quote\"d.rs", "back\\slash.rs", "привет.rs"];
    for name in names {
        write(root, &format!("src/old/{name}"), "fn moved() {}\n");
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
    assert_eq!(result.staged_state.staged_files.len(), names.len());
    for name in names {
        let source = format!("src/old/{name}");
        let destination = format!("src/new/{name}");
        assert_eq!(
            staged_bytes(&result, &destination),
            &b"fn moved() {}\n"[..],
            "the destination carries the moved content for {name:?}"
        );
        assert!(
            result.staged_state.deleted_paths.contains(&source),
            "the source removal is recorded for {name:?}"
        );
    }
    // The subtree is relocated in memory only, so the working tree keeps every
    // source.
    assert!(root.join("src/old/plain.rs").exists());
    assert!(!root.join("src/new").exists());
}

/// A relocation carries bytes while every text operation stays text: a
/// replacement that addresses a binary moved destination is refused with the
/// strict-decode classification, the move keeps its effect, and the refusal
/// claims exactly the path it addressed.
#[test]
fn a_replacement_addressing_a_binary_move_destination_is_refused() {
    let dir = repository();
    let root = dir.path();
    let binary: &[u8] = &[0xff, 0xfe, 0x00];
    write_bytes(root, "assets/logo.bin", binary);
    write(root, "assets/notes.txt", "notes\n");

    let edits = [
        EditOperation::Move {
            from: "assets/logo.bin".to_string(),
            to: "assets/brand/logo.bin".to_string(),
        },
        EditOperation::Replace {
            path: "assets/brand/logo.bin".to_string(),
            old_text: "anything".to_string(),
            new_text: "replacement".to_string(),
        },
        EditOperation::Delete {
            path: "assets/notes.txt".to_string(),
        },
        EditOperation::Overwrite {
            path: "assets/brand/logo.bin".to_string(),
            content: "text".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["assets/logo.bin", "assets/notes.txt"]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        2,
        "the suppressed member is reported: {:?}",
        result.failed_files
    );
    let failure = result
        .failed_files
        .first()
        .expect("the replacement is refused");
    assert_eq!(failure.path, "assets/brand/logo.bin");
    match &failure.error {
        EditError::Io(path, source) => {
            assert_eq!(path, "assets/brand/logo.bin");
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                source.to_string().contains("valid UTF-8"),
                "the refusal must carry the standard message: {source}"
            );
        }
        other => panic!("expected the strict-decode refusal, got {other:?}"),
    }
    let suppressed = result
        .failed_files
        .get(1)
        .expect("the suppressed member is reported");
    assert_eq!(suppressed.path, "assets/brand/logo.bin");
    match &suppressed.error {
        EditError::InvalidPath(path, message) => {
            assert_eq!(path, "assets/brand/logo.bin");
            assert!(message.contains("was not applied"), "{message}");
            assert!(
                message.contains("valid UTF-8"),
                "the reason names the earlier rejection: {message}"
            );
        }
        other => panic!("expected the suppression to be reported, got {other:?}"),
    }
    // The move applied and keeps its effect, and the refusal claims no path it
    // did not address: the deletion of another file still applies.
    assert_eq!(staged_bytes(&result, "assets/brand/logo.bin"), binary);
    assert!(result
        .staged_state
        .deleted_paths
        .contains("assets/logo.bin"));
    assert!(result
        .staged_state
        .deleted_paths
        .contains("assets/notes.txt"));
    assert_eq!(
        result.successful_paths,
        vec![
            "assets/logo.bin",
            "assets/brand/logo.bin",
            "assets/notes.txt"
        ]
    );
}

/// The text view of the staged state is lossless: it answers exactly the
/// entries that decode as UTF-8 and omits every other payload.
#[test]
fn the_staged_text_view_omits_entries_that_do_not_decode() {
    let mut staged = StagedEditsState::default();
    staged
        .staged_files
        .insert("src/lib.rs".to_string(), b"pub fn value() {}\n".to_vec());
    staged
        .staged_files
        .insert("src/empty.rs".to_string(), Vec::new());
    staged
        .staged_files
        .insert("assets/logo.bin".to_string(), vec![0xff, 0x00, 0xfe]);
    staged.deleted_paths.insert("src/gone.rs".to_string());

    let text = staged.staged_text_files();

    assert_eq!(
        text.get("src/lib.rs").map(String::as_str),
        Some("pub fn value() {}\n")
    );
    assert_eq!(text.get("src/empty.rs").map(String::as_str), Some(""));
    assert!(
        !text.contains_key("assets/logo.bin"),
        "a payload that is not UTF-8 is absent from the text view"
    );
    assert!(!text.contains_key("src/gone.rs"));
    assert_eq!(
        staged.staged_files.len(),
        3,
        "the text view never mutates the state"
    );
}

/// A replacement addresses text, so binary staged content refuses the member
/// with the very classification a disk read of those bytes produces.
#[test]
fn a_replacement_addressing_binary_staged_content_is_refused() {
    let dir = repository();
    let root = dir.path();
    let binary = vec![0xff, 0xfe, 0x00];
    write_bytes(root, "assets/logo.bin", &binary);
    let mut staged = StagedEditsState::default();
    staged
        .staged_files
        .insert("assets/logo.bin".to_string(), binary);

    let edits = [EditOperation::Replace {
        path: "assets/logo.bin".to_string(),
        old_text: "anything".to_string(),
        new_text: "replacement".to_string(),
    }];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&["assets/logo.bin"]),
        &edits,
        Some(&staged),
    );

    assert!(!result.is_success());
    let failure = result
        .failed_files
        .first()
        .expect("the replacement is refused");
    assert_eq!(failure.path, "assets/logo.bin");
    match &failure.error {
        EditError::Io(path, source) => {
            assert_eq!(path, "assets/logo.bin");
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                source.to_string().contains("valid UTF-8"),
                "the refusal must carry the standard message: {source}"
            );
        }
        other => panic!("expected the strict-decode refusal, got {other:?}"),
    }
    assert_eq!(
        result.staged_state, staged,
        "the refused member leaves the staged state untouched"
    );
}

/// Creates a repository whose index Git cannot read.
///
/// `git init` followed by `git add -A` writes the index, and overwriting it
/// makes every command that consults it fail. `git rev-parse` never reads the
/// index, so the directory stays an inspectable work tree: the failure is a
/// refused probe rather than an uninspectable repository.
fn repository_with_unreadable_index() -> tempfile::TempDir {
    let dir = repository();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n.tauqe/\n");
    track_all(root);
    std::fs::write(root.join(".git/index"), b"corrupt").unwrap();
    dir
}

/// A probe Git refused refuses the write instead of admitting it: only a target
/// Git cannot inspect degrades to admitting every path.
#[test]
fn a_refused_ignore_probe_refuses_a_create() {
    let dir = repository_with_unreadable_index();
    let root = dir.path();

    let created = EditOperation::Create {
        path: "generated/build.log".into(),
        content: "artifact\n".into(),
    };
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        std::slice::from_ref(&created),
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        1,
        "the write is refused once: {:?}",
        result.failed_files
    );
    let failure = result.failed_files.first().expect("the create is refused");
    assert_eq!(failure.path, "generated/build.log");
    match &failure.error {
        EditError::IgnoreProbeFailed { path, reason } => {
            assert_eq!(path, "generated/build.log");
            assert!(
                reason.contains("index"),
                "the refusal must carry Git's diagnostic: {reason}"
            );
        }
        other => panic!("expected a refused ignore probe, got {other:?}"),
    }
    assert!(result.successful_paths.is_empty());
    assert!(result.staged_state.staged_files.is_empty());
    assert!(!root.join("generated").exists());
}

/// Both admission branches read the one answer, so a refused probe refuses a
/// sandbox write as well instead of admitting it on the assumption that Git
/// ignores `.tauqe/`.
#[test]
fn a_refused_ignore_probe_refuses_a_sandbox_write_too() {
    let dir = repository_with_unreadable_index();
    let root = dir.path();

    let created = EditOperation::Create {
        path: ".tauqe/mockups/prototype.rs".into(),
        content: "pub fn prototype() {}\n".into(),
    };
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        std::slice::from_ref(&created),
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        1,
        "the write is refused once: {:?}",
        result.failed_files
    );
    assert!(matches!(
        &result.failed_files[0].error,
        EditError::IgnoreProbeFailed { path, .. } if path == ".tauqe/mockups/prototype.rs"
    ));
    assert!(result.staged_state.staged_files.is_empty());
    assert!(!root.join(".tauqe/mockups/prototype.rs").exists());
}

/// A target that is not a Git work tree has no ignore rules at all, so the
/// answer keeps admitting ordinary writes there.
#[test]
fn an_uninspectable_target_admits_a_create() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let created = EditOperation::Create {
        path: "generated.rs".into(),
        content: "pub fn generated() {}\n".into(),
    };
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        std::slice::from_ref(&created),
        None,
    );

    assert!(result.is_success(), "{:?}", result.failed_files);
    assert_eq!(
        staged_bytes(&result, "generated.rs"),
        &b"pub fn generated() {}\n"[..]
    );
    assert!(
        !root.join("generated.rs").exists(),
        "staging never touches disk"
    );
}

/// A file member mutates the staged state only after every admission and
/// validation step succeeded, so a refusal leaves the effect of an earlier
/// member intact instead of needing a snapshot of the path it addressed.
#[test]
fn a_refused_file_member_keeps_the_effect_of_an_earlier_member() {
    let dir = repository();
    let root = dir.path();

    let edits = [
        EditOperation::Create {
            path: "src/a.rs".to_string(),
            content: "pub fn a() {}\n".to_string(),
        },
        // The intervening member keeps the two operations on `src/a.rs` in
        // separate members, so the refusal below addresses content an earlier
        // member already staged.
        EditOperation::Create {
            path: "src/b.rs".to_string(),
            content: "pub fn b() {}\n".to_string(),
        },
        EditOperation::Replace {
            path: "src/a.rs".to_string(),
            old_text: "pub fn missing() {}\n".to_string(),
            new_text: "pub fn replaced() {}\n".to_string(),
        },
    ];
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        &edits,
        None,
    );

    assert!(!result.is_success());
    assert_eq!(
        result.failed_files.len(),
        1,
        "only the replacement is refused: {:?}",
        result.failed_files
    );
    let failure = result
        .failed_files
        .first()
        .expect("the replacement is refused");
    assert_eq!(failure.path, "src/a.rs");
    assert_eq!(
        staged_bytes(&result, "src/a.rs"),
        &b"pub fn a() {}\n"[..],
        "the refusal keeps the content the earlier member staged"
    );
    assert_eq!(staged_bytes(&result, "src/b.rs"), &b"pub fn b() {}\n"[..]);
    assert_eq!(
        result.successful_paths,
        vec!["src/a.rs", "src/b.rs"],
        "both earlier members keep their place in the report"
    );
    assert!(result.staged_state.deleted_paths.is_empty());
}

/// A destination the disk commit could never create is refused while the change
/// set is still purely in memory: a `create` beneath a component that already
/// exists as a regular file fails with `ENOTDIR` and rolls the whole change set
/// back instead of reporting the offending member.
#[test]
fn a_create_beneath_a_file_blocked_parent_is_refused() {
    let dir = repository();
    let root = dir.path();
    write(root, "src/new", "not a directory\n");

    let created = EditOperation::Create {
        path: "src/new/a.rs".to_string(),
        content: "pub fn a() {}\n".to_string(),
    };
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        std::slice::from_ref(&created),
        None,
    );

    assert!(!result.is_success());
    let failure = result.failed_files.first().expect("the create is refused");
    assert_eq!(failure.path, "src/new/a.rs");
    match &failure.error {
        EditError::ParentNotDirectory { path, blocked_by } => {
            assert_eq!(path, "src/new/a.rs");
            assert_eq!(blocked_by, "src/new");
        }
        other => panic!("expected a blocked parent rejection, got {other:?}"),
    }
    assert!(result.staged_state.staged_files.is_empty());
    assert!(result.staged_state.deleted_paths.is_empty());
    assert!(result.successful_paths.is_empty());
}

/// An `overwrite` reads the same admission as a `create`, so a destination
/// beneath a blocked parent is refused with the same typed rejection.
#[test]
fn an_overwrite_beneath_a_file_blocked_parent_is_refused() {
    let dir = repository();
    let root = dir.path();
    write(root, "src/new", "not a directory\n");

    let overwritten = EditOperation::Overwrite {
        path: "src/new/a.rs".to_string(),
        content: "pub fn a() {}\n".to_string(),
    };
    let result = stage_and_validate_edits(
        root,
        &ProjectConfig::default(),
        &context_state(&[]),
        std::slice::from_ref(&overwritten),
        None,
    );

    assert!(!result.is_success());
    let failure = result
        .failed_files
        .first()
        .expect("the overwrite is refused");
    assert_eq!(failure.path, "src/new/a.rs");
    match &failure.error {
        EditError::ParentNotDirectory { blocked_by, .. } => assert_eq!(blocked_by, "src/new"),
        other => panic!("expected a blocked parent rejection, got {other:?}"),
    }
    assert!(result.staged_state.staged_files.is_empty());
    assert!(result.successful_paths.is_empty());
}

/// A rename proves the parent chain of its destination before it transfers any
/// content, so the source keeps its place instead of being recorded as moved.
#[test]
fn a_rename_beneath_a_file_blocked_parent_is_refused() {
    let dir = repository();
    let root = dir.path();
    write(root, "a.txt", "from a\n");
    write(root, "src/new", "not a directory\n");

    let edits = [EditOperation::Move {
        from: "a.txt".to_string(),
        to: "src/new/a.txt".to_string(),
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
    assert_eq!(failure.path, "src/new/a.txt");
    match &failure.error {
        EditError::ParentNotDirectory { blocked_by, .. } => assert_eq!(blocked_by, "src/new"),
        other => panic!("expected a blocked parent rejection, got {other:?}"),
    }
    assert!(result.staged_state.staged_files.is_empty());
    assert!(!result.staged_state.deleted_paths.contains("a.txt"));
    assert!(result.successful_paths.is_empty());
}

/// A directory move proves the parent chain of every destination it expands
/// into before the first file stages, so a blocked destination refuses the whole
/// member instead of leaving a half-moved subtree behind.
#[test]
fn a_directory_move_beneath_a_file_blocked_parent_is_refused() {
    let dir = repository();
    let root = dir.path();
    write(root, "src/old/moved.rs", "fn moved() {}\n");
    write(root, "src/new", "not a directory\n");
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

    assert!(!result.is_success());
    let failure = result.failed_files.first().expect("the move is refused");
    assert_eq!(failure.path, "src/new/moved.rs");
    match &failure.error {
        EditError::ParentNotDirectory { blocked_by, .. } => assert_eq!(blocked_by, "src/new"),
        other => panic!("expected a blocked parent rejection, got {other:?}"),
    }
    assert!(result.staged_state.staged_files.is_empty());
    assert!(result.staged_state.deleted_paths.is_empty());
    assert!(result.successful_paths.is_empty());
}
