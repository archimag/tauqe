//! The virtual-staging state machine.
//!
//! A change set is applied to memory before any byte reaches disk: consecutive
//! operations addressing one path form an atomic member, a directory move forms
//! one member covering the whole subtree it expands into, a refused member
//! restores exactly the paths it touched, and the remaining members continue
//! independently.
//!
//! The machine reads only wire values: the operations of the change set, the
//! resolved [`ProjectConfig`] that declares the trust boundaries, and the
//! [`ContextState`] snapshot that says which paths are in context. It therefore
//! never observes a live context engine, and the one check it cannot make itself
//! — the in-memory validation of a repository configuration file — arrives as the
//! [`StagedConfigValidator`] hook.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use tauqe_protocol::{ContextState, EditOperation, GlobPattern, ProjectConfig};

use super::admission::{admit_move_destination, admit_write, ignored_target_paths};
use super::error::EditError;
use super::fuzzy::apply_replace;
use super::protection::{is_protected, protected_dir_move_path, protected_patterns};
use super::staging::{self, apply_anchor_edits, expand_dir_move, ExpandedMove};
use crate::paths::{normalize_repo_path, verify_path_within_repo_for_write};

/// Validates the staged text of one path while it is still purely in memory.
///
/// `path` is the normalized repository path, and `content` is the staged text or
/// `None` when the change set deletes the file. A hook that does not own the path
/// reports `Ok(())`; a rejected document comes back as the message the typed
/// [`EditError::InvalidConfig`] carries. The engine that owns the repository
/// configuration supplies the hook, so the substrate reads no configuration file
/// of its own.
pub type StagedConfigValidator = fn(&str, Option<&str>) -> Result<(), String>;

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
    project: &ProjectConfig,
    context: &ContextState,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    stage_and_validate_edits_with(repo_root, project, context, edits, initial_staged, None)
}

/// Stages `edits` and hands every staged file to `config_validator`.
///
/// The hook is the only harness-specific input of the attempt: a repository
/// configuration governs the environment the change set is verified in, and only
/// the engine that owns that configuration can judge its staged text.
pub fn stage_and_validate_edits_with(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &ContextState,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    config_validator: Option<StagedConfigValidator>,
) -> StageEditsResult {
    // Staging reads the original file contents, which is blocking file I/O.
    // Run it through the blocking helper so a large file never stalls an async
    // worker thread.
    crate::blocking::run_blocking(|| {
        stage_and_validate_edits_inner(
            repo_root,
            project,
            context,
            edits,
            initial_staged,
            config_validator,
        )
    })
}

