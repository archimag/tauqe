//! Settlement of an edit transaction. The caller owns the obligation to settle it.

use crate::context::ContextManager;
use crate::workflow::policy::ApplyPolicy;
use anyhow::{bail, Context, Result};
use ring::digest::{digest, Digest, SHA256};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Preserves originals for files the apply policy cannot restore.
pub(super) struct DiskBackup {
    root: PathBuf,
    files: BTreeMap<String, Original>,
}

enum Original {
    File(Vec<u8>),
    Absent,
    Unavailable(String),
}

/// Pre-command evidence. A Git status code does not establish the original
/// bytes (checkout filters may change them), or even prove a path was absent.
pub(super) struct ArtifactBaseline {
    revision: Option<String>,
    entries: BTreeMap<PathBuf, BaselineEntry>,
}

enum BaselineEntry {
    Directory,
    File(Digest),
    Unavailable,
}

impl ArtifactBaseline {
    pub(super) fn capture(root: &Path) -> Self {
        let revision = crate::git::run_git(Some(root), &["rev-parse", "--verify", "HEAD"])
            .ok()
            .map(|revision| revision.trim().to_owned());
        let tracked: std::collections::HashSet<PathBuf> =
            crate::git::list_repository_files(Some(root))
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect();
        let mut entries = BTreeMap::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(path) = pending.pop() {
            // A failed or partial directory listing cannot prove absence.
            let children = fs::read_dir(root.join(&path))
                .and_then(|children| children.collect::<std::io::Result<Vec<_>>>());
            let Ok(children) = children else {
                entries.insert(path, BaselineEntry::Unavailable);
                continue;
            };
            entries.insert(path.clone(), BaselineEntry::Directory);
            for child in children {
                let relative = path.join(child.file_name());
                if tauqe_tools::paths::is_excluded_path(&relative.to_string_lossy()) {
                    entries.insert(relative, BaselineEntry::Unavailable);
                    continue;
                }
                let entry = match child.file_type() {
                    Ok(kind) if kind.is_dir() => {
                        pending.push(relative);
                        continue;
                    }
                    Ok(kind) if kind.is_file() && tracked.contains(&relative) => {
                        match fs::read(child.path()) {
                            Ok(bytes) => BaselineEntry::File(digest(&SHA256, &bytes)),
                            Err(_) => BaselineEntry::Unavailable,
                        }
                    }
                    _ => BaselineEntry::Unavailable,
                };
                entries.insert(relative, entry);
            }
        }
        Self { revision, entries }
    }

    fn original(&self, root: &Path, path: &str) -> Result<Original> {
        match self.entries.get(Path::new(path)) {
            Some(BaselineEntry::File(expected)) => {
                let revision = self
                    .revision
                    .as_deref()
                    .context("no baseline Git revision")?;
                let bytes = crate::git::cmd::read_blob(root, revision, path)?;
                if expected.as_ref() != digest(&SHA256, &bytes).as_ref() {
                    bail!("Git blob does not match the pre-command file bytes");
                }
                Ok(Original::File(bytes))
            }
            Some(_) => bail!("no exact copy of the pre-command path"),
            None => {
                for parent in Path::new(path).ancestors().skip(1) {
                    match self.entries.get(parent) {
                        Some(BaselineEntry::Directory) => return Ok(Original::Absent),
                        Some(_) => break,
                        None => {}
                    }
                }
                bail!("pre-command path absence could not be confirmed")
            }
        }
    }
}

