//! Transactional on-disk commit of a validated change set.
//!
//! A validated change set is written all-or-nothing through the sibling commit
//! policy of [`crate::atomic_write`], the single authoritative implementation
//! of the staging rules: new content is synced into a unique sibling file, any
//! existing target is moved aside to a unique backup sibling, and the staged
//! content is renamed into place. Every step is a rename on a single
//! filesystem, so the original's permissions, timestamps and symlinks survive,
//! the renames are flushed to their parent directory, and a rare failure rolls
//! the working tree back to its pre-commit state.

use std::path::{Path, PathBuf};

use super::{EditError, StagedEditsState};
use crate::atomic_write::SiblingCommit;

/// Drops the staged content of changes that never reached their commit.
fn discard_staged(changes: &[SiblingCommit]) {
    for change in changes {
        change.discard_staged_content();
    }
}

/// Creates the parent directories of a staged write, recording the ones this
/// commit actually created so rollback can remove them again.
fn create_parent_dirs(parent: &Path, created: &mut Vec<PathBuf>) -> std::io::Result<()> {
    // Path safety is enforced before staging: `commit_staged_edits` calls
    // `verify_path_within_repo` for the full target, which walks every ancestor
    // component and rejects escaping or dangling symlinks. Here we only need to
    // resolve which directories to create.

    // Walk upward and collect the ancestors that do not exist yet, deepest
    // first, so rollback removes exactly the directories this commit created.
    let mut missing = Vec::new();
    let mut current = Some(parent);
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        current = dir.parent();
    }
    if missing.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent)?;
    created.extend(missing);
    Ok(())
}

