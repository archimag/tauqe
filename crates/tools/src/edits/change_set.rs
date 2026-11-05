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

use super::admission::{admit_move_destination, admit_write, ignored_target_paths, IgnoreAnswer};
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
///
/// Staged content is bytes: a rename carries the payload of a file verbatim, so
/// an image, a font, or a serialized asset relocates exactly as a source file
/// does. The text pipeline reads a staged entry through a strict UTF-8 decode,
/// and the only text view of the whole state is
/// [`StagedEditsState::staged_text_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedEditsState {
    pub staged_files: HashMap<String, Vec<u8>>,
    pub deleted_paths: HashSet<String>,
}

impl StagedEditsState {
    /// Returns the staged files that decode losslessly as UTF-8, as owned text.
    ///
    /// Staged content is bytes, so this is the only text view of the state: an
    /// entry whose bytes are not valid UTF-8 is absent rather than lossily
    /// rendered, so a prompt or a streaming preview never shows a payload the
    /// turn cannot address as text. A path recorded as deleted holds no content
    /// and therefore never appears.
    pub fn staged_text_files(&self) -> HashMap<String, String> {
        self.staged_files
            .iter()
            .filter_map(|(path, bytes)| {
                std::str::from_utf8(bytes)
                    .ok()
                    .map(|text| (path.clone(), text.to_string()))
            })
            .collect()
    }
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

/// The refusal bookkeeping of one staging attempt.
///
/// A member is refused in one of two places: a path that cannot even be
/// normalized refuses the member before it is built, and an operation the state
/// machine rejects refuses it while it applies. Both write here, so both leave
/// the same trace: the refusal is reported, and every path the member addressed
/// is claimed. A later member that touches a claimed path is therefore refused
/// for the same reason instead of being skipped without a trace.
#[derive(Debug, Default)]
struct StagingFailureLog {
    /// The refusals, in the order the members were built and applied.
    entries: Vec<FailedFileEdit>,
    /// The reason each claimed path is refused under.
    reasons: HashMap<String, String>,
}

impl StagingFailureLog {
    /// Records `rejection` and claims every path the member addressed.
    ///
    /// `addressed` names the paths of the member and the path that caused the
    /// refusal is claimed beside them, so a member that failed on one side of
    /// a rename claims both of its sides, exactly as a member refused while it
    /// applied does.
    fn refuse(&mut self, rejection: StagingRejection, addressed: &[&str]) {
        let (path, error) = rejection;
        let reason = error.to_string();
        self.claim(addressed, &path, &reason);
        self.entries.push(FailedFileEdit {
            path,
            error: *error,
        });
    }

    /// Refuses a member that touches a path an earlier refusal claimed.
    ///
    /// Returns `true` when the member was refused and recorded. The reported
    /// rejection names the earlier reason, so an operation the change set can
    /// no longer apply stays visible to the patch-retry loop, and the remaining
    /// paths of the member join the earlier refusal under its own reason.
    fn refuse_if_claimed(&mut self, addressed: &[&str]) -> bool {
        let Some((path, reason)) = addressed
            .iter()
            .find_map(|path| self.reasons.get_key_value(*path))
            .map(|(path, reason)| (path.clone(), reason.clone()))
        else {
            return false;
        };
        let message = format!(
            "the change set refused an earlier operation that addressed this path, \
             so this operation was not applied: {reason}"
        );
        self.claim(addressed, &path, &reason);
        self.entries.push(FailedFileEdit {
            path: path.clone(),
            error: EditError::InvalidPath(path, message),
        });
        true
    }