/// Recheck write admission and refuse to restore through a symlink, even one
/// pointing inside the repository: it may now alias a different file.
fn backup_path(root: &Path, path: &str) -> Result<PathBuf> {
    crate::context::verify_path_within_repo_for_write(root, path)?;
    let full = root.join(path);
    for component in full.ancestors().take_while(|entry| *entry != root) {
        match component.symlink_metadata() {
            Ok(metadata) if metadata.is_symlink() => bail!("path traverses a symbolic link"),
            Ok(metadata) if component == full && !metadata.is_file() => {
                bail!("path is not a regular file");
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(full)
}

impl DiskBackup {
    pub(super) fn record_paths(&mut self, paths: &[String]) -> Result<()> {
        let mut captured = BTreeMap::new();
        for path in paths {
            match self.files.get(path) {
                Some(Original::Unavailable(reason)) => {
                    bail!("Cannot back up `{path}`: {reason}");
                }
                Some(_) => continue,
                None => {}
            }
            let original = backup_path(&self.root, path)
                .and_then(|full| match fs::read(full) {
                    Ok(bytes) => Ok(Original::File(bytes)),
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(Original::Absent),
                    Err(error) => Err(error.into()),
                })
                .with_context(|| format!("Cannot back up `{path}`"))?;
            captured.insert(path.clone(), original);
        }
        self.files.extend(captured);
        Ok(())
    }

    pub(super) fn record_artifacts(&mut self, baseline: &ArtifactBaseline, paths: &[String]) {
        for path in paths {
            self.files.entry(path.clone()).or_insert_with(|| {
                baseline
                    .original(&self.root, path)
                    .unwrap_or_else(|error| Original::Unavailable(format!("{error:#}")))
            });
        }
    }

    pub(super) fn known_files(&self) -> std::collections::HashMap<String, Option<Vec<u8>>> {
        self.files
            .iter()
            .filter_map(|(path, original)| match original {
                Original::File(bytes) => Some((path.clone(), Some(bytes.clone()))),
                Original::Absent => Some((path.clone(), None)),
                Original::Unavailable(_) => None,
            })
            .collect()
    }

    fn rollback(&self, needs_restore: impl Fn(&str) -> bool) -> Result<()> {
        let failures: Vec<_> = self
            .files
            .iter()
            .filter(|(path, _)| needs_restore(path))
            .filter_map(|(path, original)| {
                self.restore(path, original)
                    .err()
                    .map(|error| format!("`{path}`: {error:#}"))
            })
            .collect();
        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }
        Ok(())
    }

    fn restore(&self, path: &str, original: &Original) -> Result<()> {
        let full = backup_path(&self.root, path)?;
        match original {
            Original::File(bytes) => {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(full, bytes)?;
            }
            Original::Absent => match fs::remove_file(full) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
            Original::Unavailable(reason) => bail!("original unavailable: {reason}"),
        }
        Ok(())
    }
}

pub struct TurnTransaction {
    pub(super) backup: DiskBackup,
    settled: bool,
}

/// Finalization failure is explicit: verified bytes must not imply a commit.
pub struct CommitOutcome {
    pub commit_hash: Option<String>,
    pub commit_error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RollbackOutcome {
    NothingToDo,
    FinalizationAttempted,
    RestoredByPolicy,
    /// The selected files were restored. A policy error still leaves Git
    /// recovery unconfirmed, even when every backed-up byte was restored.
    RestoredFromBackup {
        policy_error: Option<String>,
    },
    Incomplete {
        policy_error: Option<String>,
        backup_error: String,
    },
}

impl RollbackOutcome {
    /// A secondary diagnostic, not a replacement for the reason work stopped.
    pub fn diagnostic(&self) -> Option<String> {
        match self {
            Self::RestoredFromBackup { policy_error: Some(error) } => Some(format!(
                "Policy rollback failed: {error}\nFile bytes were restored from backup, but restoration of Git refs, index and transaction marker was not confirmed."
            )),
            Self::Incomplete { policy_error, backup_error } => {
                let backup = format!("Backup restoration was incomplete: {backup_error}");
                Some(match policy_error {
                    Some(error) => format!("Policy rollback failed: {error}\n{backup}"),
                    None => backup,
                })
            }
            _ => None,
        }
    }
}

impl TurnTransaction {
    pub fn new(root: &Path) -> Self {
        Self {
            backup: DiskBackup {
                root: root.to_path_buf(),
                files: BTreeMap::new(),
            },
            settled: false,
        }
    }

    pub async fn finalize(
        &mut self,
        policy: &mut dyn ApplyPolicy,
        paths: &[String],
        summary: &str,
    ) -> CommitOutcome {
        let result = policy.apply_final(&self.backup.root, paths, summary).await;
        // Finalization consumes the policy's checkpoint even on failure. Do not
        // claim a rollback after that point; preserve the explicit commit error.
        self.settled = true;
        match result {
            Ok(commit_hash) => CommitOutcome {
                commit_hash,
                commit_error: None,
            },
            Err(error) => {
                let message = format!(
                    "Changes were applied but the Git transaction could not be finalized: {error}"
                );
                tracing::error!(%error, "{message}");
                CommitOutcome {
                    commit_hash: None,
                    commit_error: Some(message),
                }
            }
        }
    }

    pub async fn rollback(
        self,
        policy: &mut dyn ApplyPolicy,
        context: &mut ContextManager,
    ) -> RollbackOutcome {
        if self.settled {
            return RollbackOutcome::FinalizationAttempted;
        }
        let (restored, policy_error) = match policy.rollback(&self.backup.root).await {
            Ok(restored) => (restored, None),
            Err(error) => (false, Some(format!("{error:#}"))),
        };
        let outcome = crate::blocking::run_blocking(|| {
            let outcome = if restored {
                self.restore_outside_checkpoint()
            } else {
                match self.backup.rollback(|_| true) {
                    Err(error) => RollbackOutcome::Incomplete {
                        policy_error,
                        backup_error: format!("{error:#}"),
                    },
                    Ok(()) if self.backup.files.is_empty() && policy_error.is_none() => {
                        RollbackOutcome::NothingToDo
                    }
                    Ok(()) if self.backup.files.is_empty() => RollbackOutcome::Incomplete {
                        policy_error,
                        backup_error: "No file backups were available".into(),
                    },
                    Ok(()) => RollbackOutcome::RestoredFromBackup { policy_error },
                }
            };
            // Retain research; only paths removed by restoration leave context.
            context.prune_missing_files();
            outcome
        });
        if let Some(diagnostic) = outcome.diagnostic() {
            tracing::error!(%diagnostic, "turn recovery could not be confirmed");
        }
        outcome
    }

    fn restore_outside_checkpoint(&self) -> RollbackOutcome {
        // Checkpoint rollback leaves ignored paths alone. Restore the known
        // sandbox paths even if the general coverage probe fails.
        let paths: Vec<_> = self.backup.files.keys().cloned().collect();
        if paths.is_empty() {
            return RollbackOutcome::RestoredByPolicy;
        }
        let ignored = tauqe_tools::git::state::gitignored_paths(&self.backup.root, &paths);
        let needs_restore = |path: &str| {
            crate::mockups::is_mockups_path(path)
                || ignored
                    .as_ref()
                    .is_ok_and(|paths| paths.iter().any(|p| p == path))
        };
        let replayed = paths.iter().any(|path| needs_restore(path));
        let replay = self.backup.rollback(needs_restore);
        let mut errors = Vec::new();
        if let Err(error) = ignored {
            errors.push(format!("Could not determine Git coverage: {error:#}"));
        }
        if let Err(error) = replay {
            errors.push(format!("{error:#}"));
        }
        if !errors.is_empty() {
            RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: errors.join("\n"),
            }
        } else if replayed {
            RollbackOutcome::RestoredFromBackup { policy_error: None }
        } else {
            RollbackOutcome::RestoredByPolicy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_touch_preserves_bytes_and_absence_and_reports_all_restore_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut transaction = TurnTransaction::new(root);
        let backup = &mut transaction.backup;
        let paths = ["binary", "new", "conflict-a", "conflict-b"].map(str::to_owned);
        fs::write(root.join("binary"), [0, 255, 128]).unwrap();
        backup.record_paths(&paths).unwrap();
        fs::write(root.join("binary"), b"edited").unwrap();
        fs::write(root.join("new"), b"created").unwrap();
        fs::create_dir(root.join("conflict-a")).unwrap();
        fs::create_dir(root.join("conflict-b")).unwrap();
        backup.record_paths(&paths).unwrap();

        let error = backup.rollback(|_| true).unwrap_err().to_string();
        assert!(
            error.contains("`conflict-a`") && error.contains("`conflict-b`"),
            "{error}"
        );
        assert!(root.join("conflict-a").is_dir());
        assert!(root.join("conflict-b").is_dir());
        assert_eq!(fs::read(root.join("binary")).unwrap(), [0, 255, 128]);
        assert!(!root.join("new").exists());
    }

    #[test]
    fn artifact_backup_uses_only_git_bytes_proven_to_match_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("binary"), [0, 255, 128]).unwrap();
        fs::write(root.join("dirty"), b"committed").unwrap();
        crate::git::init_repository(Some(root), true).unwrap();
        fs::write(root.join("dirty"), b"uncommitted").unwrap();
        let baseline = ArtifactBaseline::capture(root);
        for path in ["binary", "dirty", "new"] {
            fs::write(root.join(path), b"artifact").unwrap();
        }
        let mut transaction = TurnTransaction::new(root);
        transaction
            .backup
            .record_artifacts(&baseline, &["binary", "dirty", "new"].map(str::to_owned));

        let error = transaction
            .backup
            .rollback(|_| true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`dirty`: original unavailable"), "{error}");
        assert_eq!(fs::read(root.join("binary")).unwrap(), [0, 255, 128]);
        assert_eq!(fs::read(root.join("dirty")).unwrap(), b"artifact");
        assert!(!root.join("new").exists());
    }

    #[tokio::test]
    async fn checkpoint_rollback_also_restores_existing_sandbox_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        let path = ".tauqe/mockups/draft.txt";
        fs::create_dir_all(root.join(".tauqe/mockups")).unwrap();
        fs::write(root.join(path), b"developer draft").unwrap();
        let mut context = ContextManager::new(root.to_path_buf());
        let mut transaction = TurnTransaction::new(root);
        transaction.backup.record_paths(&[path.into()]).unwrap();
        let mut policy = crate::workflow::GitTransactionPolicy::new();
        policy
            .apply_temporary_batches(root, &Default::default(), &mut context, &[])
            .await
            .unwrap();
        fs::write(root.join(path), b"temporary draft").unwrap();

        assert_eq!(
            transaction.rollback(&mut policy, &mut context).await,
            RollbackOutcome::RestoredFromBackup { policy_error: None }
        );
        assert_eq!(fs::read(root.join(path)).unwrap(), b"developer draft");
    }
}
