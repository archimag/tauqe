use std::collections::{HashMap, HashSet};
use tauqe_protocol::EditOperation;

use crate::context::ContextManager;
use crate::edits::{EditError, StagedEditsState};

/// Formats a human-readable failure reason for `EditFileRetrying` event and prompts.
pub fn format_patch_retry_reason(err: &EditError) -> String {
    match err {
        EditError::NoMatch { .. } => {
            "Search block not found (0 matches). Check indentation and line endings.".to_string()
        }
        EditError::AmbiguousMatch { count, .. } => {
            format!(
                "Search block matches {} times. Provide more unique context.",
                count
            )
        }
        other => other.to_string(),
    }
}

/// Constructs a specialized prompt asking the model to fix failed edit blocks.
pub fn build_patch_retry_prompt(
    succeeded_files: &HashSet<String>,
    failed_files: &HashMap<String, EditError>,
    target_language: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Some of the proposed edits failed to apply.\n\n");

    if !succeeded_files.is_empty() {
        let mut succ_sorted: Vec<&String> = succeeded_files.iter().collect();
        succ_sorted.sort();
        prompt.push_str("Successfully applied edits for:\n");
        for path in succ_sorted {
            prompt.push_str(&format!("- {}\n", path));
        }
        prompt.push_str("(Do NOT regenerate changes for the above files; their updated versions are already present in <context>).\n\n");
    }

    prompt.push_str("Failed files and errors:\n");
    let mut failed_sorted: Vec<(&String, &EditError)> = failed_files.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        let explanation = match err {
            EditError::NoMatch { .. } => {
                "Search block not found (0 matches). Verify exact line content, indentation, and context."
            }
            EditError::AmbiguousMatch { count, .. } => {
                &format!(
                    "Search block matches {} times. Include more surrounding lines to make the match uniquely identifiable.",
                    count
                )
            }
            other => &other.to_string(),
        };
        prompt.push_str(&format!("- `{}`: {}\n", path, explanation));
    }

    prompt.push_str("\nPlease provide corrected edit blocks ONLY for the failed files listed above.");
    if let Some(lang) = target_language {
        prompt.push_str(&format!(
            "\n\nNote: Formulate your explanations in {} (the language of the user's request), while conducting all reasoning strictly in English.",
            lang
        ));
    }
    prompt
}

/// Filters out failed files that the model chose not to retry in the current set of edits.
pub fn retain_retried_failed_files(
    last_failed_errors: &mut HashMap<String, EditError>,
    context_manager: &ContextManager,
    edits: &[EditOperation],
) {
    let mut retried_paths = HashSet::new();
    for edit in edits {
        match edit {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path }
            | EditOperation::Overwrite { path, .. } => {
                let clean = context_manager.normalize_path(path).unwrap_or_else(|_| path.clone());
                retried_paths.insert(clean);
            }
            EditOperation::Move { from, to } => {
                let clean_from = context_manager.normalize_path(from).unwrap_or_else(|_| from.clone());
                let clean_to = context_manager.normalize_path(to).unwrap_or_else(|_| to.clone());
                retried_paths.insert(clean_from);
                retried_paths.insert(clean_to);
            }
        }
    }
    last_failed_errors.retain(|path, _| retried_paths.contains(path));
}

