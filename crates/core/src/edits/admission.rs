//! Admission control for the paths a change set may write.
//!
//! Git is the single source of truth for what a repository tracks, so a path
//! its ignore rules exclude can never be committed. Detecting that only after
//! the whole turn applied its edits aborts the Git transaction instead of the
//! edit, which is why the question is asked while the change set is still
//! purely in memory.
//!
//! One batched `git check-ignore` call answers for the whole change set, and
//! both admission branches read that one answer, so the admission and the
//! refusal can never disagree about what Git ignores. The mockups sandbox in
//! `.tauqe/mockups/` inverts the rule rather than escaping it: it is writable
//! exactly as long as the same answer covers it.

use std::collections::HashSet;
use std::path::Path;

use tauqe_protocol::EditOperation;

use crate::context::ContextManager;
use crate::mockups::is_mockups_path;

use super::error::EditError;

/// Decides whether a change set may write `path`, given the batched ignore
/// answer.
///
/// A path outside the mockups sandbox keeps the ordinary rule: a write Git
/// ignores is refused with [`EditError::IgnoredByGit`], because the change set
/// could never be committed. The sandbox inverts that rule rather than
/// escaping it: `.tauqe/mockups/` is writable exactly as long as the same
/// answer covers it, so a mockup Git would track is refused with
/// [`EditError::MockupsNotIgnored`] instead of entering a transaction it would
/// abort. Both branches read one answer, so the admission and the refusal can
/// never disagree about what Git ignores.
pub(super) fn admit_write(path: &str, ignored_paths: &HashSet<String>) -> Result<(), EditError> {
    let ignored = ignored_paths.contains(path);
    if is_mockups_path(path) {
        if ignored {
            Ok(())
        } else {
            Err(EditError::MockupsNotIgnored(path.to_string()))
        }
    } else if ignored {
        Err(EditError::IgnoredByGit(path.to_string()))
    } else {
        Ok(())
    }
}

/// Resolves the repository paths a change set would create, rewrite, or move to.
///
/// A path Git refuses to stage can only be detected after the whole turn has
/// already applied its edits, which aborts the Git transaction instead of the
/// edit. Asking the question during staging keeps the failure inside the turn:
/// one batched `git check-ignore` call answers for the whole change set, and a
/// repository Git cannot inspect degrades to "nothing is ignored" rather than
/// blocking an otherwise valid turn.
pub(super) fn ignored_target_paths(
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
