//! The virtual-staging state machine.
//!
//! A change set is applied to memory before any byte reaches disk: consecutive
//! operations addressing one path form an atomic member, a refused member
//! restores exactly the paths it touched, and the remaining members continue
//! independently.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use tauqe_protocol::{EditOperation, GlobPattern};

use super::admission::{admit_write, ignored_target_paths};
use super::error::EditError;
use super::fuzzy::apply_replace;
use super::protection::{is_protected, protected_patterns};
use super::staging::{self, apply_anchor_edits};
use crate::context::ContextManager;

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
/// path are applied in order, and a member that fails is rolled back to the state
/// it found, so a member that applied earlier keeps its own effect. Other members
/// succeed independently. Pass the returned staged state into the next attempt to
/// retain earlier successes.
pub fn stage_and_validate_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    // Staging reads the original file contents, which is blocking file I/O.
    // Run it through the blocking helper so a large file never stalls an async
    // worker thread.
    crate::blocking::run_blocking(|| {
        stage_and_validate_edits_inner(repo_root, context_manager, edits, initial_staged)
    })
}

/// Applies a change set to the in-memory state one atomic member at a time.
///
/// Every file's operations reach [`stage_file_edits`] together, so a replacement
/// that misses its needle rolls the whole file back instead of leaving a
/// half-applied file behind. A rename is the one member that changes two paths
/// at once, and it resolves its source from the state this attempt has already
/// built, which is what carries the content of a chained rename (`a -> b -> c`)
/// forward. A rejected member restores every path it addressed to the snapshot
/// taken immediately before that member applied, so an earlier member keeps its
/// effect even when a later member fails on a path the two of them share, and
/// the remaining members continue independently.
pub(super) fn stage_and_validate_edits_inner(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    let mut state = initial_staged.cloned().unwrap_or_default();
    let protected = protected_patterns(repo_root);
    let ignored = ignored_target_paths(repo_root, context_manager, edits);
    let (units, mut failed_files) = build_staging_units(context_manager, edits);
    let mut failed_paths: HashSet<String> = HashSet::new();
    // The members that applied cleanly, in change-set order. The reported change
    // set is derived from them, so an earlier member that shares a path with a
    // later refused member keeps its place in it.
    let mut applied_units: Vec<usize> = Vec::new();

    for (index, unit) in units.iter().enumerate() {
        if unit.is_rejected(&failed_paths) {
            continue;
        }
        // Every member snapshots its own paths immediately before it applies, so a
        // refusal restores the state that member found. A replacement addressed to
        // a rename destination is therefore rolled back to the renamed content,
        // instead of the opening state of the attempt, in which the destination did
        // not exist yet and the moved content would be lost.
        let snapshots: Vec<(String, PathSnapshot)> = unit
            .paths()
            .into_iter()
            .map(|path| (path.to_string(), PathSnapshot::capture(&state, path)))
            .collect();
        let applied = apply_staging_unit(
            repo_root,
            context_manager,
            unit,
            &mut state,
            &protected,
            &ignored,
        );
        match applied {
            Ok(()) => applied_units.push(index),
            Err((path, error)) => {
                for (rejected, snapshot) in &snapshots {
                    snapshot.restore(&mut state, rejected);
                    failed_paths.insert(rejected.clone());
                }
                failed_files.push(FailedFileEdit {
                    path,
                    error: *error,
                });
            }
        }
    }

    let mut successful_paths: Vec<String> = Vec::new();
    let mut successful_edits: Vec<EditOperation> = Vec::new();
    for index in applied_units {
        let Some(unit) = units.get(index) else {
            continue;
        };
        for path in unit.paths() {
            if !successful_paths.iter().any(|known| known.as_str() == path) {
                successful_paths.push(path.to_string());
            }
        }
        successful_edits.extend(unit.operations());
    }
    // A path this attempt deleted in memory never reached disk, so the commit
    // has no file to remove for it and is not asked to act on it.
    successful_paths
        .retain(|path| !state.deleted_paths.contains(path) || repo_root.join(path).exists());

    StageEditsResult {
        staged_state: state,
        successful_paths,
        successful_edits,
        failed_files,
    }
}

/// The state one path held before a staging member touched it.
///
/// A rejected member is restored to this snapshot, so an operation that succeeded
/// in an earlier member keeps its effect: a replacement that follows a rename is
/// rolled back to the renamed content, not to the opening state of the attempt.
struct PathSnapshot {
    content: Option<String>,
    deleted: bool,
}

impl PathSnapshot {
    /// Captures the state `path` currently holds.
    fn capture(staged: &StagedEditsState, path: &str) -> Self {
        Self {
            content: staged.staged_files.get(path).cloned(),
            deleted: staged.deleted_paths.contains(path),
        }
    }

