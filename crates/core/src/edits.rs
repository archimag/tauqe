use std::collections::{HashMap, HashSet};
use std::path::Path;
use serde::Deserialize;
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

/// Applies an EditProposal atomically.
///
/// If ANY edit fails validation (e.g. read-only file, old_text not found, ambiguous match),
/// the entire operation is safely rejected and no files on disk are modified.
pub fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    if proposal.edits.is_empty() {
        return Ok(Vec::new());
    }

    // Stage file contents and deletions in memory for atomic validation
    let mut staged_files: HashMap<String, String> = HashMap::new();
    let mut deleted_paths: Vec<String> = Vec::new();
    let mut changed_paths: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for edit in &proposal.edits {
        match edit {
            EditOperation::Replace {
                path,
                old_text,
                new_text,
            } => {
                let clean_path = context_manager
                    .normalize_path(path)
                    .map_err(|e| EditError::InvalidPath(path.clone(), e.to_string()))?;

                if !context_manager
                    .contains(&clean_path)
                    .map_err(|e| EditError::InvalidPath(clean_path.clone(), e.to_string()))?
                {
                    return Err(EditError::NotInContext(clean_path));
                }

                if !context_manager
                    .is_editable(&clean_path)
                    .map_err(|e| EditError::InvalidPath(clean_path.clone(), e.to_string()))?
                {
                    return Err(EditError::ReadOnly(clean_path));
                }

                let current_content = if let Some(staged) = staged_files.get(&clean_path) {
                    staged.clone()
                } else {
                    let full_path = repo_root.join(&clean_path);
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(clean_path.clone()));
                    }
                    std::fs::read_to_string(&full_path)
                        .map_err(|err| EditError::Io(clean_path.clone(), err))?
                };

                let (effective_content, effective_old, effective_new) =
                    if current_content.contains("\r\n") && !old_text.contains("\r\n") {
                        (
                            current_content,
                            old_text.replace('\n', "\r\n"),
                            new_text.replace('\n', "\r\n"),
                        )
                    } else if !current_content.contains("\r\n") && old_text.contains("\r\n") {
                        (
                            current_content,
                            old_text.replace("\r\n", "\n"),
                            new_text.replace("\r\n", "\n"),
                        )
                    } else {
                        (current_content, old_text.clone(), new_text.clone())
                    };

                let matches: Vec<(usize, &str)> =
                    effective_content.match_indices(&effective_old).collect();
                if matches.is_empty() {
                    return Err(EditError::NoMatch { path: clean_path });
                }
                if matches.len() > 1 {
                    return Err(EditError::AmbiguousMatch {
                        path: clean_path,
                        count: matches.len(),
                    });
                }

                let (match_idx, _) = matches[0];
                let mut updated_content = String::with_capacity(
                    effective_content.len()
                        + effective_new.len().saturating_sub(effective_old.len()),
                );
                updated_content.push_str(&effective_content[..match_idx]);
                updated_content.push_str(&effective_new);
                updated_content.push_str(&effective_content[match_idx + effective_old.len()..]);

                staged_files.insert(clean_path.clone(), updated_content);

                if seen_paths.insert(clean_path.clone()) {
                    changed_paths.push(clean_path);
                }
            }
            EditOperation::Create { path, content } => {
                let clean_path = context_manager
                    .normalize_path(path)
                    .map_err(|e| EditError::InvalidPath(path.clone(), e.to_string()))?;

                if context_manager.contains(&clean_path).unwrap_or(false)
                    && !context_manager.is_editable(&clean_path).unwrap_or(false)
                {
                    return Err(EditError::ReadOnly(clean_path));
                }
                staged_files.insert(clean_path.clone(), content.clone());
                if seen_paths.insert(clean_path.clone()) {
                    changed_paths.push(clean_path);
                }
            }
            EditOperation::Delete { path } => {
                let clean_path = context_manager
                    .normalize_path(path)
                    .map_err(|e| EditError::InvalidPath(path.clone(), e.to_string()))?;

                if context_manager.contains(&clean_path).unwrap_or(false)
                    && !context_manager.is_editable(&clean_path).unwrap_or(false)
                {
                    return Err(EditError::ReadOnly(clean_path));
                }
                deleted_paths.push(clean_path.clone());
                if seen_paths.insert(clean_path.clone()) {
                    changed_paths.push(clean_path);
                }
            }
        }
    }

    // Atomic disk commit
    for (path, content) in &staged_files {
        let full_path = repo_root.join(path);
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&full_path, content).map_err(|err| EditError::Io(path.clone(), err))?;

        if context_manager.contains(path).unwrap_or(false) {
            let _ = context_manager.update_file_metadata(path);
        } else {
            let _ = context_manager.add_file(path, workbench_protocol::ContextAccess::Editable);
        }
    }

    for path in &deleted_paths {
        let full_path = repo_root.join(path);
        if full_path.exists() {
            let _ = std::fs::remove_file(&full_path);
        }
        let _ = context_manager.remove_file(path);
    }

    Ok(changed_paths)
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
                        summary: raw.summary.unwrap_or_else(|| "Applied code edits".to_string()),
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
