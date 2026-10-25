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

/// Every rejection a staged change set can produce.
///
/// A read-only context file is deliberately not among them. The harness
/// auto-promotes such a file to editable once a change set that touches it is
/// applied, so auto-promotion is the single documented enforcement path and the
/// enum never advertises a rejection the engine cannot produce.
#[derive(Debug, Error)]
pub enum EditError {
    #[error("File '{0}' is not in context. Only files explicitly in context can be edited.")]
    NotInContext(String),

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

    #[error("In file '{path}': invalid project configuration: {message}")]
    InvalidConfig { path: String, message: String },

    #[error(
        "Path '{0}' is excluded by the repository ignore rules (.gitignore or .git/info/exclude). Choose a path the repository tracks."
    )]
    IgnoredByGit(String),
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

/// Stages edits in memory and streams every staged file through the project filter.
///
/// Virtual staging has two halves now that formatting is a pure stream: the change
/// set is applied to memory by [`stage_and_validate_edits`], and the content of
/// the files this attempt staged is then passed through the declared
/// `[project.format]` filter on standard input. What lands in
/// [`StageEditsResult::staged_state`] is therefore already formatted, so the code
/// that eventually reaches the working tree is the formatted code and Git never
/// observes an unformatted intermediate.
///
/// The filter is resolved once per call, so a change set costs one configuration
/// read instead of one per file. A repository that declares no filter keeps the
/// authored content, and a filter that fails or times out does the same: a broken
/// formatter never blocks a turn.
pub async fn prepare_staged_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    let mut result = stage_and_validate_edits(repo_root, context_manager, edits, initial_staged);
    format_staged_files(repo_root, &mut result).await;
    result
}