    /// Puts `path` back into the captured state.
    fn restore(&self, staged: &mut StagedEditsState, path: &str) {
        match &self.content {
            Some(content) => {
                staged
                    .staged_files
                    .insert(path.to_string(), content.clone());
                staged.deleted_paths.remove(path);
            }
            None => {
                staged.staged_files.remove(path);
                if self.deleted {
                    staged.deleted_paths.insert(path.to_string());
                } else {
                    staged.deleted_paths.remove(path);
                }
            }
        }
    }
}

/// One atomic member of a staging attempt.
enum StagingUnit<'a> {
    /// A run of operations addressing a single file.
    File {
        path: String,
        operations: Vec<&'a EditOperation>,
    },
    /// A rename, the one operation that changes two paths at once.
    Move {
        from: String,
        to: String,
        operation: &'a EditOperation,
    },
}

impl StagingUnit<'_> {
    /// The normalized paths this member changes.
    fn paths(&self) -> Vec<&str> {
        match self {
            Self::File { path, .. } => vec![path.as_str()],
            Self::Move { from, to, .. } => vec![from.as_str(), to.as_str()],
        }
    }

    /// Whether this member addresses a path an earlier member already failed.
    fn is_rejected(&self, failed_paths: &HashSet<String>) -> bool {
        self.paths().iter().any(|path| failed_paths.contains(*path))
    }

    /// The operations this member was built from, in change-set order.
    fn operations(&self) -> Vec<EditOperation> {
        match self {
            Self::File { operations, .. } => operations.iter().copied().cloned().collect(),
            Self::Move { operation, .. } => vec![EditOperation::clone(*operation)],
        }
    }
}

/// A refused staging member: the offending path and the reason it was refused.
///
/// The reason is boxed because `EditError` alone already fills the 128-byte
/// budget `clippy::result_large_err` allows, and the pair would exceed it.
type StagingRejection = (String, Box<EditError>);

/// Builds the rejection of `path` caused by `error`.
fn rejection(path: &str, error: EditError) -> StagingRejection {
    (path.to_string(), Box::new(error))
}

/// Groups a change set into the atomic members of a staging attempt.
///
/// Consecutive operations on the same normalized path form one member, which is
/// also the unit the anchored batching of [`stage_file_edits`] resolves against.
/// A path that cannot be normalized is reported immediately instead of forming a
/// member of its own.
fn build_staging_units<'a>(
    context_manager: &ContextManager,
    edits: &'a [EditOperation],
) -> (Vec<StagingUnit<'a>>, Vec<FailedFileEdit>) {
    let mut units: Vec<StagingUnit<'a>> = Vec::new();
    let mut failed_files = Vec::new();
    // The path of the last member, while it still accepts consecutive
    // operations on the same file.
    let mut open: Option<String> = None;

    for edit in edits {
        match edit {
            EditOperation::Move { from, to } => {
                open = None;
                match (
                    normalize_edit_path(context_manager, from),
                    normalize_edit_path(context_manager, to),
                ) {
                    (Ok(from), Ok(to)) => units.push(StagingUnit::Move {
                        from,
                        to,
                        operation: edit,
                    }),
                    (Err((path, error)), _) | (_, Err((path, error))) => {
                        failed_files.push(FailedFileEdit {
                            path,
                            error: *error,
                        });
                    }
                }
            }
            _ => {
                let path = match normalize_edit_path(context_manager, operation_path(edit)) {
                    Ok(path) => path,
                    Err((path, error)) => {
                        failed_files.push(FailedFileEdit {
                            path,
                            error: *error,
                        });
                        continue;
                    }
                };
                if open.as_deref() == Some(path.as_str()) {
                    if let Some(StagingUnit::File { operations, .. }) = units.last_mut() {
                        operations.push(edit);
                        continue;
                    }
                }
                open = Some(path.clone());
                units.push(StagingUnit::File {
                    path,
                    operations: vec![edit],
                });
            }
        }
    }

    (units, failed_files)
}

/// Names the path an operation writes, regardless of its kind.
fn operation_path(operation: &EditOperation) -> &str {
    match operation {
        EditOperation::Replace { path, .. }
        | EditOperation::Create { path, .. }
        | EditOperation::Overwrite { path, .. }
        | EditOperation::Delete { path, .. }
        | EditOperation::LineAnchor { path, .. } => path,
        EditOperation::Move { from, .. } => from,
    }
}

/// Normalizes one change-set path against the repository.
///
/// The raw spelling travels back with the rejection, because a path that never
/// normalized has no normalized form to report.
fn normalize_edit_path(
    context_manager: &ContextManager,
    path: &str,
) -> Result<String, StagingRejection> {
    context_manager.normalize_path(path).map_err(|err| {
        rejection(
            path,
            EditError::InvalidPath(path.to_string(), err.to_string()),
        )
    })
}

