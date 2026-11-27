//! Settlement of an edit transaction. The caller owns the obligation to settle it.

use crate::config::project::ProjectConfig;
use crate::context::ContextManager;
use crate::edits::{EditOperation, EditOutcome, VerificationVerdict};
use crate::git::state::{changed_worktree_paths, snapshot_worktree, WorktreeSnapshot};
use crate::stop::HardCancellation;
use crate::workflow::policy::{ApplyPolicy, FinalizationStatus, PolicyRollback};
use anyhow::{bail, Context, Result};
use ring::digest::{digest, Digest, SHA256};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Preserves originals for files the apply policy cannot restore.
struct DiskBackup {
    shared: Arc<SharedTransaction>,
}

#[derive(Default)]
struct TransactionState {
    files: BTreeMap<String, Original>,
    changed_files: Vec<String>,
    lifecycle: Lifecycle,
}

struct SharedTransaction {
    root: PathBuf,
    state: Mutex<TransactionState>,
    operations: crate::blocking::StartedOperations,
    commands: crate::blocking::StartedOperations,
    pending_command: tokio::sync::Mutex<Option<CommandEvidence>>,
}

struct CommandEvidence {
    baseline: Arc<ArtifactBaseline>,
    worktree: WorktreeSnapshot,
}

#[derive(Default)]
enum Lifecycle {
    #[default]
    Open,
    Finalizing,
    RollingBack,
    Finalized(CommitOutcome),
    RolledBack(RollbackOutcome),
}

#[derive(Clone)]
enum Original {
    File(Vec<u8>),
    Absent,
    Unavailable(String),
}

/// Pre-command evidence. A Git status code does not establish the original
/// bytes (checkout filters may change them), or even prove a path was absent.
struct ArtifactBaseline {
    revision: Option<String>,
    entries: BTreeMap<PathBuf, BaselineEntry>,
}

enum BaselineEntry {
    Directory,
    File(Digest),
    Unavailable,
}

impl ArtifactBaseline {
    pub fn capture(root: &Path) -> Self {
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

    fn changed_dirty_files(&self, root: &Path, before: &WorktreeSnapshot) -> Vec<String> {
        before
            .keys()
            .filter(|path| {
                let Some(BaselineEntry::File(expected)) = self.entries.get(Path::new(path)) else {
                    return false;
                };
                fs::read(root.join(path))
                    .is_ok_and(|bytes| expected.as_ref() != digest(&SHA256, &bytes).as_ref())
            })
            .cloned()
            .collect()
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
    fn state(&self) -> std::sync::MutexGuard<'_, TransactionState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(super) async fn record_paths(&self, paths: &[String]) -> Result<()> {
        self.shared.operations.wait().await;
        let missing = {
            let state = self.state();
            paths
                .iter()
                .filter(|path| !state.files.contains_key(*path))
                .cloned()
                .collect::<Vec<_>>()
        };
        let shared = Arc::clone(&self.shared);
        let root = shared.root.clone();
        let handle = self
            .shared
            .operations
            .start(
                move || {
                    let mut captured = BTreeMap::new();
                    for path in &missing {
                        let original = backup_path(&root, path)
                            .and_then(|full| match fs::read(full) {
                                Ok(bytes) => Ok(Original::File(bytes)),
                                Err(error) if error.kind() == ErrorKind::NotFound => {
                                    Ok(Original::Absent)
                                }
                                Err(error) => Err(error.into()),
                            })
                            .with_context(|| format!("Cannot back up `{path}`"))?;
                        captured.insert(path.clone(), original);
                    }
                    let mut data = shared
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    data.files.extend(captured);
                    Ok::<_, anyhow::Error>(())
                },
                None,
            )
            .map_err(anyhow::Error::msg)?;
        handle
            .take_result()
            .await
            .context("backup capture did not return a result")??
    }

    pub(super) fn known_files(&self) -> std::collections::HashMap<String, Option<Vec<u8>>> {
        self.state()
            .files
            .iter()
            .filter_map(|(path, original)| match original {
                Original::File(bytes) => Some((path.clone(), Some(bytes.clone()))),
                Original::Absent => Some((path.clone(), None)),
                Original::Unavailable(_) => None,
            })
            .collect()
    }

    fn rollback(&self, needs_restore: impl Fn(&str) -> bool) -> Result<()> {
        let files = self.state().files.clone();
        let failures: Vec<_> = files
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
        let full = backup_path(&self.shared.root, path)?;
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

struct TurnTransaction {
    pub(super) backup: DiskBackup,
}

/// Internal owner of a turn's policy, first-touch bytes and cumulative paths.
pub(crate) struct PolicyTransaction {
    policy: Box<dyn ApplyPolicy>,
    shared: Arc<SharedTransaction>,
}

/// Terminal file facts. Failed finalization remains an explicit EditOutcome.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PolicySettlement {
    NoChanges,
    Finalized(EditOutcome),
    RolledBack(RollbackOutcome),
}

impl PolicyTransaction {
    pub(crate) fn new(root: &Path, policy: Box<dyn ApplyPolicy>) -> Self {
        Self {
            policy,
            shared: TurnTransaction::new(root).backup.shared,
        }
    }

    fn ensure_open(&self) -> Result<()> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        match &state.lifecycle {
            Lifecycle::Open => Ok(()),
            Lifecycle::Finalizing | Lifecycle::RollingBack => {
                bail!("file transaction settlement is already in progress")
            }
            Lifecycle::Finalized(commit) => bail!(
                "file transaction has already been finalized (commit: {:?}, error: {:?})",
                commit.commit_hash,
                commit.commit_error
            ),
            Lifecycle::RolledBack(outcome) => {
                bail!("file transaction has already been rolled back: {outcome:?}")
            }
        }
    }

    fn backup(&self) -> DiskBackup {
        DiskBackup {
            shared: Arc::clone(&self.shared),
        }
    }

    /// An owned snapshot of paths published by completed operations.
    pub fn changed_files(&self) -> Vec<String> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .changed_files
            .clone()
    }