/// Evicts any files that are being re-edited in the current retry from previously staged state
/// and cumulative edits, so they can be freshly staged and validated against their clean base.
pub fn prepare_staged_for_retry(
    staged_state: &mut StagedEditsState,
    cumulative_edits: &mut Vec<EditOperation>,
    succeeded_files: &mut HashSet<String>,
    context_manager: &ContextManager,
    new_edits: &[EditOperation],
) {
    let mut touched_paths = HashSet::new();
    for edit in new_edits {
        match edit {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path }
            | EditOperation::Overwrite { path, .. } => {
                let clean = context_manager
                    .normalize_path(path)
                    .unwrap_or_else(|_| path.clone());
                touched_paths.insert(clean);
            }
            EditOperation::Move { from, to } => {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
                touched_paths.insert(clean_from);
                touched_paths.insert(clean_to);
            }
        }
    }

    for path in &touched_paths {
        staged_state.staged_files.remove(path);
        staged_state.deleted_paths.remove(path);
        succeeded_files.remove(path);
    }

    cumulative_edits.retain(|op| {
        let op_path = match op {
            EditOperation::Replace { path, .. }
            | EditOperation::Create { path, .. }
            | EditOperation::Delete { path }
            | EditOperation::Overwrite { path, .. } => context_manager
                .normalize_path(path)
                .unwrap_or_else(|_| path.clone()),
            EditOperation::Move { from, to } => {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
                if touched_paths.contains(&clean_from) || touched_paths.contains(&clean_to) {
                    return false;
                }
                return true;
            }
        };
        !touched_paths.contains(&op_path)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_patch_retry_prompt_format() {
        let mut succeeded = HashSet::new();
        succeeded.insert("crates/core/src/lib.rs".to_string());

        let mut failed = HashMap::new();
        failed.insert(
            "crates/core/src/main.rs".to_string(),
            EditError::NoMatch {
                path: "crates/core/src/main.rs".to_string(),
            },
        );
        failed.insert(
            "crates/core/src/utils.rs".to_string(),
            EditError::AmbiguousMatch {
                path: "crates/core/src/utils.rs".to_string(),
                count: 3,
            },
        );

        let prompt = build_patch_retry_prompt(&succeeded, &failed, Some("Russian"));

        assert!(prompt.contains("Russian"));
        assert!(prompt.contains("Successfully applied edits for:"));
        assert!(prompt.contains("- crates/core/src/lib.rs"));
        assert!(prompt.contains("(Do NOT regenerate changes for the above files; their updated versions are already present in <context>)."));

        assert!(prompt.contains("Failed files and errors:"));
        assert!(prompt.contains("crates/core/src/main.rs"));
        assert!(prompt.contains("Search block not found (0 matches)"));

        assert!(prompt.contains("crates/core/src/utils.rs"));
        assert!(prompt.contains("Search block matches 3 times"));

        assert!(prompt.contains(
            "Please provide corrected edit blocks ONLY for the failed files listed above."
        ));
    }

    #[test]
    fn test_retain_retried_failed_files_drops_unretried_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cm = ContextManager::new(dir.path().to_path_buf());
        let mut failed = HashMap::new();
        failed.insert("a.rs".to_string(), EditError::ReadOnly("a.rs".to_string()));
        failed.insert("b.rs".to_string(), EditError::NoMatch { path: "b.rs".to_string() });

        let edits = vec![EditOperation::Replace {
            path: "b.rs".to_string(),
            old_text: "old".to_string(),
            new_text: "new".to_string(),
        }];

        retain_retried_failed_files(&mut failed, &cm, &edits);
        assert!(!failed.contains_key("a.rs"));
        assert!(failed.contains_key("b.rs"));

        retain_retried_failed_files(&mut failed, &cm, &[]);
        assert!(failed.is_empty());
    }

    #[test]
    fn test_prepare_staged_for_retry_evicts_modified_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cm = ContextManager::new(dir.path().to_path_buf());
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("a.rs".to_string(), "staged a".to_string());
        staged
            .staged_files
            .insert("b.rs".to_string(), "staged b".to_string());

        let mut cumulative = vec![
            EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "old a".to_string(),
                new_text: "staged a".to_string(),
            },
            EditOperation::Replace {
                path: "b.rs".to_string(),
                old_text: "old b".to_string(),
                new_text: "staged b".to_string(),
            },
        ];

        let mut succeeded = HashSet::new();
        succeeded.insert("a.rs".to_string());
        succeeded.insert("b.rs".to_string());

        let retry_edits = vec![EditOperation::Replace {
            path: "a.rs".to_string(),
            old_text: "old a".to_string(),
            new_text: "fresh a".to_string(),
        }];

        prepare_staged_for_retry(
            &mut staged,
            &mut cumulative,
            &mut succeeded,
            &cm,
            &retry_edits,
        );

        assert!(!staged.staged_files.contains_key("a.rs"));
        assert!(staged.staged_files.contains_key("b.rs"));
        assert_eq!(cumulative.len(), 1);
        assert!(matches!(&cumulative[0], EditOperation::Replace { path, .. } if path == "b.rs"));
        assert!(!succeeded.contains("a.rs"));
        assert!(succeeded.contains("b.rs"));
    }
}