/// Applies one staging member, leaving the state untouched when it is refused.
fn apply_staging_unit(
    repo_root: &Path,
    context_manager: &ContextManager,
    unit: &StagingUnit<'_>,
    state: &mut StagedEditsState,
    protected: &[GlobPattern],
    ignored: &HashSet<String>,
) -> Result<(), StagingRejection> {
    match unit {
        StagingUnit::File { path, operations } => {
            if is_protected(protected, path) {
                return Err(rejection(path, EditError::ProtectedPath(path.clone())));
            }
            let writes = operations.iter().copied().any(|operation| {
                matches!(
                    operation,
                    EditOperation::Create { .. } | EditOperation::Overwrite { .. }
                )
            });
            if writes {
                admit_write(path, ignored).map_err(|error| rejection(path, error))?;
            }
            let staged = stage_file_edits(repo_root, context_manager, path, operations, state)
                .map_err(|error| rejection(path, error))?;
            match staged {
                Some(content) => {
                    state.staged_files.insert(path.clone(), content);
                    state.deleted_paths.remove(path);
                }
                None => {
                    state.staged_files.remove(path);
                    state.deleted_paths.insert(path.clone());
                }
            }
            Ok(())
        }
        StagingUnit::Move { from, to, .. } => {
            if is_protected(protected, from) {
                return Err(rejection(from, EditError::ProtectedPath(from.clone())));
            }
            if is_protected(protected, to) {
                return Err(rejection(to, EditError::ProtectedPath(to.clone())));
            }
            admit_write(to, ignored).map_err(|error| rejection(to, error))?;
            stage_move(repo_root, context_manager, from, to, state)
        }
    }
}