/// Removes directories created during the commit, deepest first so parents are
/// only removed once their children are gone. A non-empty directory is left in
/// place because it was not created exclusively by this commit.
fn remove_created_dirs(created: &[PathBuf]) {
    let mut dirs: Vec<&PathBuf> = created.iter().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Commits a fully validated change set to disk with all-or-nothing semantics.
///
/// New contents are first materialized in unique sibling files and synced, so a
/// mid-write failure (for example, a full disk) surfaces before any target file
/// is touched and a crash can never publish a truncated target. Existing
/// targets are moved aside to unique backup siblings rather than read into
/// memory, so the commit does not duplicate large files and rollback restores
/// permissions, timestamps and symlinks exactly. Each rename is flushed to its
/// parent directory by the shared staging policy of [`crate::atomic_write`]. A
/// rare rename or removal failure rolls the working tree back to its pre-commit
/// state, including any directories this commit created.
pub(crate) fn commit_staged_edits(
    repo_root: &Path,
    staged: &StagedEditsState,
    paths: &[String],
) -> Result<(), EditError> {
    let mut prepared: Vec<SiblingCommit> = Vec::with_capacity(paths.len());
    let mut created_dirs: Vec<PathBuf> = Vec::new();

    for path in paths {
        crate::context::verify_path_within_repo(repo_root, path)
            .map_err(|err| EditError::InvalidPath(path.clone(), err.to_string()))?;

        let final_path = repo_root.join(path);
        let change = match staged.staged_files.get(path) {
            Some(content) => {
                // The staged content lands in a sibling of its target, so the
                // parent directory has to exist before it can be staged.
                let parent_ready = match final_path.parent() {
                    Some(parent) => create_parent_dirs(parent, &mut created_dirs),
                    None => Ok(()),
                };
                parent_ready.and_then(|()| SiblingCommit::stage(&final_path, content.as_bytes()))
            }
            None => SiblingCommit::removal(&final_path),
        };

        match change {
            Ok(change) => prepared.push(change),
            Err(err) => {
                // Nothing reached disk yet, so the staged content prepared for
                // the earlier changes of this attempt is dropped again.
                discard_staged(&prepared);
                remove_created_dirs(&created_dirs);
                return Err(EditError::Io(path.clone(), err));
            }
        }
    }

    let mut failure = None;
    for (index, change) in prepared.iter_mut().enumerate() {
        if let Err(err) = change.commit() {
            failure = Some((index, err));
            break;
        }
    }

    if let Some((index, err)) = failure {
        // Undo the changes that were already published and drop the staged
        // content of the changes this attempt never reached.
        if let Some(committed) = prepared.get(..=index) {
            for change in committed {
                change.rollback();
            }
        }
        if let Some(uncommitted) = prepared.get(index + 1..) {
            discard_staged(uncommitted);
        }
        remove_created_dirs(&created_dirs);
        let path = paths.get(index).cloned().unwrap_or_default();
        return Err(EditError::Io(path, err));
    }

    // The whole change set is on disk, so the backups of the originals are no
    // longer needed.
    for change in prepared {
        change.finish();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::commit_staged_edits;
    use crate::context::ContextManager;
    use crate::edits::{
        apply_edit_proposal, stage_and_validate_edits, EditError, StagedEditsState,
    };
    use std::path::Path;
    use tauqe_protocol::{ContextAccess, EditOperation, EditProposal};
    use tempfile::tempdir;

    /// Names of the staging siblings currently present in `dir`.
    fn leftover_siblings(dir: &Path) -> Vec<String> {
        use crate::atomic_write::{staging_backup_original_name, staging_temp_original_name};

        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                staging_temp_original_name(name).is_some()
                    || staging_backup_original_name(name).is_some()
            })
            .collect()
    }

    fn replace(path: &str, old_text: &str, new_text: &str) -> EditOperation {
        EditOperation::Replace {
            path: path.to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_move_failure_during_commit_leaves_context_intact() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.rs"), "fn hello() {}").unwrap();

        let readonly_dir = root.join("readonly_dir");
        std::fs::create_dir_all(&readonly_dir).unwrap();
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("old.rs", ContextAccess::Editable).unwrap();

        let move_op = EditOperation::Move {
            from: "old.rs".into(),
            to: "new.rs".into(),
        };
        let failing_create = EditOperation::Create {
            path: "readonly_dir/new_file.rs".into(),
            content: "content".into(),
        };

        let proposal = EditProposal {
            summary: "move and failing create".into(),
            edits: vec![move_op, failing_create],
        };

        let res = apply_edit_proposal(root, &mut cm, &proposal).await;
        assert!(res.is_err());

        // Context must not have applied the rename since commit failed
        assert!(cm.contains("old.rs").unwrap());
        assert!(!cm.contains("new.rs").unwrap());
        assert!(root.join("old.rs").exists());
        assert!(!root.join("new.rs").exists());

        // Reset permissions so tempdir cleanup succeeds
        let _ = std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755));
    }

    #[tokio::test]
    async fn commit_writes_all_files_atomically_and_leaves_no_temporary_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one").unwrap();
        std::fs::write(root.join("b.rs"), "two").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        cm.add_file("b.rs", ContextAccess::Editable).unwrap();

        let proposal = EditProposal {
            summary: "multi".into(),
            edits: vec![
                replace("a.rs", "one", "alpha"),
                EditOperation::Create {
                    path: "c.rs".into(),
                    content: "gamma".into(),
                },
                EditOperation::Delete {
                    path: "b.rs".into(),
                },
            ],
        };
        let changed = apply_edit_proposal(root, &mut cm, &proposal).await.unwrap();
        assert!(changed.contains(&"a.rs".to_string()));
        assert!(changed.contains(&"c.rs".to_string()));
        assert!(changed.contains(&"b.rs".to_string()));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(root.join("c.rs")).unwrap(), "gamma");
        assert!(!root.join("b.rs").exists());

        let leftover = std::fs::read_dir(root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().contains(".tauqe-tmp"));
        assert!(!leftover, "commit must not leave temporary files behind");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_stage_and_commit_rejects_symlink_escaping_repo() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path();
        let outside_root = outside.path();

        let secret = outside_root.join("target.txt");
        std::fs::write(&secret, "initial").unwrap();

        let sym_dir = repo_root.join("sym_dir");
        std::os::unix::fs::symlink(outside_root, &sym_dir).unwrap();

        let sym_file = repo_root.join("sym_file.txt");
        std::os::unix::fs::symlink(&secret, &sym_file).unwrap();

        let mut cm = ContextManager::new(repo_root.to_path_buf());

        // Attempt to create a file inside an escaping symlink directory
        let create_op = EditOperation::Create {
            path: "sym_dir/hacked.txt".into(),
            content: "payload".into(),
        };
        let result =
            stage_and_validate_edits(repo_root, &cm, std::slice::from_ref(&create_op), None);
        assert!(!result.is_success());
        assert!(matches!(
            &result.failed_files[0].error,
            EditError::InvalidPath(_, _)
        ));
        assert!(!outside_root.join("hacked.txt").exists());

        let proposal = EditProposal {
            summary: "create via escaping symlink".into(),
            edits: vec![create_op],
        };
        assert!(apply_edit_proposal(repo_root, &mut cm, &proposal)
            .await
            .is_err());
        assert!(!outside_root.join("hacked.txt").exists());

        // Attempt to overwrite or replace a file via an escaping symlink
        let replace_op = EditOperation::Replace {
            path: "sym_file.txt".into(),
            old_text: "initial".into(),
            new_text: "modified".into(),
        };
        let replace_res = stage_and_validate_edits(repo_root, &cm, &[replace_op], None);
        assert!(!replace_res.is_success());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "initial");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_stage_and_commit_rejects_dangling_symlink_escaping_repo() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path();
        let outside_root = outside.path();

        // Directory symlink whose target does not exist yet and lives outside
        // the repository. It must be rejected before any directory is created.
        let dangling_target = outside_root.join("missing_dir");
        let sym_dir = repo_root.join("dangling_dir");
        std::os::unix::fs::symlink(&dangling_target, &sym_dir).unwrap();

        let mut cm = ContextManager::new(repo_root.to_path_buf());

        let create_op = EditOperation::Create {
            path: "dangling_dir/sub/hacked.txt".into(),
            content: "payload".into(),
        };
        let result =
            stage_and_validate_edits(repo_root, &cm, std::slice::from_ref(&create_op), None);
        assert!(!result.is_success());
        assert!(matches!(
            &result.failed_files[0].error,
            EditError::InvalidPath(_, _)
        ));

        let proposal = EditProposal {
            summary: "create via dangling escaping symlink".into(),
            edits: vec![create_op],
        };
        assert!(apply_edit_proposal(repo_root, &mut cm, &proposal)
            .await
            .is_err());
        assert!(!outside_root.join("missing_dir").exists());
    }

    #[test]
    fn a_directory_target_is_rejected_before_any_rename() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("existing_dir")).unwrap();
        std::fs::write(root.join("existing_dir").join("inner.rs"), "inner").unwrap();

        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("existing_dir".to_string(), "content".to_string());

        let result = commit_staged_edits(root, &staged, &["existing_dir".to_string()]);

        assert!(matches!(result, Err(EditError::Io(_, _))));
        assert_eq!(
            std::fs::read_to_string(root.join("existing_dir").join("inner.rs")).unwrap(),
            "inner",
            "a directory target must never be replaced"
        );
        assert!(leftover_siblings(root).is_empty());
    }

    #[test]
    fn concurrent_commits_of_one_path_never_publish_partial_content() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("shared.rs"), "original").unwrap();

        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("shared.rs".to_string(), "content".to_string());

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                let staged = staged.clone();
                std::thread::spawn(move || {
                    commit_staged_edits(&root, &staged, &["shared.rs".to_string()])
                })
            })
            .collect();

        let mut published = 0;
        for handle in handles {
            if handle.join().unwrap().is_ok() {
                published += 1;
            }
        }

        // The staging and backup names carry a process id and a sequence
        // number, so overlapping commits cannot truncate each other's staged
        // content: whatever reaches the target is one writer's whole payload.
        assert!(
            published >= 1,
            "at least one commit publishes the change set"
        );
        let content = std::fs::read_to_string(root.join("shared.rs")).ok();
        assert!(
            content.is_none() || content.as_deref() == Some("content"),
            "a commit can only publish whole content, got {content:?}"
        );
        assert!(
            leftover_siblings(&root).is_empty(),
            "no staging sibling may survive a commit"
        );
    }
}
