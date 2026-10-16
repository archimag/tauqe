//! Transactional on-disk commit of a validated change set.
//!
//! A validated change set is written all-or-nothing. New content is first
//! staged in a temporary sibling file, then any existing target is moved aside
//! to a backup sibling and the staged content is renamed into place. Every step
//! is a rename on a single filesystem, so the original's permissions,
//! timestamps and symlinks survive, and a rare failure rolls the working tree
//! back to its pre-commit state.

use std::path::{Path, PathBuf};

use super::{EditError, StagedEditsState};

/// A validated disk change prepared for an atomic commit.
///
/// A write stages its new content in a temporary sibling file. The commit then
/// moves any existing target aside to a backup sibling and moves the staged
/// content into place. Moving rather than copying preserves the original's
/// permissions, timestamps and symlinks, and keeps rollback independent of file
/// size.
struct PreparedChange {
    path: String,
    final_path: PathBuf,
    backup_path: PathBuf,
    /// Staged new content, present only for a write.
    temp_path: Option<PathBuf>,
    /// Set once the original was moved aside to `backup_path`.
    had_original: bool,
}

/// Returns the temporary sibling path used to stage a write before commit.
///
/// The temporary file lives in the same directory as its target so the final
/// rename stays on a single filesystem and is therefore atomic.
fn temp_sibling_path(final_path: &Path) -> PathBuf {
    sibling_path(final_path, ".tauqe-tmp")
}

/// Returns the backup sibling path used to move an original target aside.
///
/// The backup lives in the same directory as its target so moving the original
/// aside and restoring it stay on a single filesystem and are therefore atomic.
fn backup_sibling_path(final_path: &Path) -> PathBuf {
    sibling_path(final_path, ".tauqe-bak")
}

fn sibling_path(final_path: &Path, suffix: &str) -> PathBuf {
    let mut path = final_path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Removes the staged temporary files of every change that still has one.
fn remove_temp_files(changes: &[PreparedChange]) {
    for change in changes {
        if let Some(temp_path) = &change.temp_path {
            let _ = std::fs::remove_file(temp_path);
        }
    }
}

/// Removes the backup files created while committing the given changes.
fn remove_backup_files(changes: &[PreparedChange]) {
    for change in changes {
        if change.had_original {
            let _ = std::fs::remove_file(&change.backup_path);
        }
    }
}

/// Creates the parent directories of a staged write, recording the ones this
/// commit actually created so rollback can remove them again.
fn create_parent_dirs(
    repo_root: &Path,
    parent: &Path,
    created: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if let Ok(canonical_root) = repo_root.canonicalize() {
        let mut ancestor = parent;
        while !ancestor.exists() && ancestor.symlink_metadata().is_err() {
            match ancestor.parent() {
                Some(p) if !p.as_os_str().is_empty() => ancestor = p,
                _ => break,
            }
        }
        if ancestor.exists() || ancestor.symlink_metadata().is_ok() {
            match ancestor.canonicalize() {
                Ok(canonical_anc) => {
                    if !canonical_anc.starts_with(&canonical_root) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!(
                                "Parent directory '{}' escapes repository root via symlink",
                                parent.display()
                            ),
                        ));
                    }
                }
                Err(err) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "Parent directory '{}' ancestor cannot be resolved: {}",
                            parent.display(),
                            err
                        ),
                    ));
                }
            }
        }
    }

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

/// Moves the original target aside (if any) and moves the staged content into
/// place. Both steps are renames on a single filesystem, so a failure at any
/// point leaves enough information for [`rollback_change`] to restore the
/// target exactly.
fn commit_change(change: &mut PreparedChange) -> std::io::Result<()> {
    if change.final_path.symlink_metadata().is_ok() {
        // Drop any stale backup left by an interrupted earlier commit before
        // moving the current original aside.
        let _ = std::fs::remove_file(&change.backup_path);
        std::fs::rename(&change.final_path, &change.backup_path)?;
        change.had_original = true;
    }
    if let Some(temp_path) = &change.temp_path {
        std::fs::rename(temp_path, &change.final_path)?;
    }
    Ok(())
}

/// Restores a single target to its pre-commit state.
///
/// Staged content that reached the target is removed, and an original moved to
/// a backup is restored by a single rename, which preserves its permissions,
/// timestamps and symlink target.
fn rollback_change(change: &PreparedChange) {
    if let Some(temp_path) = &change.temp_path {
        let _ = std::fs::remove_file(&change.final_path);
        let _ = std::fs::remove_file(temp_path);
    }
    if change.had_original {
        let _ = std::fs::rename(&change.backup_path, &change.final_path);
    }
}