/// Applies a change set to the in-memory state one atomic member at a time.
///
/// Every file's operations reach [`stage_file_edits`] together, so a replacement
/// that misses its needle rolls the whole file back instead of leaving a
/// half-applied file behind. A rename changes two paths at once and a directory
/// move changes the whole subtree it expands into; both resolve their sources
/// from the state this attempt has already built, which is what carries the
/// content of a chained rename (`a -> b -> c`) forward. A rejected member
/// restores every path it addressed to the snapshot taken immediately before
/// that member applied, so an earlier member keeps its effect even when a later
/// member fails on a path the two of them share, and the remaining members
/// continue independently.
pub(crate) fn stage_and_validate_edits_inner(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &ContextState,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    config_validator: Option<StagedConfigValidator>,
) -> StageEditsResult {
    let mut state = initial_staged.cloned().unwrap_or_default();
    let protected = protected_patterns(project);
    // The context answers every membership question of the attempt, so its paths
    // are collected once instead of scanning the whole item list per lookup.
    let context_paths: HashSet<&str> = context
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    let (units, mut failed_files) = build_staging_units(repo_root, edits);
    // The ignore probe is one batched `git check-ignore` call, so the whole
    // change set is resolved against Git before the first member applies. A
    // directory move contributes one candidate per file it expands into, which
    // is why the candidates come from the built members rather than from the
    // raw operations.
    let mut write_targets: Vec<String> = units
        .iter()
        .flat_map(|unit| unit.write_targets())
        .map(str::to_string)
        .collect();
    write_targets.sort();
    write_targets.dedup();
    let ignored = ignored_target_paths(repo_root, &write_targets);
    let mut failed_paths: HashSet<String> = HashSet::new();
    // The members that applied cleanly, in change-set order. The reported change
    // set is derived from them, so an earlier member that shares a path with a
    // later refused member keeps its place in it.
    let mut applied_units: Vec<usize> = Vec::new();

    // Every member's path list is resolved once for the attempt: the rejection
    // check, the snapshot capture, and the success accounting all read the same
    // list, so a directory move expanding to many files is never re-enumerated
    // per pass.
    let unit_paths: Vec<Vec<&str>> = units.iter().map(|unit| unit.paths()).collect();

    for (index, (unit, paths)) in units.iter().zip(unit_paths.iter()).enumerate() {
        if paths.iter().any(|path| failed_paths.contains(*path)) {
            continue;
        }
        // Every member snapshots its own paths immediately before it applies, so a
        // refusal restores the state that member found. A replacement addressed to
        // a rename destination is therefore rolled back to the renamed content,
        // instead of the opening state of the attempt, in which the destination did
        // not exist yet and the moved content would be lost.
        let snapshots: Vec<(String, PathSnapshot)> = paths
            .iter()
            .copied()
            .map(|path| (path.to_string(), PathSnapshot::capture(&state, path)))
            .collect();
        let applied = apply_staging_unit(
            repo_root,
            &context_paths,
            unit,
            &mut state,
            &protected,
            &ignored,
            config_validator,
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
    // Membership is answered by a set, so a path two members share is reported
    // once without scanning every path that was already reported.
    let mut reported_paths: HashSet<&str> = HashSet::new();
    let mut successful_edits: Vec<EditOperation> = Vec::new();
    for index in applied_units {
        let Some(unit) = units.get(index) else {
            continue;
        };
        let Some(paths) = unit_paths.get(index) else {
            continue;
        };
        for path in paths.iter().copied() {
            if reported_paths.insert(path) {
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
    /// A directory rename, expanded into the per-file moves of its subtree.
    ///
    /// The expansion is built once, before the first member applies, so every
    /// path the member addresses is known up front: the destination paths join
    /// the batched ignore probe, and a refusal restores each of them from the
    /// snapshot taken immediately before the member applied.
    DirMove {
        from: String,
        to: String,
        files: Vec<ExpandedMove>,
        operation: &'a EditOperation,
    },
}

impl StagingUnit<'_> {
    /// The normalized paths this member changes.
    fn paths(&self) -> Vec<&str> {
        match self {
            Self::File { path, .. } => vec![path.as_str()],
            Self::Move { from, to, .. } => vec![from.as_str(), to.as_str()],
            Self::DirMove { files, .. } => files
                .iter()
                .flat_map(|file| [file.from.as_str(), file.to.as_str()])
                .collect(),
        }
    }

    /// The paths this member would write, which the batched ignore probe asks
    /// about.
    ///
    /// A member that only rewrites content in place writes nothing, while a
    /// directory move contributes one target per file it relocates, so the probe
    /// answers for every path the member could create.
    fn write_targets(&self) -> Vec<&str> {
        match self {
            Self::File { path, operations } => {
                let writes = operations.iter().copied().any(|operation| {
                    matches!(
                        operation,
                        EditOperation::Create { .. } | EditOperation::Overwrite { .. }
                    )
                });
                if writes {
                    vec![path.as_str()]
                } else {
                    Vec::new()
                }
            }
            Self::Move { to, .. } => vec![to.as_str()],
            Self::DirMove { files, .. } => files.iter().map(|file| file.to.as_str()).collect(),
        }
    }

    /// The operations this member was built from, in change-set order.
    fn operations(&self) -> Vec<EditOperation> {
        match self {
            Self::File { operations, .. } => operations.iter().copied().cloned().collect(),
            Self::Move { operation, .. } => vec![EditOperation::clone(*operation)],
            Self::DirMove { operation, .. } => vec![EditOperation::clone(*operation)],
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

/// Reports whether the context snapshot holds the normalized `path`.
///
/// The snapshot is the wire form of the live context, so the substrate never
/// borrows the context engine: staging learns exactly the same fact — the file
/// is in context — from the protocol value. Its paths are collected into a set
/// once per attempt, so every lookup is a hash probe instead of a scan over the
/// whole item list.
fn in_context(context_paths: &HashSet<&str>, path: &str) -> bool {
    context_paths.contains(path)
}

/// Reports whether `path` is already taken as a write destination.
///
/// A path the attempt has already written is occupied, and so is a path the
/// working tree still holds: staging never touches disk, which is why an
/// `exists()` probe alone answers the wrong question. A path the same change set
/// deletes is free instead — the removal and the rename that takes its place
/// reach the working tree and the index together — so "delete `b`, then move `a`
/// to `b`" resolves exactly like the equivalent delete followed by a create.
fn destination_occupied(staged: &StagedEditsState, repo_root: &Path, path: &str) -> bool {
    if staged.staged_files.contains_key(path) {
        return true;
    }
    if staged.deleted_paths.contains(path) {
        return false;
    }
    repo_root.join(path).exists()
}

/// Groups a change set into the atomic members of a staging attempt.
///
/// Consecutive operations on the same normalized path form one member, which is
/// also the unit the anchored batching of [`stage_file_edits`] resolves against.
/// A directory move is expanded here, before the attempt starts, so its member
/// addresses the files of the subtree instead of the directory itself. A path
/// that cannot be normalized is reported immediately instead of forming a member
/// of its own.
fn build_staging_units<'a>(
    repo_root: &Path,
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
                    normalize_edit_path(repo_root, from),
                    normalize_edit_path(repo_root, to),
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
            EditOperation::MoveDir { from, to } => {
                // A directory move names a whole subtree, so it is expanded
                // into the per-file moves it consists of before the attempt
                // starts. A member left carrying the directory operation would
                // address the directory itself instead of the files underneath
                // it.
                open = None;
                match (
                    normalize_edit_path(repo_root, from),
                    normalize_edit_path(repo_root, to),
                ) {
                    (Ok(from), Ok(to)) => match expand_dir_move(repo_root, &from, &to) {
                        Ok(files) => units.push(StagingUnit::DirMove {
                            from,
                            to,
                            files,
                            operation: edit,
                        }),
                        Err(error) => {
                            failed_files.push(FailedFileEdit { path: from, error });
                        }
                    },
                    (Err((path, error)), _) | (_, Err((path, error))) => {
                        failed_files.push(FailedFileEdit {
                            path,
                            error: *error,
                        });
                    }
                }
            }
            _ => {
                let path = match normalize_edit_path(repo_root, operation_path(edit)) {
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
        EditOperation::Move { from, .. } | EditOperation::MoveDir { from, .. } => from,
    }
}

/// Normalizes one change-set path against the repository.
///
/// The raw spelling travels back with the rejection, because a path that never
/// normalized has no normalized form to report.
fn normalize_edit_path(repo_root: &Path, path: &str) -> Result<String, StagingRejection> {
    normalize_repo_path(repo_root, path).map_err(|err| {
        rejection(
            path,
            EditError::InvalidPath(path.to_string(), err.to_string()),
        )
    })
}

/// Applies one staging member, leaving the state untouched when it is refused.
fn apply_staging_unit(
    repo_root: &Path,
    context_paths: &HashSet<&str>,
    unit: &StagingUnit<'_>,
    state: &mut StagedEditsState,
    protected: &[GlobPattern],
    ignored: &HashSet<String>,
    config_validator: Option<StagedConfigValidator>,
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
            let staged = stage_file_edits(repo_root, context_paths, path, operations, state)
                .map_err(|error| rejection(path, error))?;
            // A repository configuration that would no longer load is refused
            // while the change set is still purely in memory, so the patch-retry
            // loop fixes it instead of a broken file reaching the working tree.
            if let Some(validate) = config_validator {
                validate(path, staged.as_deref()).map_err(|message| {
                    rejection(
                        path,
                        EditError::InvalidConfig {
                            path: path.clone(),
                            message,
                        },
                    )
                })?;
            }
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
            admit_move_destination(repo_root, to, ignored).map_err(|error| rejection(to, error))?;
            stage_move(repo_root, context_paths, from, to, state)
        }
        StagingUnit::DirMove {
            from, to, files, ..
        } => {
            // A protected path is refused before the first file moves, so a
            // directory move can never carry a trust boundary out of the
            // declaration that guards it. The declaration may name a whole
            // subtree or a single file inside it, so both directory names and
            // both sides of every expanded file pair are asked.
            let guarded = protected_dir_move_path(
                protected,
                [from.as_str(), to.as_str()],
                files
                    .iter()
                    .map(|file| (file.from.as_str(), file.to.as_str())),
            );
            if let Some(path) = guarded {
                return Err(rejection(path, EditError::ProtectedPath(path.to_string())));
            }
            // A rename never overwrites, so the destination subtree is proved
            // free before the first file stages. A destination the same change
            // set already deletes is free as well, because staging never touches
            // disk and the removal reaches the file only with the commit.
            // Answering for the whole subtree up front keeps the refusal
            // deterministic instead of depending on the order the expansion
            // happens to enumerate.
            let occupied = files
                .iter()
                .map(|file| file.to.as_str())
                .find(|target| destination_occupied(state, repo_root, target));
            if let Some(path) = occupied {
                return Err(rejection(
                    path,
                    EditError::DestinationExists(path.to_string()),
                ));
            }
            for file in files {
                // Every destination is a write target, so the batched ignore
                // answer and the repository-root containment both apply to it
                // exactly as they do to an explicit write.
                admit_move_destination(repo_root, &file.to, ignored)
                    .map_err(|error| rejection(&file.to, error))?;
            }
            // The subtree moves as one member: a refusal below restores every
            // path the expansion addresses from the snapshot the caller took
            // immediately before this member applied, so no half-moved
            // directory survives the attempt.
            for file in files {
                // A path this attempt already deleted has no content left to
                // relocate, so its deletion keeps its meaning at the source
                // path instead of following the directory.
                if state.deleted_paths.contains(&file.from) {
                    continue;
                }
                stage_rename(repo_root, &file.from, &file.to, state)?;
            }
            Ok(())
        }
    }
}

/// Stages a rename of a file the change set names explicitly.
///
/// An explicit move addresses one file, so its source must be part of the
/// context or of the state this attempt has already built: a rename never
/// guesses which file the model meant. A directory move reaches
/// [`stage_rename`] directly, because its sources are the deterministic contents
/// of the directory it names rather than paths the model typed.
fn stage_move(
    repo_root: &Path,
    context_paths: &HashSet<&str>,
    from: &str,
    to: &str,
    state: &mut StagedEditsState,
) -> Result<(), StagingRejection> {
    // A source this attempt created in memory has no file on disk, so it is
    // carried forward exactly like a replace of a freshly staged file.
    if !in_context(context_paths, from) && !state.staged_files.contains_key(from) {
        return Err(rejection(from, EditError::NotInContext(from.to_string())));
    }
    stage_rename(repo_root, from, to, state)
}

/// Moves the staged or on-disk content of `from` to `to`.
///
/// The source is resolved from the state this attempt has already built before
/// the working tree is consulted, so a chained rename carries its content
/// forward instead of looking for a file that never reached disk. The
/// destination must still be free, because a rename never overwrites; a path
/// the same change set deletes counts as free, since staging never touches disk
/// and its removal reaches the file only with the commit.
fn stage_rename(
    repo_root: &Path,
    from: &str,
    to: &str,
    state: &mut StagedEditsState,
) -> Result<(), StagingRejection> {
    for path in [from, to] {
        verify_path_within_repo_for_write(repo_root, path).map_err(|err| {
            rejection(
                path,
                EditError::InvalidPath(path.to_string(), err.to_string()),
            )
        })?;
    }
    if destination_occupied(state, repo_root, to) {
        return Err(rejection(to, EditError::DestinationExists(to.to_string())));
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
    context_paths: &HashSet<&str>,
    path: &str,
    edits: &[&EditOperation],
    initial_staged: &StagedEditsState,
) -> Result<Option<String>, EditError> {
    // A staged file is a write target, so the escape boundary must be
    // inspectable: an uncanonicalizable repository root refuses the write
    // instead of silently admitting it.
    verify_path_within_repo_for_write(repo_root, path)
        .map_err(|err| EditError::InvalidPath(path.to_string(), err.to_string()))?;
    let in_context = in_context(context_paths, path);
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
            EditOperation::MoveDir { .. } => {
                return Err(EditError::InvalidPath(
                    path.to_string(),
                    "Directory moves cannot be staged as per-file edits".to_string(),
                ));
            }
        }
        index += 1;
    }

    Ok(content)
}

#[cfg(test)]
mod tests;