    /// Copies original bytes from completed operations without exposing the backup.
    pub fn original_bytes(&self) -> std::collections::HashMap<String, Option<Vec<u8>>> {
        self.backup().known_files()
    }

    /// Register pre-command evidence before handing out the retained process owner.
    /// Artifact capture remains the transaction's responsibility if the waiter is lost.
    pub async fn begin_command(&self) -> Result<crate::blocking::StartedOperations> {
        self.ensure_open()?;
        let mut pending = self.shared.pending_command.lock().await;
        if pending.is_some() {
            bail!("a project command is already in progress");
        }
        self.shared.commands.wait().await;
        let root = self.shared.root.clone();
        let evidence = crate::blocking::run_blocking_async(move || CommandEvidence {
            baseline: Arc::new(ArtifactBaseline::capture(&root)),
            worktree: snapshot_worktree(&root),
        })
        .await?;
        *pending = Some(evidence);
        Ok(self.shared.commands.clone())
    }

    /// Account for a command only after its process group has stopped.
    pub async fn complete_command(&self) -> Result<Vec<String>> {
        let mut pending = self.shared.pending_command.lock().await;
        let evidence = pending
            .as_ref()
            .context("no project command is in progress")?;
        let summary = self.shared.commands.wait().await;
        if summary.unconfirmed != 0 {
            bail!("project command shutdown could not be confirmed");
        }
        let root = self.shared.root.clone();
        let before = evidence.worktree.clone();
        let baseline = Arc::clone(&evidence.baseline);
        let paths = crate::blocking::run_blocking_async(move || {
            let mut paths = changed_worktree_paths(&before, &snapshot_worktree(&root));
            paths.extend(baseline.changed_dirty_files(&root, &before));
            paths.sort();
            paths.dedup();
            paths
        })
        .await?;
        self.capture_artifacts_inner(Arc::clone(&evidence.baseline), &paths)
            .await?;
        pending.take();
        Ok(paths)
    }

