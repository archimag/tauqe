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
//! The answer separates the one target that degrades from the one that refuses.
//! A directory that is not a Git work tree has no ignore rules at all, so
//! nothing is ignored there and every ordinary write keeps its admission. A
//! probe that failed on a repository Git can inspect leaves the exclusion of
//! every path unknown, so the change set admits no write: only an uninspectable
//! repository degrades to admitting them, because there is nothing to guess
//! about.
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

/// The batched answer of the repository ignore rules for one change set.
///
/// One probe answers for the whole change set, so both admission branches read
/// the same value. The answer is typed rather than a bare set of ignored paths,
/// because a target Git cannot inspect and a probe Git refused admit opposite
/// writes.
#[derive(Debug)]
pub enum IgnoreAnswer {
    /// Git answered for the whole change set; its rules exclude these paths.
    Resolved(HashSet<String>),
    /// The target is not a Git work tree, so no rule can exclude a write.
    Uninspectable,
    /// Git was asked and failed, so the exclusion of every path is unknown.
    Failed(String),
}

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
///
/// An answer that resolved nothing admits nothing: a probe Git refused leaves
/// every exclusion unknown, and admitting a write on that guess would abort the
/// Git transaction after the bytes already reached disk. Such a write is
/// refused with [`EditError::IgnoreProbeFailed`], which names both the path and
/// Git's diagnostic, so the patch-retry loop reports the real cause instead of
/// a rejection it cannot explain.
pub fn admit_write(path: &str, answer: &IgnoreAnswer) -> Result<(), EditError> {
    let ignored = match answer {
        IgnoreAnswer::Resolved(ignored) => ignored.contains(path),
        IgnoreAnswer::Uninspectable => false,
        IgnoreAnswer::Failed(reason) => {
            return Err(EditError::IgnoreProbeFailed {
                path: path.to_string(),
                reason: reason.clone(),
            });
        }
    };
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
    answer: &IgnoreAnswer,
) -> Result<(), EditError> {
    crate::paths::verify_path_within_repo_for_write(repo_root, target)
        .map_err(|err| EditError::InvalidPath(target.to_string(), err.to_string()))?;
    admit_write(target, answer)
}
/// Resolves the batched ignore answer the admission of a change set reads.
///
/// A path Git refuses to stage can only be detected after the whole turn has
/// already applied its edits, which aborts the Git transaction instead of the
/// edit. Asking the question during staging keeps the failure inside the turn:
/// one batched `git check-ignore` call answers for the whole change set.
///
/// The answer separates the two reasons a probe can produce no verdict. A
/// directory that is not a Git work tree has no ignore rules, so nothing is
/// ignored there and every ordinary write keeps its admission. A probe that
/// failed on a repository Git can inspect leaves the exclusion of every path
/// unknown, so the change set admits no write: only an uninspectable repository
/// degrades to admitting them.
///
/// The candidates are the normalized write targets of the staging members, so a
/// directory move contributes one target per file it relocates and a member that
/// only rewrites content in place contributes none.
pub fn ignored_target_paths(repo_root: &Path, candidates: &[String]) -> IgnoreAnswer {
    if candidates.is_empty() {
        return IgnoreAnswer::Resolved(HashSet::new());
    }

    let failure = match crate::git::state::gitignored_paths(repo_root, candidates) {
        Ok(ignored) => return IgnoreAnswer::Resolved(ignored.into_iter().collect()),
        Err(err) => err,
    };

    // The two failure reasons are classified structurally, never by reading
    // Git's diagnostic: a directory that is not a work tree and a command that
    // was refused both surface as a non-zero exit status. A directory Git
    // cannot inspect is the documented degradation, so the question is asked of
    // Git itself, and only after the probe failed.
    if !crate::git::state::is_git_repository(Some(repo_root)) {
        return IgnoreAnswer::Uninspectable;
    }

    let reason = format!("{failure:#}");
    tracing::warn!(
        "The repository ignore probe failed for {} path(s), so no write is admitted: {}",
        candidates.len(),
        reason
    );
    IgnoreAnswer::Failed(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the answer of a probe that resolved the supplied ignored paths.
    fn resolved(paths: &[&str]) -> IgnoreAnswer {
        IgnoreAnswer::Resolved(paths.iter().map(|path| (*path).to_string()).collect())
    }

    #[test]
    fn a_resolved_answer_decides_both_admission_branches() {
        let answer = resolved(&["target/generated.rs", ".tauqe/mockups/draft.rs"]);

        // A path Git excludes can never be committed, so its write is refused.
        assert!(matches!(
            admit_write("target/generated.rs", &answer),
            Err(EditError::IgnoredByGit(path)) if path == "target/generated.rs"
        ));
        assert!(admit_write("src/lib.rs", &answer).is_ok());

        // The sandbox inverts the rule instead of escaping it: it is writable
        // exactly as long as the same answer covers it.
        assert!(admit_write(".tauqe/mockups/draft.rs", &answer).is_ok());
        assert!(matches!(
            admit_write(".tauqe/mockups/tracked.rs", &answer),
            Err(EditError::MockupsNotIgnored(path)) if path == ".tauqe/mockups/tracked.rs"
        ));
    }

    #[test]
    fn an_uninspectable_target_admits_an_ordinary_write_only() {
        // A directory outside a Git work tree has no ignore rules, so an
        // ordinary write keeps its admission.
        assert!(admit_write("src/generated.rs", &IgnoreAnswer::Uninspectable).is_ok());

        // The sandbox is admitted only while Git demonstrably ignores it, and a
        // target Git cannot inspect cannot demonstrate that.
        assert!(matches!(
            admit_write(".tauqe/mockups/draft.rs", &IgnoreAnswer::Uninspectable),
            Err(EditError::MockupsNotIgnored(path)) if path == ".tauqe/mockups/draft.rs"
        ));
    }

    #[test]
    fn a_refused_probe_admits_no_write_at_all() {
        let answer = IgnoreAnswer::Failed("git check-ignore failed".to_string());

        // Both branches read the same answer, so an unknown exclusion refuses
        // the ordinary write and the sandbox write alike.
        for path in ["src/generated.rs", ".tauqe/mockups/draft.rs"] {
            match admit_write(path, &answer) {
                Err(EditError::IgnoreProbeFailed {
                    path: refused,
                    reason,
                }) => {
                    assert_eq!(refused, path);
                    assert_eq!(reason, "git check-ignore failed");
                }
                other => panic!("expected a refused probe for '{path}', got {other:?}"),
            }
        }
    }
}