/// Streams the files staged in this attempt through the project format filter.
///
/// Only paths staged in the current attempt are filtered. Content retained from an
/// earlier attempt already passed through the formatter, and running the filter
/// over it again would spawn a process for bytes that cannot change.
async fn format_staged_files(repo_root: &Path, result: &mut StageEditsResult) {
    if result.successful_paths.is_empty() {
        return;
    }

    // The filter is a property of the repository, so the declaration is resolved
    // once and reused for every file of the change set.
    let project = crate::toolchain::effective_project_config(repo_root, None).await;

    for path in result.successful_paths.clone() {
        let Some(authored) = result.staged_state.staged_files.get(&path).cloned() else {
            // A move records both sides of the rename, and only its destination
            // holds content; a deletion holds none.
            continue;
        };
        let Some(outcome) =
            crate::toolchain::format_with_project_config(repo_root, &project, &authored, None)
                .await
        else {
            // The repository declares no in-memory formatter.
            return;
        };
        if outcome.formatted {
            result
                .staged_state
                .staged_files
                .insert(path, outcome.content);
        } else if let Some(reason) = outcome.error {
            tracing::warn!(
                file = %path,
                reason = %reason,
                "in-memory formatter kept the authored content"
            );
        }
    }
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
    let ignored_paths = ignored_target_paths(repo_root, context_manager, edits);

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

                if ignored_paths.contains(&clean_to) {
                    result.failed_files.push(FailedFileEdit {
                        path: clean_to.clone(),
                        error: EditError::IgnoredByGit(clean_to),
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
                    // Git refuses to stage a path its ignore rules exclude,
                    // so writing such a path is rejected here, while the
                    // change set is still purely in memory.
                    let writes_body = matches!(
                        edit,
                        EditOperation::Create { .. } | EditOperation::Overwrite { .. }
                    );
                    if writes_body && ignored_paths.contains(&clean_path) {
                        result.failed_files.push(FailedFileEdit {
                            path: clean_path.clone(),
                            error: EditError::IgnoredByGit(clean_path),
                        });
                        continue;
                    }
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

/// Resolves the repository paths a change set would create, rewrite, or move to.
///
/// A path Git refuses to stage can only be detected after the whole turn has
/// already applied its edits, which aborts the Git transaction instead of the
/// edit. Asking the question during staging keeps the failure inside the turn:
/// one batched `git check-ignore` call answers for the whole change set, and a
/// repository Git cannot inspect degrades to "nothing is ignored" rather than
/// blocking an otherwise valid turn.
fn ignored_target_paths(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
) -> HashSet<String> {
    let mut candidates: Vec<String> = edits
        .iter()
        .filter_map(|edit| match edit {
            EditOperation::Create { path, .. } | EditOperation::Overwrite { path, .. } => {
                Some(path)
            }
            EditOperation::Move { to, .. } => Some(to),
            _ => None,
        })
        .filter_map(|path| context_manager.normalize_path(path).ok())
        .collect();

    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        return HashSet::new();
    }

    match crate::git::state::gitignored_paths(repo_root, &candidates) {
        Ok(ignored) => ignored.into_iter().collect(),
        Err(err) => {
            tracing::warn!(
                "Failed to check repository ignore rules for {} path(s): {}",
                candidates.len(),
                err
            );
            HashSet::new()
        }
    }
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
    validate_staged_project_config(path, content.as_deref())?;
    Ok(content)
}

/// Repository-relative path of the project configuration file.
const PROJECT_CONFIG_PATH: &str = "tauqe.toml";

/// Validates the staged content of the project configuration file.
///
/// The harness must never commit a configuration it could not load on its next
/// start, so a change set that rewrites `tauqe.toml` is checked while it is
/// still purely in memory. The validation has two levels: the TOML syntax is
/// parsed first, and the resulting declaration is then validated semantically,
/// so dropped model roles or a missing verification command are rejected as
/// well. A failure surfaces as an ordinary [`EditError`], which activates the
/// patch-retry loop instead of letting a broken file reach the working tree.
fn validate_staged_project_config(path: &str, content: Option<&str>) -> Result<(), EditError> {
    if path != PROJECT_CONFIG_PATH {
        return Ok(());
    }
    let Some(content) = content else {
        return Ok(());
    };

    // Level 1 — syntactic validation of the staged bytes.
    let parsed: crate::config::AppConfig =
        toml::from_str(content).map_err(|err| EditError::InvalidConfig {
            path: path.to_string(),
            message: format!(
                "the file is not valid TOML: {}",
                crate::escape::single_line(&err.to_string())
            ),
        })?;

    // Level 2 — semantic validation of the mandatory structures the harness
    // depends on: the model roles must resolve and verification must stay
    // declared.
    parsed.validate().map_err(|err| EditError::InvalidConfig {
        path: path.to_string(),
        message: crate::escape::single_line(&err),
    })
}

/// Applies an EditProposal after validating and formatting every file in memory.
///
/// If ANY edit fails validation, no files on disk are modified. The commit is
/// atomic: new contents are staged in temporary sibling files and renamed into
/// place, and a mid-commit IO failure rolls the working tree back to its
/// pre-commit state.
pub async fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    // The change set is applied and formatted purely in memory first, so the
    // working tree only ever receives code the project filter already accepted.
    let result = prepare_staged_edits(repo_root, context_manager, &proposal.edits, None).await;
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }

    // Writing the validated change set, including creating parent directories,
    // is blocking file I/O. Run it through the blocking helper so a burst of
    // writes never stalls an async worker thread.
    crate::blocking::run_blocking(|| {
        commit_staged_edits(repo_root, &result.staged_state, &result.successful_paths)
    })?;

    // Reconcile the context layer with the committed change set strictly after
    // the disk commit succeeds. The net transition is derived directly from the
    // final staged state instead of replaying individual `Move` renames: every
    // path that no longer exists on disk is dropped, and every staged path is
    // ensured present and editable. Deriving the transition from the net state
    // keeps chained moves (`a -> b -> c`), moves whose source path is recreated,
    // and mixed operation sequences from desynchronizing context from disk.
    //
    // A path already present keeps the layer that owns it, and a path the turn
    // created joins the transient auto layer. The user layer is never widened by
    // a turn: it holds exactly the files the developer added by hand.
    crate::blocking::run_blocking(|| {
        for path in &result.staged_state.deleted_paths {
            let _ = context_manager.remove_file(path);
        }
        for path in result.staged_state.staged_files.keys() {
            if context_manager.contains(path).unwrap_or(false) {
                let _ = context_manager.update_file_metadata(path);
                let _ = context_manager.set_access(path, tauqe_protocol::ContextAccess::Editable);
            } else {
                // A path absent from context is a file the turn created (or a
                // move destination), so it widens the transient auto layer only.
                let _ =
                    context_manager.add_auto_file(path, tauqe_protocol::ContextAccess::Editable);
            }
        }
    });
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

    #[tokio::test]
    async fn partial_failure_rolls_back_entire_file_and_keeps_disk_unchanged() {
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
        assert!(apply_edit_proposal(root, &mut cm, &proposal).await.is_err());
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
            let result =
                stage_and_validate_edits(root, &cm, std::slice::from_ref(&operation), None);
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
        let result =
            stage_and_validate_edits(root, &cm, std::slice::from_ref(&dropped_verify), None);
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
}