    /// `touched` includes deletions and both ends of moves.
    pub async fn apply(
        &mut self,
        project: &ProjectConfig,
        context: &mut ContextManager,
        touched: &[String],
        batches: &[&[EditOperation]],
        cancel: Option<&HardCancellation>,
    ) -> Result<Vec<String>> {
        self.ensure_open()?;
        let root = self.shared.root.clone();
        self.backup().record_paths(touched).await?;
        // Publication can complete after its waiter disappears. Retain paths
        // before entering it, including writes that subsequently report failure.
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            for path in touched {
                if !state.changed_files.contains(path) {
                    state.changed_files.push(path.clone());
                }
            }
        }
        let changed = self
            .policy
            .apply_temporary_batches_owned(&root, project, context, batches, cancel)
            .await?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        for path in &changed {
            if !state.changed_files.contains(path) {
                state.changed_files.push(path.clone());
            }
        }
        Ok(changed)
    }

    /// Capture artifacts only where the baseline proves original bytes or absence.
    async fn capture_artifacts_inner(
        &self,
        baseline: Arc<ArtifactBaseline>,
        paths: &[String],
    ) -> Result<()> {
        let root = self.shared.root.clone();
        self.shared.operations.wait().await;
        // Retain path ownership before starting the capture, even if its waiter vanishes.
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            for path in paths {
                if !state.changed_files.contains(path) {
                    state.changed_files.push(path.clone());
                }
            }
        }
        let missing: Vec<_> = {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            paths
                .iter()
                .filter(|path| !state.files.contains_key(*path))
                .cloned()
                .collect()
        };
        let shared = Arc::clone(&self.shared);
        let handle = self
            .shared
            .operations
            .start(
                move || {
                    let originals = missing
                        .into_iter()
                        .map(|path| {
                            let original =
                                baseline.original(&root, &path).unwrap_or_else(|error| {
                                    Original::Unavailable(format!("{error:#}"))
                                });
                            (path, original)
                        })
                        .collect::<BTreeMap<_, _>>();
                    shared
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .files
                        .extend(originals);
                },
                None,
            )
            .map_err(anyhow::Error::msg)?;
        handle
            .take_result()
            .await
            .context("artifact capture did not return a result")??;
        Ok(())
    }

    /// A lost waiter can rejoin finalization without starting a second commit.
    pub async fn finalize_resumable(
        &mut self,
        summary: &str,
        verification: VerificationVerdict,
        cancel: Option<&HardCancellation>,
    ) -> Result<Option<(PolicySettlement, bool)>> {
        self.settle_finalization(summary, verification, cancel, true)
            .await
    }

    /// Rejoin only a finalization already admitted by the underlying policy.
    pub async fn finalization_result(
        &mut self,
        summary: &str,
        verification: VerificationVerdict,
        cancel: Option<&HardCancellation>,
    ) -> Result<Option<(PolicySettlement, bool)>> {
        self.settle_finalization(summary, verification, cancel, false)
            .await
    }

    async fn settle_finalization(
        &mut self,
        summary: &str,
        verification: VerificationVerdict,
        cancel: Option<&HardCancellation>,
        admit: bool,
    ) -> Result<Option<(PolicySettlement, bool)>> {
        if !admit {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(state.lifecycle, Lifecycle::Open) {
                return Ok(None);
            }
        }
        if self.shared.pending_command.lock().await.is_some() {
            bail!("project command consequences have not been recorded");
        }
        let commands = self.shared.commands.wait().await;
        if commands.unconfirmed != 0 {
            bail!("project command shutdown could not be confirmed");
        }
        {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            match state.lifecycle {
                Lifecycle::Open | Lifecycle::Finalizing => {}
                _ => bail!("file transaction has already been settled"),
            }
        }
        self.shared.operations.wait().await;
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle = Lifecycle::Finalizing;
        let changed_files = self.changed_files();
        let status = if admit {
            self.policy
                .finalize_resumable(&self.shared.root, &changed_files, summary, cancel)
                .await?
        } else {
            self.policy
                .finalization_result(&self.shared.root, &changed_files, summary, cancel)
                .await?
        };
        let FinalizationStatus::Completed { result, cancelled } = status else {
            self.shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lifecycle = Lifecycle::Open;
            return Ok(None);
        };
        let commit = match result {
            Ok(commit_hash) => CommitOutcome {
                commit_hash,
                commit_error: None,
            },
            Err(error) => CommitOutcome {
                commit_hash: None,
                commit_error: Some(format!(
                    "Changes were applied but the Git transaction could not be finalized: {error}"
                )),
            },
        };
        let no_changes = changed_files.is_empty()
            && commit.commit_error.is_none()
            && commit.commit_hash.is_none();
        let applied = commit.commit_error.is_none();
        let error = commit.commit_error.clone();
        let hash = commit.commit_hash.clone();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle = Lifecycle::Finalized(commit);
        if no_changes {
            return Ok(Some((PolicySettlement::NoChanges, cancelled)));
        }
        Ok(Some((
            PolicySettlement::Finalized(EditOutcome {
                applied,
                error,
                changed_files,
                commit_hash: hash,
                verification: Some(verification),
            }),
            cancelled,
        )))
    }

    pub async fn rollback(&mut self, context: &mut ContextManager) -> Result<PolicySettlement> {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            match state.lifecycle {
                Lifecycle::Open | Lifecycle::RollingBack => {
                    state.lifecycle = Lifecycle::RollingBack
                }
                _ => bail!("file transaction has already been settled"),
            }
        }
        self.shared.commands.close();
        if let Err(error) = self.complete_pending_command().await {
            let outcome = RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: format!(
                    "Project command completion or artifact capture failed: {error:#}"
                ),
            };
            self.shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lifecycle = Lifecycle::RolledBack(outcome.clone());
            return Ok(PolicySettlement::RolledBack(outcome));
        }
        if self.shared.commands.wait().await.unconfirmed != 0 {
            let outcome = RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: "Project command shutdown could not be confirmed".into(),
            };
            self.shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lifecycle = Lifecycle::RolledBack(outcome.clone());
            return Ok(PolicySettlement::RolledBack(outcome));
        }
        let outcome = TurnTransaction {
            backup: self.backup(),
        }
        .rollback(self.policy.as_mut(), context)
        .await;
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lifecycle = Lifecycle::RolledBack(outcome.clone());
        Ok(PolicySettlement::RolledBack(outcome))
    }

    async fn complete_pending_command(&mut self) -> Result<()> {
        if self.shared.pending_command.lock().await.is_some() {
            self.complete_command().await?;
        }
        Ok(())
    }
}

