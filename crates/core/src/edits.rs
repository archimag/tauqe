use std::collections::{HashMap, HashSet};
use std::path::Path;
use tauqe_protocol::{EditOperation, EditProposal};
use thiserror::Error;

use crate::context::ContextManager;

mod atomic;
mod fuzzy;
pub mod protocol;
pub mod stream;

pub use protocol::{EditProtocolFactory, XmlEditProtocol};
pub use stream::{EditStreamFilter, JsonStreamFilter, XmlStreamFilter};

pub(crate) use fuzzy::apply_replace;

use atomic::commit_staged_edits;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("File '{0}' is not in context. Only files explicitly in context can be edited.")]
    NotInContext(String),

    #[error("File '{0}' is read-only. Change its access to editable before applying edits.")]
    ReadOnly(String),

    #[error("File '{0}' does not exist on disk.")]
    FileNotFound(String),

    #[error("Target path '{0}' already exists.")]
    DestinationExists(String),

    #[error("Target path '{0}' is invalid: {1}")]
    InvalidPath(String, String),

    #[error("In file '{path}': old_text not found (0 matches). Code might be stale or incorrect.")]
    NoMatch { path: String },

    #[error("In file '{path}': old_text matches {count} times. Replacement must be uniquely identifiable (exactly 1 match).")]
    AmbiguousMatch { path: String, count: usize },

    #[error("IO error for '{0}': {1}")]
    Io(String, #[source] std::io::Error),
}

/// Cumulative in-memory state. Paths are normalized relative to the repository.
/// A path must occur in at most one of these collections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedEditsState {
    pub staged_files: HashMap<String, String>,
    pub deleted_paths: HashSet<String>,
}

#[derive(Debug)]
pub struct FailedFileEdit {
    pub path: String,
    pub error: EditError,
}

#[derive(Debug)]
pub struct StageEditsResult {
    /// Includes successful state from earlier attempts, unchanged on a file failure.
    pub staged_state: StagedEditsState,
    /// Successful paths and operations from this attempt only.
    pub successful_paths: Vec<String>,
    pub successful_edits: Vec<EditOperation>,
    pub failed_files: Vec<FailedFileEdit>,
}

impl StageEditsResult {
    pub fn is_success(&self) -> bool {
        self.failed_files.is_empty()
    }
}

/// Validates edits without changing disk or context. Operations for each normalized
/// path are applied in order, and the entire file is rolled back if any operation
/// fails. Other files can succeed independently. Pass the returned staged state
/// into the next attempt to retain earlier successes.
pub fn stage_and_validate_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    // Staging reads the original file contents, which is blocking file I/O.
    // Run it through the blocking helper so a large file never stalls an async
    // worker thread.
    crate::blocking::run_blocking(|| {
        stage_and_validate_edits_inner(repo_root, context_manager, edits, initial_staged)
    })
}

