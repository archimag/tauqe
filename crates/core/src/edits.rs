use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use thiserror::Error;
use workbench_protocol::{EditOperation, EditProposal, ModelResult};

use crate::context::ContextManager;

pub mod protocol;
pub mod stream;

pub use protocol::{
    CustomSearchReplaceEditProtocol, EditProtocol, EditProtocolFactory, SearchReplaceMarkers,
    XmlEditProtocol,
};
pub use stream::XmlStreamFilter;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("File '{0}' is not in context. Only files explicitly in context can be edited.")]
    NotInContext(String),

    #[error("File '{0}' is read-only. Change its access to editable before applying edits.")]
    ReadOnly(String),

    #[error("File '{0}' does not exist on disk.")]
    FileNotFound(String),

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
    let mut result = StageEditsResult {
        staged_state: initial_staged.cloned().unwrap_or_default(),
        successful_paths: Vec::new(),
        successful_edits: Vec::new(),
        failed_files: Vec::new(),
    };
    let mut file_order = Vec::new();
    let mut grouped: HashMap<String, Vec<&EditOperation>> = HashMap::new();
    let mut invalid_paths = HashSet::new();

    for edit in edits {
        let path = match edit {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path } => path,
        };
        match context_manager.normalize_path(path) {
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
        }
    }

    for path in file_order {
        let operations = &grouped[&path];
        match stage_file_edits(
            repo_root,
            context_manager,
            &path,
            operations,
            &result.staged_state,
        ) {
            Ok(Some(content)) => {
                result.staged_state.deleted_paths.remove(&path);
                result.staged_state.staged_files.insert(path.clone(), content);
            }
            Ok(None) => {
                result.staged_state.staged_files.remove(&path);
                result.staged_state.deleted_paths.insert(path.clone());
            }
            Err(error) => {
                result.failed_files.push(FailedFileEdit { path, error });
                continue;
            }
        }
        result.successful_paths.push(path);
        result.successful_edits.extend(operations.iter().map(|edit| (**edit).clone()));
    }
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
    let invalid_path = |err: anyhow::Error| EditError::InvalidPath(path.to_string(), err.to_string());
    let in_context = context_manager.contains(path).map_err(invalid_path)?;
    let editable = context_manager.is_editable(path).map_err(invalid_path)?;
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
        if in_context && !editable {
            return Err(EditError::ReadOnly(path.to_string()));
        }
        if !in_context && !newly_created {
            match edit {
                EditOperation::Create { .. } if !full_path.exists() => {}
                _ => return Err(EditError::NotInContext(path.to_string())),
            }
        }

        match edit {
            EditOperation::Replace { old_text, new_text, .. } => {
                if !loaded {
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(path.to_string()));
                    }
                    content = Some(std::fs::read_to_string(&full_path)
                        .map_err(|err| EditError::Io(path.to_string(), err))?);
                    loaded = true;
                }
                let current = content.as_ref()
                    .ok_or_else(|| EditError::FileNotFound(path.to_string()))?;
                let (effective_old, effective_new) =
                    if current.contains("\r\n") && !old_text.contains("\r\n") {
                        (old_text.replace('\n', "\r\n"), new_text.replace('\n', "\r\n"))
                    } else if !current.contains("\r\n") && old_text.contains("\r\n") {
                        (old_text.replace("\r\n", "\n"), new_text.replace("\r\n", "\n"))
                    } else {
                        (old_text.clone(), new_text.clone())
                    };
                let matches: Vec<(usize, &str)> = current.match_indices(&effective_old).collect();
                if matches.is_empty() {
                    return Err(EditError::NoMatch { path: path.to_string() });
                }
                if matches.len() > 1 {
                    return Err(EditError::AmbiguousMatch {
                        path: path.to_string(),
                        count: matches.len(),
                    });
                }
                let match_idx = matches[0].0;
                let mut updated = String::with_capacity(
                    current.len() + effective_new.len().saturating_sub(effective_old.len()),
                );
                updated.push_str(&current[..match_idx]);
                updated.push_str(&effective_new);
                updated.push_str(&current[match_idx + effective_old.len()..]);
                content = Some(updated);
            }
            EditOperation::Create { content: new_content, .. } => {
                content = Some(new_content.clone());
                loaded = true;
                newly_created = !full_path.exists();
            }
            EditOperation::Delete { .. } => {
                content = None;
                loaded = true;
            }
        }
    }
    Ok(content)
}