/// Finalization failure is explicit: verified bytes must not imply a commit.
struct CommitOutcome {
    pub commit_hash: Option<String>,
    pub commit_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn new(root: &Path) -> Self {
        Self {
            backup: DiskBackup {
                shared: Arc::new(SharedTransaction {
                    root: root.to_path_buf(),
                    state: Mutex::new(TransactionState::default()),
                    operations: crate::blocking::StartedOperations::default(),
                    commands: crate::blocking::StartedOperations::default(),
                    pending_command: tokio::sync::Mutex::new(None),
                }),
            },
        }
    }

    async fn rollback(
        self,
        policy: &mut dyn ApplyPolicy,
        context: &mut ContextManager,
    ) -> RollbackOutcome {
        if matches!(
            self.backup.state().lifecycle,
            Lifecycle::Finalizing | Lifecycle::Finalized(_)
        ) {
            return RollbackOutcome::FinalizationAttempted;
        }
        self.backup.shared.operations.close();
        let operations = self.backup.shared.operations.wait().await;
        if operations.unconfirmed != 0 {
            return RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: format!(
                    "{} technical operation(s) could not be confirmed",
                    operations.unconfirmed
                ),
            };
        }
        if let Err(error) = policy.stop_and_wait().await {
            return RollbackOutcome::Incomplete {
                policy_error: Some(format!("{error:#}")),
                backup_error:
                    "Active publication could not be confirmed; restoration was not attempted"
                        .into(),
            };
        }
        let (restored, policy_error) = match policy.rollback(&self.backup.shared.root).await {
            Ok(PolicyRollback::RestoredCheckpoint) => (true, None),
            Ok(PolicyRollback::NoCheckpoint) => (false, None),
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
                    Ok(()) if self.backup.state().files.is_empty() && policy_error.is_none() => {
                        RollbackOutcome::NothingToDo
                    }
                    Ok(()) if self.backup.state().files.is_empty() => RollbackOutcome::Incomplete {
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
        if operations.unconfirmed != 0 {
            return RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: format!(
                    "{} technical operation(s) could not be confirmed",
                    operations.unconfirmed
                ),
            };
        }
        if let Some(diagnostic) = outcome.diagnostic() {
            tracing::error!(%diagnostic, "turn recovery could not be confirmed");
        }
        outcome
    }

    fn restore_outside_checkpoint(&self) -> RollbackOutcome {
        // Checkpoint rollback leaves ignored paths alone. Restore the known
        // sandbox paths even if the general coverage probe fails.
        let paths: Vec<_> = self.backup.state().files.keys().cloned().collect();
        if paths.is_empty() {
            return RollbackOutcome::RestoredByPolicy;
        }
        let ignored = tauqe_tools::git::state::gitignored_paths(&self.backup.shared.root, &paths);
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

    #[tokio::test]
    async fn first_touch_preserves_bytes_and_absence_and_reports_all_restore_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let transaction = TurnTransaction::new(root);
        let backup = &transaction.backup;
        let paths = ["binary", "new", "conflict-a", "conflict-b"].map(str::to_owned);
        fs::write(root.join("binary"), [0, 255, 128]).unwrap();
        backup.record_paths(&paths).await.unwrap();
        fs::write(root.join("binary"), b"edited").unwrap();
        fs::write(root.join("new"), b"created").unwrap();
        fs::create_dir(root.join("conflict-a")).unwrap();
        fs::create_dir(root.join("conflict-b")).unwrap();
        backup.record_paths(&paths).await.unwrap();

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

    #[tokio::test]
    async fn artifact_backup_uses_only_git_bytes_proven_to_match_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("binary"), [0, 255, 128]).unwrap();
        fs::write(root.join("dirty"), b"committed").unwrap();
        crate::git::init_repository(Some(root), true).unwrap();
        fs::write(root.join("dirty"), b"uncommitted").unwrap();
        let policy = PolicyTransaction::new(
            root,
            Box::new(crate::workflow::policy::DirectApplyPolicy::new()),
        );
        let baseline = Arc::new(ArtifactBaseline::capture(root));
        for path in ["binary", "dirty", "new"] {
            fs::write(root.join(path), b"artifact").unwrap();
        }
        policy
            .capture_artifacts_inner(baseline, &["binary", "dirty", "new"].map(str::to_owned))
            .await
            .unwrap();

        let error = policy.backup().rollback(|_| true).unwrap_err().to_string();
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
        let transaction = TurnTransaction::new(root);
        transaction
            .backup
            .record_paths(&[path.into()])
            .await
            .unwrap();
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
