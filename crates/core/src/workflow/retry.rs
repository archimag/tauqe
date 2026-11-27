use crate::edits::EditCreate;
use crate::edits::EditDelete;
use crate::edits::EditLineAnchor;
use crate::edits::EditMove;
use crate::edits::EditMoveDir;
use crate::edits::EditOperation;
use crate::edits::EditOverwrite;
use crate::edits::EditReplace;
use crate::workflow::PatchStrategy;
use std::collections::{HashMap, HashSet};

use crate::context::ContextManager;
use crate::edits::strategy::StrategyProfile;
use crate::edits::EditError;

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
    patch_strategy: PatchStrategy,
) -> String {
    let profile = StrategyProfile::of(patch_strategy);
    let source = "Authoritative file versions are supplied in this request: the original <file> content in <context>, with content, anchors, access, and availability overridden by <current_versions> where present. Paths without an override retain their original context version. This file view includes accepted virtual changes. The rejected edit text is diagnostic material, not a file version. A context request registers a path or increases its access; an already satisfied request does not supply another file version.";
    if patch_strategy == PatchStrategy::ApplyPatch {
        let errors = failed_files
            .values()
            .map(|error| profile.describe_failure(error))
            .chain(protocol_errors.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n");
        return format!(
            "Patch staging failed: {errors}\n\n{source}\n\n{}",
            profile.retry_instruction
        );
    }
    let mut prompt = String::new();
    prompt.push_str("Some of the proposed edits failed virtual staging.\n\n");
    prompt.push_str(source);
    prompt.push_str("\n\n");

    if !succeeded_files.is_empty() {
        let mut succ_sorted: Vec<&String> = succeeded_files.iter().collect();
        succ_sorted.sort();
        prompt.push_str("Accepted virtual edits for:\n");
        for path in succ_sorted {
            prompt.push_str(&format!("- {}\n", path));
        }
        prompt.push_str("These edits are retained in the virtual state; acceptance here does not mean they have been written to disk.\n\n");
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
        prompt.push_str("\nRequired result: corrected edit blocks conforming to the active protocol and authoritative file versions.");
    } else {
        prompt.push('\n');
        prompt.push_str(profile.retry_instruction);
    }
    prompt
}

/// Filters out failed files that the model chose not to retry in the current set of edits.
///
/// The removed rejections are returned rather than discarded. A path the model
/// left out of its corrected set was never staged, so its edit is not part of
/// the change set that will be applied: removing the entry only stops the
/// harness from asking for that file again, and the caller stays responsible
/// for reporting the omission.
pub fn retain_retried_failed_files(
    last_failed_errors: &mut HashMap<String, EditError>,
    context_manager: &ContextManager,
    edits: &[EditOperation],
) -> HashMap<String, EditError> {
    let mut retried_paths = HashSet::new();
    for edit in edits {
        match edit {
            EditOperation::Replace(EditReplace { path, .. })
            | EditOperation::Create(EditCreate { path, .. })
            | EditOperation::Delete(EditDelete { path })
            | EditOperation::Overwrite(EditOverwrite { path, .. })
            | EditOperation::LineAnchor(EditLineAnchor { path, .. }) => {
                let clean = context_manager
                    .normalize_path(path)
                    .unwrap_or_else(|_| path.clone());
                retried_paths.insert(clean);
            }
            EditOperation::Move(EditMove { from, to })
            | EditOperation::MoveDir(EditMoveDir { from, to }) => {
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
    let recorded: Vec<String> = last_failed_errors.keys().cloned().collect();
    let mut dropped = HashMap::new();
    for path in recorded {
        if retried_paths.contains(&path) {
            continue;
        }
        if let Some(error) = last_failed_errors.remove(&path) {
            dropped.insert(path, error);
        }
    }
    dropped
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
            PatchStrategy::SearchReplace,
        );

        assert!(prompt.contains("Protocol parsing errors encountered in your edit blocks:"));
        assert!(prompt.contains("Malformed XML edit block: Unclosed tag"));

        assert!(prompt.contains("Accepted virtual edits for:"));
        assert!(prompt.contains("- crates/core/src/lib.rs"));
        assert!(prompt.contains("does not mean they have been written to disk"));

        assert!(prompt.contains("Failed files and errors:"));
        assert!(prompt.contains("crates/core/src/main.rs"));
        assert!(prompt.contains("Search block not found (0 matches)"));

        assert!(prompt.contains("crates/core/src/utils.rs"));
        assert!(prompt.contains("Search block matches 3 times"));

        assert!(prompt.contains("Required result: corrected edit blocks for the failed files."));
    }

    #[test]
    fn repair_prompts_state_the_available_versions_and_required_result() {
        let failed = HashMap::from([(
            "a.rs".to_string(),
            EditError::NoMatch {
                path: "a.rs".to_string(),
            },
        )]);
        for strategy in [
            PatchStrategy::SearchReplace,
            PatchStrategy::LineAnchor,
            PatchStrategy::ApplyPatch,
        ] {
            let prompt = build_patch_retry_prompt(&HashSet::new(), &failed, &[], strategy);
            assert!(
                prompt.contains("supplied in this request"),
                "{strategy}: {prompt}"
            );
            assert!(prompt.contains("<file>"));
            assert!(prompt.contains("<current_versions>"));
            assert!(prompt.contains("retain their original context version"));
            assert!(prompt.contains("accepted virtual changes"));
            assert!(prompt.contains("diagnostic material, not a file version"));
            assert!(prompt.contains("Required result:"));
            assert!(!prompt.to_lowercase().contains("re-read"));
            if strategy == PatchStrategy::ApplyPatch {
                assert!(prompt.contains("The refused patch was not applied."));
                assert!(prompt.contains("a complete corrected patch"));
            }
        }
    }

    #[test]
    fn test_retain_retried_failed_files_reports_unretried_paths() {
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

        let edits = vec![EditOperation::Replace(EditReplace {
            path: "b.rs".to_string(),
            old_text: "old".to_string(),
            new_text: "new".to_string(),
        })];

        let unretried = retain_retried_failed_files(&mut failed, &cm, &edits);
        assert!(!failed.contains_key("a.rs"));
        assert!(failed.contains_key("b.rs"));
        assert_eq!(unretried.len(), 1);
        assert!(
            unretried.contains_key("a.rs"),
            "an unretried failure is reported instead of being discarded"
        );

        let unretried = retain_retried_failed_files(&mut failed, &cm, &[]);
        assert!(failed.is_empty());
        assert!(
            unretried.contains_key("b.rs"),
            "a failure left out of an empty retry set is reported as well"
        );
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

        let prompt =
            build_patch_retry_prompt(&HashSet::new(), &failed, &[], PatchStrategy::LineAnchor);

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