/// Applies an EditProposal after validating every file in memory.
///
/// If ANY edit fails validation, no files on disk are modified. As before,
/// disk IO failures during commit are returned but are not transactionally rolled back.
pub fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    let result = stage_and_validate_edits(repo_root, context_manager, &proposal.edits, None);
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }

    // Commit only after all files have passed validation.
    for path in &result.successful_paths {
        let full_path = repo_root.join(path);
        if let Some(content) = result.staged_state.staged_files.get(path) {
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| EditError::Io(path.clone(), err))?;
            }
            std::fs::write(&full_path, content)
                .map_err(|err| EditError::Io(path.clone(), err))?;

            if context_manager.contains(path).unwrap_or(false) {
                let _ = context_manager.update_file_metadata(path);
            } else {
                let _ = context_manager.add_file(path, workbench_protocol::ContextAccess::Editable);
            }
        } else {
            match std::fs::remove_file(&full_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(EditError::Io(path.clone(), err)),
            }
            let _ = context_manager.remove_file(path);
        }
    }
    Ok(result.successful_paths)
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use tempfile::tempdir;
    use workbench_protocol::ContextAccess;

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
        assert!(matches!(&result.failed_files[0].error, EditError::NoMatch { .. }));
        assert_eq!(cm.get_state().revision, before);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(root.join("b.rs")).unwrap(), "two");

        let proposal = EditProposal { summary: "test".into(), edits };
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
            root, &cm, &[replace("a.rs", "two", "three")], Some(&first.staged_state),
        );
        assert!(second.is_success());
        assert_eq!(second.staged_state.staged_files["a.rs"], "three");
        let failed = stage_and_validate_edits(
            root, &cm, &[replace("a.rs", "missing", "four")], Some(&second.staged_state),
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
        let create = EditOperation::Create { path: "new.rs".into(), content: "one".into() };
        let delete = EditOperation::Delete { path: "new.rs".into() };
        let first = stage_and_validate_edits(root, &cm, &[create.clone()], None);
        let second = stage_and_validate_edits(
            root, &cm, &[replace("new.rs", "one", "two")], Some(&first.staged_state),
        );
        assert!(second.is_success());
        assert_eq!(second.staged_state.staged_files["new.rs"], "two");
        let deleted = stage_and_validate_edits(
            root, &cm, &[delete.clone()], Some(&second.staged_state),
        );
        assert!(deleted.is_success());
        assert!(deleted.staged_state.deleted_paths.contains("new.rs"));
        assert!(!deleted.staged_state.staged_files.contains_key("new.rs"));
        assert!(!root.join("new.rs").exists());

        let proposal = EditProposal {
            summary: "create".into(),
            edits: vec![create.clone(), delete, create, replace("new.rs", "one", "final")],
        };
        assert_eq!(apply_edit_proposal(root, &mut cm, &proposal).unwrap(), vec!["new.rs"]);
        assert_eq!(std::fs::read_to_string(root.join("new.rs")).unwrap(), "final");
        assert!(cm.is_editable("new.rs").unwrap());
        let deletion = EditProposal {
            summary: "delete".into(),
            edits: vec![EditOperation::Delete { path: "new.rs".into() }],
        };
        apply_edit_proposal(root, &mut cm, &deletion).unwrap();
        assert!(!root.join("new.rs").exists());
        assert!(!cm.contains("new.rs").unwrap());
    }

    #[test]
    fn permissions_paths_missing_files_and_ambiguous_matches_are_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("readonly.rs"), "one").unwrap();
        std::fs::write(root.join("outside.rs"), "one").unwrap();
        std::fs::write(root.join("ambiguous.rs"), "one one").unwrap();
        std::fs::write(root.join("missing.rs"), "one").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("readonly.rs", ContextAccess::ReadOnly).unwrap();
        cm.add_file("ambiguous.rs", ContextAccess::Editable).unwrap();
        cm.add_file("missing.rs", ContextAccess::Editable).unwrap();
        std::fs::remove_file(root.join("missing.rs")).unwrap();
        for edit in [
            replace("readonly.rs", "one", "two"),
            EditOperation::Create { path: "readonly.rs".into(), content: "two".into() },
            EditOperation::Delete { path: "readonly.rs".into() },
        ] {
            let result = stage_and_validate_edits(root, &cm, &[edit], None);
            assert!(matches!(&result.failed_files[0].error, EditError::ReadOnly(_)));
        }
        for edit in [
            replace("outside.rs", "one", "two"),
            EditOperation::Create { path: "outside.rs".into(), content: "two".into() },
            EditOperation::Delete { path: "outside.rs".into() },
        ] {
            let result = stage_and_validate_edits(root, &cm, &[edit], None);
            assert!(matches!(&result.failed_files[0].error, EditError::NotInContext(_)));
        }
        let ambiguous = stage_and_validate_edits(root, &cm, &[replace("ambiguous.rs", "one", "two")], None);
        assert!(matches!(&ambiguous.failed_files[0].error, EditError::AmbiguousMatch { count: 2, .. }));
        let missing = stage_and_validate_edits(root, &cm, &[replace("missing.rs", "one", "two")], None);
        assert!(matches!(&missing.failed_files[0].error, EditError::FileNotFound(_)));
        let invalid = stage_and_validate_edits(root, &cm, &[replace("../escape.rs", "one", "two")], None);
        assert!(matches!(&invalid.failed_files[0].error, EditError::InvalidPath(_, _)));
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
}

