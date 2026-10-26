use std::collections::{HashMap, HashSet};
use tauqe_protocol::{EditOperation, PatchStrategy};

use crate::context::ContextManager;
use crate::edits::strategy::StrategyProfile;
use crate::edits::{EditError, StagedEditsState};

/// Formats a human-readable failure reason for `EditFileRetrying` event and prompts.
///
/// The active patch strategy renders the wording, so the retrying event and the
/// retry prompt state one reason in the dialect the turn actually speaks. A
/// single renderer also keeps the two surfaces from drifting into divergent
/// paraphrases of the same failure.
pub fn format_patch_retry_reason(err: &EditError, patch_strategy: PatchStrategy) -> String {
    StrategyProfile::of(patch_strategy).describe_failure(err)
}

/// Constructs a specialized prompt asking the model to fix failed edit blocks.
///
/// Every reason and the closing correction guidance are rendered by the active
/// patch strategy, so a line-anchor turn is never taught the vocabulary of a
/// search/replace turn: the prompt corrects the addresses the turn actually
/// sent instead of asking for a field the strategy does not have.
pub fn build_patch_retry_prompt(
    succeeded_files: &HashSet<String>,
    failed_files: &HashMap<String, EditError>,
    protocol_errors: &[String],
    target_language: Option<&str>,
    patch_strategy: PatchStrategy,
) -> String {
    let profile = StrategyProfile::of(patch_strategy);
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

    if !protocol_errors.is_empty() {
        prompt.push_str("Protocol parsing errors encountered in your edit blocks:\n");
        for err in protocol_errors {
            prompt.push_str(&format!("- {}\n", err));
        }
        prompt.push('\n');
    }

    if !failed_files.is_empty() {
        prompt.push_str("Failed files and errors:\n");
        let mut failed_sorted: Vec<(&String, &EditError)> = failed_files.iter().collect();
        failed_sorted.sort_by_key(|(p, _)| *p);
        for (path, err) in failed_sorted {
            prompt.push_str(&format!(
                "- `{}`: {}\n",
                path,
                profile.describe_failure(err)
            ));
        }
    }

    if failed_files.is_empty() {
        prompt.push_str("\nPlease provide corrected edit blocks strictly following the required protocol format.");
    } else {
        prompt.push_str(
            "\nPlease provide corrected edit blocks ONLY for the failed files listed above.\n",
        );
        prompt.push_str(profile.retry_instruction);
    }
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
            | EditOperation::Overwrite { path, .. }
            | EditOperation::LineAnchor { path, .. } => {
                let clean = context_manager
                    .normalize_path(path)
                    .unwrap_or_else(|_| path.clone());
                retried_paths.insert(clean);
            }
            EditOperation::Move { from, to } => {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
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
            | EditOperation::Overwrite { path, .. }
            | EditOperation::LineAnchor { path, .. } => {
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

    // A Move operation owns two paths: rolled back `from` leaves a deleted
    // entry while `to` holds staged content. When a retry touches only one side
    // of such a move, expand the touched set to both sides so neither a stale
    // `deleted_paths` nor a stale `staged_files` entry survives the rollback.
    let mut changed = true;
    while changed {
        changed = false;
        for op in cumulative_edits.iter() {
            if let EditOperation::Move { from, to } = op {
                let clean_from = context_manager
                    .normalize_path(from)
                    .unwrap_or_else(|_| from.clone());
                let clean_to = context_manager
                    .normalize_path(to)
                    .unwrap_or_else(|_| to.clone());
                if touched_paths.contains(&clean_from) || touched_paths.contains(&clean_to) {
                    changed |= touched_paths.insert(clean_from);
                    changed |= touched_paths.insert(clean_to);
                }
            }
        }
    }

    for path in &touched_paths {
        staged_state.staged_files.remove(path);
        staged_state.deleted_paths.remove(path);
        succeeded_files.remove(path);
    }

    cumulative_edits.retain(|op| match op {
        EditOperation::Replace { path, .. }
        | EditOperation::Create { path, .. }
        | EditOperation::Delete { path }
        | EditOperation::Overwrite { path, .. }
        | EditOperation::LineAnchor { path, .. } => {
            let op_path = context_manager
                .normalize_path(path)
                .unwrap_or_else(|_| path.clone());
            !touched_paths.contains(&op_path)
        }
        EditOperation::Move { from, to } => {
            let clean_from = context_manager
                .normalize_path(from)
                .unwrap_or_else(|_| from.clone());
            let clean_to = context_manager
                .normalize_path(to)
                .unwrap_or_else(|_| to.clone());
            !touched_paths.contains(&clean_from) && !touched_paths.contains(&clean_to)
        }
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

        let proto_errors = vec!["Malformed XML edit block: Unclosed tag".to_string()];
        let prompt = build_patch_retry_prompt(
            &succeeded,
            &failed,
            &proto_errors,
            Some("Russian"),
            PatchStrategy::SearchReplace,
        );

        assert!(prompt.contains("Protocol parsing errors encountered in your edit blocks:"));
        assert!(prompt.contains("Malformed XML edit block: Unclosed tag"));

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
        failed.insert(
            "a.rs".to_string(),
            EditError::NotInContext("a.rs".to_string()),
        );
        failed.insert(
            "b.rs".to_string(),
            EditError::NoMatch {
                path: "b.rs".to_string(),
            },
        );

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

    #[test]
    fn test_prepare_staged_for_retry_expands_move_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cm = ContextManager::new(dir.path().to_path_buf());
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("b.rs".to_string(), "content of a".to_string());
        staged.deleted_paths.insert("a.rs".to_string());

        let mut cumulative = vec![
            EditOperation::Move {
                from: "a.rs".to_string(),
                to: "b.rs".to_string(),
            },
            EditOperation::Replace {
                path: "c.rs".to_string(),
                old_text: "old".to_string(),
                new_text: "new".to_string(),
            },
        ];

        let mut succeeded = HashSet::new();
        succeeded.insert("a.rs".to_string());
        succeeded.insert("b.rs".to_string());
        succeeded.insert("c.rs".to_string());

        // Retry touches only the `to` side of the earlier move.
        let retry_edits = vec![EditOperation::Replace {
            path: "b.rs".to_string(),
            old_text: "content of a".to_string(),
            new_text: "fixed".to_string(),
        }];

        prepare_staged_for_retry(
            &mut staged,
            &mut cumulative,
            &mut succeeded,
            &cm,
            &retry_edits,
        );

        assert!(!staged.staged_files.contains_key("b.rs"));
        assert!(!staged.deleted_paths.contains("a.rs"));
        assert_eq!(cumulative.len(), 1);
        assert!(matches!(&cumulative[0], EditOperation::Replace { path, .. } if path == "c.rs"));
        assert!(!succeeded.contains("a.rs"));
        assert!(!succeeded.contains("b.rs"));
        assert!(succeeded.contains("c.rs"));
    }
    #[test]
    fn line_anchor_retry_prompt_speaks_the_anchor_dialect() {
        let failed = HashMap::from([
            (
                "src/lib.rs".to_string(),
                EditError::AnchorOutOfRange {
                    path: "src/lib.rs".to_string(),
                    anchor: "kR7t".to_string(),
                    lines: 12,
                },
            ),
            (
                "src/other.rs".to_string(),
                EditError::OverlappingAnchorRanges {
                    path: "src/other.rs".to_string(),
                    start: "kR7t".to_string(),
                    end: "Qw2n".to_string(),
                    other_start: "ZbY9".to_string(),
                    other_end: "mDgH".to_string(),
                },
            ),
        ]);

        let prompt = build_patch_retry_prompt(
            &HashSet::new(),
            &failed,
            &[],
            None,
            PatchStrategy::LineAnchor,
        );

        // The anchored contour addresses lines by anchors, so the retry request
        // must never teach the model a field it was never asked to send.
        for foreign in ["old_text", "new_text", "Search block"] {
            assert!(
                !prompt.contains(foreign),
                "the anchored retry prompt named {foreign}: {prompt}"
            );
        }
        assert!(prompt.contains("`start` and `end` anchors"), "{prompt}");
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("names no line of the file"));
        assert!(prompt.contains("disjoint line ranges"));
    }
}