fn stage_and_validate_edits_inner(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    let mut result = StageEditsResult {
        staged_state: initial_staged.cloned().unwrap_or_default(),
        successful_paths: Vec::new(),
        successful_edits: Vec::new(),
        failed_files: Vec::new(),
    };
    let mut file_order = Vec::new();
    let mut grouped: HashMap<String, Vec<&EditOperation>> = HashMap::new();
    let mut invalid_paths = HashSet::new();

    let flush_grouped = |grouped: &mut HashMap<String, Vec<&EditOperation>>,
                         file_order: &mut Vec<String>,
                         res: &mut StageEditsResult| {
        for path in file_order.drain(..) {
            if let Some(operations) = grouped.remove(&path) {
                match stage_file_edits(
                    repo_root,
                    context_manager,
                    &path,
                    &operations,
                    &res.staged_state,
                ) {
                    Ok(Some(content)) => {
                        res.staged_state.deleted_paths.remove(&path);
                        res.staged_state.staged_files.insert(path.clone(), content);
                    }
                    Ok(None) => {
                        res.staged_state.staged_files.remove(&path);
                        res.staged_state.deleted_paths.insert(path.clone());
                    }
                    Err(error) => {
                        res.failed_files.push(FailedFileEdit { path, error });
                        continue;
                    }
                }
                if !res.successful_paths.contains(&path) {
                    res.successful_paths.push(path);
                }
                res.successful_edits.extend(operations.into_iter().cloned());
            }
        }
    };

    for edit in edits {
        match edit {
            EditOperation::Move { from, to } => {
                flush_grouped(&mut grouped, &mut file_order, &mut result);

                let clean_from = match context_manager.normalize_path(from) {
                    Ok(p) => p,
                    Err(err) => {
                        result.failed_files.push(FailedFileEdit {
                            path: from.clone(),
                            error: EditError::InvalidPath(from.clone(), err.to_string()),
                        });
                        continue;
                    }
                };

                let clean_to = match context_manager.normalize_path(to) {
                    Ok(p) => p,
                    Err(err) => {
                        result.failed_files.push(FailedFileEdit {
                            path: to.clone(),
                            error: EditError::InvalidPath(to.clone(), err.to_string()),
                        });
                        continue;
                    }
                };

                if clean_from == clean_to {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_to.clone(),
                        error: EditError::InvalidPath(
                            clean_to,
                            "Source and destination are identical".to_string(),
                        ),
                    });
                    continue;
                }

                let in_context = context_manager.contains(&clean_from).unwrap_or(false);
                let newly_created = !repo_root.join(&clean_from).exists()
                    && result.staged_state.staged_files.contains_key(&clean_from);

                if !in_context && !newly_created {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_from.clone(),
                        error: EditError::NotInContext(clean_from),
                    });
                    continue;
                }

                let content = if let Some(c) = result.staged_state.staged_files.get(&clean_from) {
                    Some(c.clone())
                } else if result.staged_state.deleted_paths.contains(&clean_from) {
                    None
                } else {
                    let full = repo_root.join(&clean_from);
                    if full.is_file() {
                        std::fs::read_to_string(&full).ok()
                    } else {
                        None
                    }
                };

                let Some(content) = content else {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_from.clone(),
                        error: EditError::FileNotFound(clean_from),
                    });
                    continue;
                };

                let full_to = repo_root.join(&clean_to);
                let dest_exists = (full_to.exists()
                    && !result.staged_state.deleted_paths.contains(&clean_to))
                    || result.staged_state.staged_files.contains_key(&clean_to);

                if dest_exists {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_to.clone(),
                        error: EditError::DestinationExists(clean_to),
                    });
                    continue;
                }

                result.staged_state.staged_files.remove(&clean_from);
                result.staged_state.deleted_paths.insert(clean_from.clone());
                result
                    .staged_state
                    .staged_files
                    .insert(clean_to.clone(), content);
                result.staged_state.deleted_paths.remove(&clean_to);

                if !result.successful_paths.contains(&clean_from) {
                    result.successful_paths.push(clean_from);
                }
                if !result.successful_paths.contains(&clean_to) {
                    result.successful_paths.push(clean_to);
                }
                result.successful_edits.push(edit.clone());
            }
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path }
            | EditOperation::Overwrite { path, .. } => match context_manager.normalize_path(path) {
                Ok(clean_path) => {
                    if !grouped.contains_key(&clean_path) {
                        file_order.push(clean_path.clone());
                    }
                    grouped.entry(clean_path).or_default().push(edit);
                }
                Err(err) => {
                    if invalid_paths.insert(path.clone()) {
                        result.failed_files.push(FailedFileEdit {
                            path: path.clone(),
                            error: EditError::InvalidPath(path.clone(), err.to_string()),
                        });
                    }
                }
            },
        }
    }

    flush_grouped(&mut grouped, &mut file_order, &mut result);
    result
}