    /// Claims every path of a refused member under `reason`.
    ///
    /// The earliest reason wins, so a member refused because it touches a path
    /// an earlier member already claimed reports the one rejection that started
    /// the chain instead of a paraphrase of a paraphrase.
    fn claim(&mut self, addressed: &[&str], causing: &str, reason: &str) {
        for path in addressed.iter().copied().chain(std::iter::once(causing)) {
            self.reasons
                .entry(path.to_string())
                .or_insert_with(|| reason.to_string());
        }
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
/// member fails on a path the two of them share. Refusals are recorded in one
/// [`StagingFailureLog`]: a member refused while it applies claims the same
/// paths as one refused before the attempt starts, and a member refused because
/// it touches a claimed path is reported with the reason of the refusal that
/// claimed it instead of disappearing from the result.
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
    let (units, mut failures) = build_staging_units(repo_root, edits);
    // The ignore probe is one batched `git check-ignore` call, so the whole
    // change set is resolved against Git before the first member applies. A
    // directory move contributes one candidate per file it expands into, which
    // is why the candidates come from the built members rather than from the
    // raw operations. The typed answer separates a target Git cannot inspect
    // from a probe Git refused: the first has no ignore rules at all, while the
    // second refuses every write instead of admitting it on a guess.
    let mut write_targets: Vec<String> = units
        .iter()
        .flat_map(|unit| unit.write_targets())
        .map(str::to_string)
        .collect();
    write_targets.sort();
    write_targets.dedup();
    let ignore_answer = ignored_target_paths(repo_root, &write_targets);
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
        // A member that touches a path an earlier refusal claimed is refused
        // for the same reason and reported like any other, so no operation
        // disappears from the result the patch-retry loop reads.
        if failures.refuse_if_claimed(paths) {
            continue;
        }
        // Every member records the state of the paths it addresses immediately
        // before it applies, so a refusal restores exactly the state that member
        // found. A file member snapshots the body of its own path; a move member
        // records the membership of every pair it relocates instead, because its
        // transfer only permutes the staged keys and its inversion therefore moves
        // the content back instead of cloning a body per addressed path. A
        // replacement addressed to a rename destination is rolled back to the
        // renamed content either way, instead of the opening state of the attempt,
        // in which the destination did not exist yet and the moved content would be
        // lost.
        let rollback = MemberRollback::capture(unit, &state);
        let applied = apply_staging_unit(
            repo_root,
            &context_paths,
            unit,
            &mut state,
            &protected,
            &ignore_answer,
            config_validator,
        );
        match applied {
            Ok(()) => applied_units.push(index),
            Err(failure) => {
                rollback.restore(&mut state);
                failures.refuse(failure, paths);
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
        failed_files: failures.entries,
    }
}

/// The state one path held before a staging member touched it.
///
/// A rejected member is restored to this snapshot, so an operation that succeeded
/// in an earlier member keeps its effect: a replacement that follows a rename is
/// rolled back to the renamed content, not to the opening state of the attempt.
struct PathSnapshot {
    content: Option<Vec<u8>>,
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

/// The membership a relocated pair held before its move member applied.
///
/// A move member only permutes the keys of the staged map, so inverting one of
/// its pairs needs no copy of the relocated body: the content moves back from the
/// destination to the source and the deletion bits return to the values they
/// held. The destination is free when an admitted member starts — the
/// member-level pre-pass refuses an occupied one — so a pair records the staged
/// membership of its source and the deletion bits of both sides.
struct MovePairSnapshot<'a> {
    /// Source path of the pair.
    from: &'a str,
    /// Destination path of the pair.
    to: &'a str,
    /// Whether the source held staged content of its own.
    from_staged: bool,
    /// Whether the source was already recorded as deleted.
    from_deleted: bool,
    /// Whether the destination was already recorded as deleted.
    to_deleted: bool,
}

impl<'a> MovePairSnapshot<'a> {
    /// Records the membership `from` and `to` hold right now.
    fn capture(state: &StagedEditsState, from: &'a str, to: &'a str) -> Self {
        Self {
            from,
            to,
            from_staged: state.staged_files.contains_key(from),
            from_deleted: state.deleted_paths.contains(from),
            to_deleted: state.deleted_paths.contains(to),
        }
    }

    /// Puts the pair back into the membership it held before the member applied.
    ///
    /// The destination holds content only when the pair actually transferred, so
    /// a pair the member never reached leaves both of its keys untouched. A body
    /// the pair read from the working tree is dropped again, because its source
    /// had no staged entry of its own; the one source that keeps such a body is
    /// itself a destination of another pair, whose own inversion moves it on.
    fn restore(&self, state: &mut StagedEditsState, destinations: &HashSet<&str>) {
        if let Some(content) = state.staged_files.remove(self.to) {
            if self.from_staged || destinations.contains(self.from) {
                state.staged_files.insert(self.from.to_string(), content);
            }
        }
        if self.from_deleted {
            state.deleted_paths.insert(self.from.to_string());
        } else {
            state.deleted_paths.remove(self.from);
        }
        if self.to_deleted {
            state.deleted_paths.insert(self.to.to_string());
        } else {
            state.deleted_paths.remove(self.to);
        }
    }
}

/// The state a staging member restores when it is refused.
///
/// A file member rewrites content in place, so it snapshots the body of the path
/// it addresses. A move member only permutes the keys of the staged map, so it
/// records the membership of every pair it relocates and inverts the permutation
/// instead: a subtree move copies no file body on the success path, and its
/// rejection moves the staged content back between the same keys.
enum MemberRollback<'a> {
    /// The content snapshot of the path a file member rewrites.
    File(String, PathSnapshot),
    /// The prior membership of the pairs a move member relocates.
    Pairs(Vec<MovePairSnapshot<'a>>),
}

impl<'a> MemberRollback<'a> {
    /// Records the state `unit` is about to change.
    fn capture(unit: &'a StagingUnit<'_>, state: &StagedEditsState) -> Self {
        match unit {
            StagingUnit::File { path, .. } => {
                Self::File(path.clone(), PathSnapshot::capture(state, path))
            }
            StagingUnit::Move { from, to, .. } => {
                Self::Pairs(vec![MovePairSnapshot::capture(state, from, to)])
            }
            StagingUnit::DirMove { files, .. } => Self::Pairs(
                files
                    .iter()
                    .map(|file| MovePairSnapshot::capture(state, &file.from, &file.to))
                    .collect(),
            ),
        }
    }

    /// Puts every path the refused member addressed back into the captured state.
    fn restore(&self, state: &mut StagedEditsState) {
        match self {
            Self::File(path, snapshot) => snapshot.restore(state, path),
            Self::Pairs(pairs) => {
                // A source of one pair can also be the destination of another,
                // and the content of the earlier pair travels through it. The
                // pairs are therefore inverted from the last one applied to the
                // first, which hands that content back along the chain.
                let destinations: HashSet<&str> = pairs.iter().map(|pair| pair.to).collect();
                for pair in pairs.iter().rev() {
                    pair.restore(state, &destinations);
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
    /// the batched ignore probe, and a refusal inverts each pair from the
    /// membership recorded immediately before the member applied.
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
/// that cannot be normalized refuses its own operation and claims its paths
/// instead of forming a member of its own.
fn build_staging_units<'a>(
    repo_root: &Path,
    edits: &'a [EditOperation],
) -> (Vec<StagingUnit<'a>>, StagingFailureLog) {
    let mut units: Vec<StagingUnit<'a>> = Vec::new();
    let mut failures = StagingFailureLog::default();
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
                    // A rename addresses both of its sides, so a refusal claims
                    // the canonical form of the side that normalized and the raw
                    // spelling of the side that did not. A later member touching
                    // either side is then refused by exactly the rule that
                    // governs a member refused while it applied.
                    (Err(failure), to) => {
                        let to = to.unwrap_or_else(|failure| failure.0);
                        failures.refuse(failure, &[to.as_str()]);
                    }
                    (from, Err(failure)) => {
                        let from = from.unwrap_or_else(|failure| failure.0);
                        failures.refuse(failure, &[from.as_str()]);
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
                        // The expansion refused before a single file moved, so
                        // the two directories it named are claimed instead.
                        Err(error) => {
                            failures.refuse((from, Box::new(error)), &[to.as_str()]);
                        }
                    },
                    (Err(failure), to) => {
                        let to = to.unwrap_or_else(|failure| failure.0);
                        failures.refuse(failure, &[to.as_str()]);
                    }
                    (from, Err(failure)) => {
                        let from = from.unwrap_or_else(|failure| failure.0);
                        failures.refuse(failure, &[from.as_str()]);
                    }
                }
            }
            _ => {
                let path = match normalize_edit_path(repo_root, operation_path(edit)) {
                    Ok(path) => path,
                    // A file operation addresses exactly one path, so the
                    // refusal claims that path and no other.
                    Err(failure) => {
                        failures.refuse(failure, &[]);
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

    (units, failures)
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
    ignore_answer: &IgnoreAnswer,
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
                admit_write(path, ignore_answer).map_err(|error| rejection(path, error))?;
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
                    state
                        .staged_files
                        .insert(path.clone(), content.into_bytes());
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
            admit_move_destination(repo_root, to, ignore_answer)
                .map_err(|error| rejection(to, error))?;
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
            // Every destination is a write target, so the batched ignore answer
            // and the repository-root containment both apply to it exactly as
            // they do to an explicit write. Both are resolved here, once per
            // file, before the first file of the subtree stages; the loop below
            // then only transfers content.
            for file in files {
                admit_move_destination(repo_root, &file.to, ignore_answer)
                    .map_err(|error| rejection(&file.to, error))?;
            }
            // The subtree moves as one member: the caller recorded the membership
            // of every pair before this member applied, so a refusal below moves
            // the content back and no half-moved directory survives the attempt.
            for file in files {
                // A path this attempt already deleted has no content left to
                // relocate, so its deletion keeps its meaning at the source
                // path instead of following the directory.
                if state.deleted_paths.contains(&file.from) {
                    continue;
                }
                // Every admission fact of this destination was resolved in the
                // pre-pass above, so the transfer only moves the content.
                transfer_rename(repo_root, &file.from, &file.to, state)?;
            }
            Ok(())
        }
    }
}

/// Stages a rename of a file the change set names explicitly.
///
/// An explicit move addresses one file, so its source must be part of the
/// context or of the state this attempt has already built: a rename never
/// guesses which file the model meant. A directory move resolves the admission
/// of its whole subtree in one pre-pass and then reaches [`transfer_rename`]
/// directly, because its sources are the deterministic contents of the directory
/// it names rather than paths the model typed.
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

/// Resolves the admission of one explicitly named rename and performs it.
///
/// An explicit move addresses a single file the model typed, so both of its sides
/// are proved here: a source or destination that escapes the repository is
/// refused, and a destination that is already taken refuses the rename. The
/// source is resolved from the state this attempt has already built before the
/// working tree is consulted, so a chained rename carries its content forward
/// instead of looking for a file that never reached disk. The destination must
/// still be free, because a rename never overwrites; a path the same change set
/// deletes counts as free, since staging never touches disk and its removal
/// reaches the file only with the commit.
///
/// A directory move resolves the same facts for its whole subtree in one
/// member-level pre-pass and then calls [`transfer_rename`] directly, so a
/// relocated file is never re-verified and never re-probed.
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
    transfer_rename(repo_root, from, to, state)
}

/// Moves the staged or on-disk content of `from` to `to` without re-resolving
/// admission.
///
/// A staged body is moved between the map keys instead of being cloned, so a
/// member that relocates a whole subtree copies no file content. The bytes
/// travel exactly as they were staged, and a source this attempt has not staged
/// is read as bytes too, so a payload that is not text — an image, a font, or a
/// serialized asset — relocates verbatim instead of being refused. The pair
/// either transfers completely or leaves the state exactly as it found it, which
/// is what lets the caller invert it by moving the content back.
fn transfer_rename(
    repo_root: &Path,
    from: &str,
    to: &str,
    state: &mut StagedEditsState,
) -> Result<(), StagingRejection> {
    let content = match state.staged_files.remove(from) {
        Some(content) => content,
        None if state.deleted_paths.contains(from) => {
            return Err(rejection(from, EditError::FileNotFound(from.to_string())));
        }
        None => {
            let source = repo_root.join(from);
            if !source.is_file() {
                return Err(rejection(from, EditError::FileNotFound(from.to_string())));
            }
            std::fs::read(&source)
                .map_err(|err| rejection(from, EditError::Io(from.to_string(), err)))?
        }
    };
    state.staged_files.insert(to.to_string(), content);
    state.deleted_paths.remove(to);
    state.deleted_paths.insert(from.to_string());
    Ok(())
}

/// Decodes the staged bytes of a text edit.
///
/// The text pipeline addresses bytes as text, so a staged entry that is not
/// valid UTF-8 cannot serve as the base of a replacement, an overwrite, or a
/// staged read. The refusal is the classification a disk read of those bytes
/// produces, and never a silent reload from the working tree, which would
/// discard what the change set has already staged.
fn decode_staged_text(path: &str, bytes: &[u8]) -> Result<String, EditError> {
    std::str::from_utf8(bytes).map(str::to_string).map_err(|_| {
        EditError::Io(
            path.to_string(),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            ),
        )
    })
}

/// Stages the run of operations one change set addresses to `path`.
///
/// None represents a staged deletion. Disk reads are deferred until a replace
/// needs the original content, so create/delete also work for non-text files.
///
/// The text pipeline reads a staged entry through a strict UTF-8 decode: a
/// payload that does not decode refuses the member with the `InvalidData`
/// classification the same bytes produce when they are read from disk, instead
/// of reloading the working tree and discarding what the change set staged.
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
        match initial_staged.staged_files.get(path) {
            Some(bytes) => Some(decode_staged_text(path, bytes)?),
            None => None,
        }
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
