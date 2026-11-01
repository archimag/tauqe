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
//!
//! A directory move writes no directory: it stages one destination per file the
//! subtree contains, so each of them is admitted as the ordinary write it is.
//! The repository-root containment of those destinations is proved in the same
//! pass, before the first file of the subtree stages, so a destination that
//! would resolve outside the repository is refused instead of leaving a
//! half-moved directory behind.
use std::collections::HashSet;
use std::path::Path;

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
pub fn admit_write(path: &str, ignored_paths: &HashSet<String>) -> Result<(), EditError> {
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

/// Admits one destination of a directory move.
///
/// A directory move relocates a whole subtree, so every destination it expands
/// into is a write target even though the model named only two directories. The
/// containment check runs here, before the first file of the subtree stages,
/// because an escape discovered halfway through the expansion would have to be
/// undone instead of refused: the write admission fails closed while the
/// repository root cannot be canonicalized, and accepts only a path that really
/// resolves inside the repository. The batched ignore answer then decides the
/// same destination exactly as it decides an explicit write.
pub fn admit_move_destination(
    repo_root: &Path,
    target: &str,
    ignored_paths: &HashSet<String>,
) -> Result<(), EditError> {
    crate::paths::verify_path_within_repo_for_write(repo_root, target)
        .map_err(|err| EditError::InvalidPath(target.to_string(), err.to_string()))?;
    admit_write(target, ignored_paths)
}
/// Resolves which of the supplied write targets the repository ignore rules exclude.
///
/// A path Git refuses to stage can only be detected after the whole turn has
/// already applied its edits, which aborts the Git transaction instead of the
/// edit. Asking the question during staging keeps the failure inside the turn:
/// one batched `git check-ignore` call answers for the whole change set, and a
/// repository Git cannot inspect degrades to "nothing is ignored" rather than
/// blocking an otherwise valid turn.
///
/// The candidates are the normalized write targets of the staging members, so a
/// directory move contributes one target per file it relocates and a member that
/// only rewrites content in place contributes none.
pub fn ignored_target_paths(repo_root: &Path, candidates: &[String]) -> HashSet<String> {
    if candidates.is_empty() {
        return HashSet::new();
    }

    match crate::git::state::gitignored_paths(repo_root, candidates) {
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