/// None represents a staged deletion. Disk reads are deferred until a replace
/// needs the original content, so create/delete also work for non-text files.
fn stage_file_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    path: &str,
    edits: &[&EditOperation],
    initial_staged: &StagedEditsState,
) -> Result<Option<String>, EditError> {
    let invalid_path =
        |err: anyhow::Error| EditError::InvalidPath(path.to_string(), err.to_string());
    crate::context::verify_path_within_repo(repo_root, path).map_err(invalid_path)?;
    let in_context = context_manager.contains(path).map_err(invalid_path)?;
    let full_path = repo_root.join(path);
    let deleted = initial_staged.deleted_paths.contains(path);
    let mut content = if deleted {
        None
    } else {
        initial_staged.staged_files.get(path).cloned()
    };
    let mut loaded = deleted || content.is_some();
    // Files created in memory need not be added to context until disk commit.
    let mut newly_created = !full_path.exists() && content.is_some();

    for edit in edits {
        if !in_context && !newly_created {
            match edit {
                EditOperation::Create { .. } if !full_path.exists() => {}
                _ => return Err(EditError::NotInContext(path.to_string())),
            }
        }

        match edit {
            EditOperation::Replace {
                old_text, new_text, ..
            } => {
                if !loaded {
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(path.to_string()));
                    }
                    content = Some(
                        std::fs::read_to_string(&full_path)
                            .map_err(|err| EditError::Io(path.to_string(), err))?,
                    );
                    loaded = true;
                }
                let current = content
                    .as_ref()
                    .ok_or_else(|| EditError::FileNotFound(path.to_string()))?;
                let updated = apply_replace(current, old_text, new_text, path)?;
                content = Some(updated);
            }
            EditOperation::Create {
                content: new_content,
                ..
            } => {
                content = Some(new_content.clone());
                loaded = true;
                newly_created = !full_path.exists();
            }
            EditOperation::Overwrite {
                content: new_content,
                ..
            } => {
                // Full replacement requires an existing (on disk or staged) file.
                if content.is_none() && (loaded || !full_path.is_file()) {
                    return Err(EditError::FileNotFound(path.to_string()));
                }
                content = Some(new_content.clone());
                loaded = true;
            }
            EditOperation::Delete { .. } => {
                content = None;
                loaded = true;
            }
            EditOperation::Move { .. } => {
                return Err(EditError::InvalidPath(
                    path.to_string(),
                    "Move operations cannot be staged as per-file edits".to_string(),
                ));
            }
        }
    }
    Ok(content)
}

/// Applies an EditProposal after validating every file in memory.
///
/// If ANY edit fails validation, no files on disk are modified. The commit is
/// atomic: new contents are staged in temporary sibling files and renamed into
/// place, and a mid-commit IO failure rolls the working tree back to its
/// pre-commit state.
pub fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    // Writing the validated change set, including creating parent directories,
    // is blocking file I/O. Run it through the blocking helper so a burst of
    // writes never stalls an async worker thread.
    crate::blocking::run_blocking(|| {
        apply_edit_proposal_inner(repo_root, context_manager, proposal)
    })
}