/// Stages a rename, the one operation that changes two paths at once.
///
/// The source is resolved from the state this attempt has already built before
/// the working tree is consulted, so a chained rename carries its content
/// forward instead of looking for a file that never reached disk. The
/// destination must still be free, because a rename never overwrites.
fn stage_move(
    repo_root: &Path,
    context_manager: &ContextManager,
    from: &str,
    to: &str,
    state: &mut StagedEditsState,
) -> Result<(), StagingRejection> {
    for path in [from, to] {
        crate::context::verify_path_within_repo_for_write(repo_root, path).map_err(|err| {
            rejection(
                path,
                EditError::InvalidPath(path.to_string(), err.to_string()),
            )
        })?;
    }
    if state.staged_files.contains_key(to) || repo_root.join(to).exists() {
        return Err(rejection(to, EditError::DestinationExists(to.to_string())));
    }
    let in_context = context_manager.contains(from).map_err(|err| {
        rejection(
            from,
            EditError::InvalidPath(from.to_string(), err.to_string()),
        )
    })?;
    // A source this attempt created in memory has no file on disk, so it is
    // carried forward exactly like a replace of a freshly staged file.
    if !in_context && !state.staged_files.contains_key(from) {
        return Err(rejection(from, EditError::NotInContext(from.to_string())));
    }
    let content = match state.staged_files.get(from) {
        Some(content) => content.clone(),
        None if state.deleted_paths.contains(from) => {
            return Err(rejection(from, EditError::FileNotFound(from.to_string())));
        }
        None => {
            let source = repo_root.join(from);
            if !source.is_file() {
                return Err(rejection(from, EditError::FileNotFound(from.to_string())));
            }
            std::fs::read_to_string(&source)
                .map_err(|err| rejection(from, EditError::Io(from.to_string(), err)))?
        }
    };
    state.staged_files.insert(to.to_string(), content);
    state.deleted_paths.remove(to);
    state.staged_files.remove(from);
    state.deleted_paths.insert(from.to_string());
    Ok(())
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
    let invalid_path =
        |err: anyhow::Error| EditError::InvalidPath(path.to_string(), err.to_string());
    // A staged file is a write target, so the escape boundary must be
    // inspectable: an uncanonicalizable repository root refuses the write
    // instead of silently admitting it.
    crate::context::verify_path_within_repo_for_write(repo_root, path).map_err(invalid_path)?;
    let in_context = context_manager.contains(path).map_err(invalid_path)?;
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

    let mut index = 0;
    while index < edits.len() {
        let Some(edit) = edits.get(index) else {
            break;
        };
        if !in_context && !newly_created {
            match edit {
                EditOperation::Create { .. } if !full_path.exists() => {}
                _ => return Err(EditError::NotInContext(path.to_string())),
            }
        }

        match edit {
            EditOperation::LineAnchor {
                start,
                end,
                content: replacement,
                ..
            } => {
                // Anchored ranges address the lines the context block rendered,
                // so a run of them is resolved against one snapshot and applied
                // from the bottom of the file upwards. Replacing a lower range
                // leaves the byte offsets of every higher range exactly where
                // the snapshot put them, which is why no cumulative line shift
                // is ever computed.
                let mut batch = vec![staging::AnchorEdit {
                    start: start.as_str(),
                    end: end.as_str(),
                    content: replacement.as_str(),
                }];
                index += 1;
                while let Some(EditOperation::LineAnchor {
                    start,
                    end,
                    content: replacement,
                    ..
                }) = edits.get(index)
                {
                    batch.push(staging::AnchorEdit {
                        start: start.as_str(),
                        end: end.as_str(),
                        content: replacement.as_str(),
                    });
                    index += 1;
                }
                if !loaded {
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(path.to_string()));
                    }
                    content = Some(
                        std::fs::read_to_string(&full_path)
                            .map_err(|err| EditError::Io(path.to_string(), err))?,
                    );
                    loaded = true;
                }
                let updated = {
                    let current = content
                        .as_deref()
                        .ok_or_else(|| EditError::FileNotFound(path.to_string()))?;
                    apply_anchor_edits(current, &batch, path)?
                };
                content = Some(updated);
                continue;
            }
            EditOperation::Replace {
                old_text, new_text, ..
            } => {
                // A replacement that echoes no text addresses no line of the
                // file, so this is the staging end of the rejection the
                // structured converter reports: the failure is carried as the
                // typed rejection and the active patch strategy decides how the
                // correction is explained.
                if old_text.trim().is_empty() {
                    return Err(EditError::ReplaceWithoutRange {
                        path: path.to_string(),
                    });
                }
                if !loaded {
                    if !full_path.is_file() {
                        return Err(EditError::FileNotFound(path.to_string()));
                    }
                    content = Some(
                        std::fs::read_to_string(&full_path)
                            .map_err(|err| EditError::Io(path.to_string(), err))?,
                    );
                    loaded = true;
                }
                let current = content
                    .as_ref()
                    .ok_or_else(|| EditError::FileNotFound(path.to_string()))?;
                let updated = apply_replace(current, old_text, new_text, path)?;
                content = Some(updated);
            }
            EditOperation::Create {
                content: new_content,
                ..
            } => {
                content = Some(new_content.clone());
                loaded = true;
                newly_created = !full_path.exists();
            }
            EditOperation::Overwrite {
                content: new_content,
                ..
            } => {
                // Full replacement requires an existing (on disk or staged) file.
                if content.is_none() && (loaded || !full_path.is_file()) {
                    return Err(EditError::FileNotFound(path.to_string()));
                }
                content = Some(new_content.clone());
                loaded = true;
            }
            EditOperation::Delete { .. } => {
                content = None;
                loaded = true;
            }
            EditOperation::Move { .. } => {
                return Err(EditError::InvalidPath(
                    path.to_string(),
                    "Move operations cannot be staged as per-file edits".to_string(),
                ));
            }
        }
        index += 1;
    }
    validate_staged_project_config(path, content.as_deref())?;
    Ok(content)
}

/// Validates the staged content of the repository configuration file.
///
/// The harness must never commit a configuration it could not load on its next
/// start, so a change set that rewrites the repository configuration is checked
/// while it is still purely in memory. Both accepted file names are guarded, so
/// a repository configured through `.tauqe.toml` receives exactly the same
/// treatment as one configured through `tauqe.toml`. The validation has two
/// levels: the TOML syntax is parsed first, and the resulting declaration is
/// then validated semantically, so dropped model roles or a missing
/// verification command are rejected as well. A failure surfaces as an ordinary
/// [`EditError`], which activates the patch-retry loop instead of letting a
/// broken file reach the working tree.
fn validate_staged_project_config(path: &str, content: Option<&str>) -> Result<(), EditError> {
    if !crate::config::is_repo_config_path(path) {
        return Ok(());
    }
    let Some(content) = content else {
        return Ok(());
    };

    // Level 1 — syntactic validation of the staged bytes.
    let parsed: crate::config::AppConfig =
        toml::from_str(content).map_err(|err| EditError::InvalidConfig {
            path: path.to_string(),
            message: format!(
                "the file is not valid TOML: {}",
                crate::escape::single_line(&err.to_string())
            ),
        })?;

    // Level 2 — semantic validation of the mandatory structures the harness
    // depends on: the model roles must resolve and verification must stay
    // declared.
    parsed.validate().map_err(|err| EditError::InvalidConfig {
        path: path.to_string(),
        message: crate::escape::single_line(&err),
    })
}