/// Commits a fully validated change set to disk with all-or-nothing semantics.
///
/// New contents are first materialized in temporary sibling files, so a
/// mid-write failure (for example, a full disk) surfaces before any target file
/// is touched and never leaves a half-written target. Existing targets are moved
/// aside to backup siblings rather than read into memory, so the commit does not
/// duplicate large files and rollback restores permissions, timestamps and
/// symlinks exactly. A rare rename or removal failure rolls the working tree
/// back to its pre-commit state, including any directories this commit created.
pub(crate) fn commit_staged_edits(
    repo_root: &Path,
    staged: &StagedEditsState,
    paths: &[String],
) -> Result<(), EditError> {
    let mut prepared = Vec::with_capacity(paths.len());
    let mut created_dirs: Vec<PathBuf> = Vec::new();

    for path in paths {
        crate::context::verify_path_within_repo(repo_root, path)
            .map_err(|err| EditError::InvalidPath(path.clone(), err.to_string()))?;

        let final_path = repo_root.join(path);
        let backup_path = backup_sibling_path(&final_path);
        let temp_path = if let Some(content) = staged.staged_files.get(path) {
            if let Some(parent) = final_path.parent() {
                if let Err(err) = create_parent_dirs(repo_root, parent, &mut created_dirs) {
                    remove_temp_files(&prepared);
                    remove_created_dirs(&created_dirs);
                    return Err(EditError::Io(path.clone(), err));
                }
            }
            let temp_path = temp_sibling_path(&final_path);
            if let Err(err) = std::fs::write(&temp_path, content) {
                let _ = std::fs::remove_file(&temp_path);
                remove_temp_files(&prepared);
                remove_created_dirs(&created_dirs);
                return Err(EditError::Io(path.clone(), err));
            }
            // Carry the original's permissions over to the staged content so a
            // write never silently widens or narrows access.
            if let Ok(metadata) = std::fs::metadata(&final_path) {
                let _ = std::fs::set_permissions(&temp_path, metadata.permissions());
            }
            Some(temp_path)
        } else {
            None
        };
        prepared.push(PreparedChange {
            path: path.clone(),
            final_path,
            backup_path,
            temp_path,
            had_original: false,
        });
    }

    for index in 0..prepared.len() {
        if let Err(err) = commit_change(&mut prepared[index]) {
            for change in &prepared[..=index] {
                rollback_change(change);
            }
            remove_temp_files(&prepared[index + 1..]);
            remove_created_dirs(&created_dirs);
            return Err(EditError::Io(prepared[index].path.clone(), err));
        }
    }

    remove_backup_files(&prepared);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::context::ContextManager;
    use crate::edits::{apply_edit_proposal, stage_and_validate_edits, EditError};
    use tauqe_protocol::{ContextAccess, EditOperation, EditProposal};
    use tempfile::tempdir;

    fn replace(path: &str, old_text: &str, new_text: &str) -> EditOperation {
        EditOperation::Replace {
            path: path.to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_move_failure_during_commit_leaves_context_intact() {
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

        let res = apply_edit_proposal(root, &mut cm, &proposal);
        assert!(res.is_err());

        // Context must not have applied the rename since commit failed
        assert!(cm.contains("old.rs").unwrap());
        assert!(!cm.contains("new.rs").unwrap());
        assert!(root.join("old.rs").exists());
        assert!(!root.join("new.rs").exists());

        // Reset permissions so tempdir cleanup succeeds
        let _ = std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755));
    }

    #[test]
    fn commit_writes_all_files_atomically_and_leaves_no_temporary_files() {
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
                EditOperation::Create { path: "c.rs".into(), content: "gamma".into() },
                EditOperation::Delete { path: "b.rs".into() },
            ],
        };
        let changed = apply_edit_proposal(root, &mut cm, &proposal).unwrap();
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

    #[test]
    #[cfg(unix)]
    fn test_stage_and_commit_rejects_symlink_escaping_repo() {
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
        let result = stage_and_validate_edits(
            repo_root,
            &cm,
            std::slice::from_ref(&create_op),
            None,
        );
        assert!(!result.is_success());
        assert!(matches!(&result.failed_files[0].error, EditError::InvalidPath(_, _)));
        assert!(!outside_root.join("hacked.txt").exists());

        let proposal = EditProposal {
            summary: "create via escaping symlink".into(),
            edits: vec![create_op],
        };
        assert!(apply_edit_proposal(repo_root, &mut cm, &proposal).is_err());
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
}