fn apply_edit_proposal_inner(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    let result = stage_and_validate_edits(repo_root, context_manager, &proposal.edits, None);
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }

    // Commit the validated change set to disk with all-or-nothing semantics.
    // The context layer is only touched after every file is committed, so a
    // disk failure never leaves context and disk out of sync.
    commit_staged_edits(repo_root, &result.staged_state, &result.successful_paths)?;

    // Reconcile the context layer with the committed change set strictly after
    // the disk commit succeeds. The net transition is derived directly from the
    // final staged state instead of replaying individual `Move` renames: every
    // path that no longer exists on disk is dropped, and every staged path is
    // ensured present and editable. Deriving the transition from the net state
    // keeps chained moves (`a -> b -> c`), moves whose source path is recreated,
    // and mixed operation sequences from desynchronizing context from disk.
    for path in &result.staged_state.deleted_paths {
        let _ = context_manager.remove_file(path);
    }
    for path in result.staged_state.staged_files.keys() {
        if context_manager.contains(path).unwrap_or(false) {
            let _ = context_manager.update_file_metadata(path);
            let _ = context_manager.set_access(path, tauqe_protocol::ContextAccess::Editable);
        } else {
            // A path absent from context is a newly created file (or a move
            // destination). Register it in the durable user layer rather than
            // the transient auto layer: the turn finalizer sweeps the auto
            // layer at turn completion and would otherwise drop the file the
            // turn just created.
            let _ = context_manager.add_file(path, tauqe_protocol::ContextAccess::Editable);
        }
    }
    Ok(result.successful_paths)
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, ContextLayer};
    use tempfile::tempdir;

    fn replace(path: &str, old_text: &str, new_text: &str) -> EditOperation {
        EditOperation::Replace {
            path: path.to_string(),
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn partial_failure_rolls_back_entire_file_and_keeps_disk_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one").unwrap();
        std::fs::write(root.join("b.rs"), "two").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        cm.add_file("b.rs", ContextAccess::Editable).unwrap();
        let before = cm.get_state().revision;
        let edits = vec![
            replace("a.rs", "one", "changed"),
            replace("b.rs", "two", "success"),
            replace("./a.rs", "missing", "failed"),
        ];
        let result = stage_and_validate_edits(root, &cm, &edits, None);
        assert_eq!(result.successful_paths, vec!["b.rs"]);
        assert_eq!(result.successful_edits, vec![edits[1].clone()]);
        assert_eq!(result.staged_state.staged_files["b.rs"], "success");
        assert!(!result.staged_state.staged_files.contains_key("a.rs"));
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
        assert!(apply_edit_proposal(root, &mut cm, &proposal).is_err());
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");
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
        assert!(second.is_success());
        assert_eq!(second.staged_state.staged_files["a.rs"], "three");
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

    #[test]
    fn create_replace_delete_and_recreate_follow_operation_order() {
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
        assert!(second.is_success());
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
            apply_edit_proposal(root, &mut cm, &proposal).unwrap(),
            vec!["new.rs"]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.rs")).unwrap(),
            "final"
        );
        assert!(cm.is_editable("new.rs").unwrap());
        let state = cm.get_state();
        let new_item = state.items.iter().find(|i| i.path == "new.rs").unwrap();
        assert_eq!(new_item.layer, ContextLayer::User);
        let deletion = EditProposal {
            summary: "delete".into(),
            edits: vec![EditOperation::Delete {
                path: "new.rs".into(),
            }],
        };
        apply_edit_proposal(root, &mut cm, &deletion).unwrap();
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
        let missing =
            stage_and_validate_edits(root, &cm, &[replace("missing.rs", "one", "two")], None);
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

    #[test]
    fn readonly_file_in_context_can_be_edited_and_promoted_to_editable() {
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
        let changed = apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert_eq!(changed, vec!["ref.rs"]);
        assert_eq!(
            std::fs::read_to_string(root.join("ref.rs")).unwrap(),
            "fn updated() {}"
        );
        assert!(cm.is_editable("ref.rs").unwrap());
    }

    #[test]
    fn overwrite_replaces_existing_file_and_requires_context() {
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
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "all new"
        );
    }

    #[test]
    fn test_move_file_stages_and_applies() {
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
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert!(!root.join("old.rs").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("new.rs")).unwrap(),
            "fn hello() {}"
        );
        assert!(cm.is_editable("new.rs").unwrap());
        assert!(!cm.contains("old.rs").unwrap());
    }

    #[test]
    fn chained_moves_leave_only_the_final_destination_in_context() {
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
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();

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

    #[test]
    fn move_with_recreated_source_keeps_source_and_destination_in_context() {
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
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();

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

    #[test]
    fn staging_preserves_crlf_and_empty_attempt_preserves_state() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one\r\n").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        let result =
            stage_and_validate_edits(root, &cm, &[replace("a.rs", "one\n", "two\n")], None);
        assert!(result.is_success());
        assert_eq!(result.staged_state.staged_files["a.rs"], "two\r\n");
        let empty = stage_and_validate_edits(root, &cm, &[], Some(&result.staged_state));
        assert!(empty.is_success());
        assert_eq!(empty.staged_state, result.staged_state);
        assert!(empty.successful_edits.is_empty());
    }

    #[test]
    fn created_file_survives_the_auto_layer_sweep() {
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
        apply_edit_proposal(root, &mut cm, &proposal).unwrap();
        assert!(cm.contains("generated.rs").unwrap());
        assert!(cm.is_editable("generated.rs").unwrap());

        // The turn finalizer clears the transient auto layer; a created file
        // must live in the durable user layer so the end-of-turn sweep does
        // not silently drop the file the turn just produced.
        cm.clear_auto();
        assert!(
            cm.contains("generated.rs").unwrap(),
            "a created file must survive the end-of-turn auto sweep"
        );
        assert!(cm.is_editable("generated.rs").unwrap());
    }
}