#[derive(Debug, Deserialize)]
struct RawProposal {
    #[serde(default)]
    summary: Option<String>,
    edits: Vec<EditOperation>,
}

pub fn parse_workbench_edit_json(raw_text: &str) -> Option<ModelResult> {
    if let Some(content) = extract_fenced_block(raw_text, "workbench_edit") {
        match serde_json::from_str::<RawProposal>(&content) {
            Ok(raw) => {
                if !raw.edits.is_empty() {
                    return Some(ModelResult::Edit {
                        summary: raw
                            .summary
                            .unwrap_or_else(|| "Applied code edits".to_string()),
                        edits: raw.edits,
                        proposal: None,
                        applied: false,
                        error: None,
                        changed_files: Vec::new(),
                        commit_hash: None,
                    });
                }
            }
            Err(err) => {
                return Some(ModelResult::Edit {
                    summary: "Malformed workbench_edit JSON".to_string(),
                    edits: Vec::new(),
                    proposal: None,
                    applied: false,
                    error: Some(format!("Invalid JSON in workbench_edit: {}", err)),
                    changed_files: Vec::new(),
                    commit_hash: None,
                });
            }
        }
    }
    None
}

pub(crate) fn extract_fenced_block(text: &str, tag: &str) -> Option<String> {
    let start_tag = format!("```{}", tag);
    let mut search_from = 0;
    while let Some(start_idx) = text[search_from..].find(&start_tag) {
        let actual_start = search_from + start_idx + start_tag.len();
        let content_start = if text[actual_start..].starts_with("\r\n") {
            actual_start + 2
        } else if text[actual_start..].starts_with('\n') {
            actual_start + 1
        } else {
            actual_start
        };

        if let Some(end_idx) = text[content_start..].find("```") {
            let content = &text[content_start..content_start + end_idx];
            return Some(content.trim().to_string());
        } else {
            search_from = actual_start;
        }
    }
    None
}
