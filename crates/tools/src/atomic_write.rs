//! Atomic, crash-safe file replacement.
//!
//! Plan, review, and history storage all persist small documents by staging
//! them into a sibling temporary file and renaming that file over the target.
//! The rename is atomic, so a reader observes either the previous complete
//! file or the new complete file, never a partially written one.
//!
//! Without the `sync_all` barrier the rename could still publish a
//! zero-length or truncated file after a crash, so the staged payload is
//! flushed and synced before the rename.
//!
//! A rename is recorded in the parent directory, not in the renamed file, so
//! the file's own `sync_all` does not make it durable either. On Unix the
//! parent directory is opened and synced after the rename, and both steps
//! belong to the barrier: a parent that cannot be opened never reaches the
//! sync and is reported exactly like a genuine sync failure, while a
//! filesystem that answers the sync itself as unsupported keeps the weaker
//! durability it offers.
//!
//! The barrier covers the whole lifecycle of a durable document, not only the
//! moment its state is published. [`remove_atomic`] removes a file and syncs
//! the directory that held it, because an unlink is recorded there exactly as
//! a rename is, so a crash cannot resurrect a removed entry. Symmetrically,
//! [`create_dir_all_durable`] creates the directories a later write needs and
//! syncs the parent of every directory it created, because the entry of a new
//! directory lives in its parent, so a crash cannot lose the chain of
//! directories that is about to hold a marker. A durable document therefore
//! travels through this module from the first directory it requires until the
//! moment it is gone.
//! The staging name carries both the process id and a process-wide monotonic
//! sequence number. A fixed name would let two writers of one process truncate
//! each other's staging file and publish a corrupted result; that hazard is
//! real for history rewrites, which hold no subsystem lock. This module is the
//! single authoritative implementation of the staging policy, so every storage
//! engine shares it instead of re-deriving it.
//!
//! That policy names every sibling it leaves behind with one of two markers:
//! [`STAGING_TEMP_MARKER`] for staged content and [`STAGING_BACKUP_MARKER`] for
//! an original moved aside. Both the atomic replacement ([`write_atomic`]) and
//! the working-tree sibling commit ([`SiblingCommit`]) follow it, so the
//! `*.tauqe-tmp*` and `*.tauqe-bak*` Git exclude rules, the sibling-name
//! parsers, and crash recovery recognize and reclaim every sibling this crate
//! can leave behind. The name grammar also carries ownership: a sibling name
//! embeds the process id of the writer that created it, and
//! [`staging_temp_sibling`] and [`staging_backup_sibling`] parse that name
//! into the file it stages or backs up and the writer that owns it.
//! [`is_process_alive`] probes whether such a writer still runs — the
//! question a reclaim pass must answer before it may touch a sibling another
//! process might still own. The restore a reclaim pass performs follows the
//! same rule: [`move_without_replace`] publishes a sibling under its target
//! only when no destination exists yet, so a target a concurrent writer
//! created inside the window keeps its content instead of being replaced by
//! stale bytes.
//!
//! The same policy covers the highest-stakes write in the system: committing a
//! validated change set over the developer's source files. [`SiblingCommit`]
//! synces new content into a unique sibling of its target, moves any original
//! aside to a unique backup sibling, renames the staged content into place, and
//! syncs the parent directory, so a working-tree commit carries exactly the
//! durability barrier a plan, review, or history write does.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter mixed into every staging file name.
///
/// A counter, not a timestamp, keeps two writers of one process apart even
/// when they start within the same clock tick.
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Atomically replaces `target` with `contents`.
///
/// The payload is staged into a process-unique sibling temporary file, flushed,
/// and `fsync`ed before the atomic rename. A failure before the rename removes
/// the staging file, so a rejected write never leaves an orphaned artifact
/// behind and never disturbs the existing target. On Unix the parent directory
/// is synced after the rename, so the new directory entry survives a crash; a
/// failure at that point is reported because the durability of the replacement
/// is then unknown.
pub fn write_atomic(target: &Path, contents: &[u8]) -> Result<()> {
    // The staging file, the write, the `fsync` barrier, and the atomic rename
    // all run off the async executor thread, so a slow disk never stalls the
    // bounded turn, provider streaming, or RPC handling. Nested inside another
    // blocking scope the helper degrades to a direct call.
    crate::blocking::run_blocking(move || {
        let staging = staging_path(target);

        let replaced = stage_and_rename(&staging, target, contents);

        // A failure before the rename leaves the staged payload behind under a
        // name nothing will ever read again; drop it. After a successful rename
        // the staging path no longer exists, so this branch never removes the
        // freshly published target.
        if replaced.is_err() {
            let _ = std::fs::remove_file(&staging);
        }

        replaced
    })
}
/// Durably removes `target`.
///
/// An unlink is recorded in the directory that held the file, not in the file
/// itself, so removing the entry alone would let a crash resurrect it. The
/// removal therefore syncs that directory, exactly as the rename performed by
/// [`write_atomic`] does. A target that is already absent is success and syncs
/// nothing, because no entry exists that a crash could bring back; every other
/// failure is reported with the path it addressed.
pub fn remove_atomic(target: &Path) -> Result<()> {
    // The unlink and its directory barrier both run off the async executor
    // thread, so a slow disk never stalls the bounded turn, provider
    // streaming, or RPC handling.
    crate::blocking::run_blocking(move || {
        match std::fs::remove_file(target) {
            Ok(()) => sync_holding_directory(target),
            // A target that is already absent is the state a completed
            // removal leaves behind: no entry survives that a crash could
            // resurrect, so there is nothing left to sync.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to remove {}", target.display()))
            }
        }
    })
}

/// Creates `dir` and every missing ancestor of it, durably.
///
/// `std::fs::create_dir_all` records each new directory in its parent, so the
/// entries it added can be lost by a crash unless those parents are synced.
/// The directories that do not exist yet are collected before the call, and
/// the parent of every directory the call created is synced afterwards,
/// shallowest first, so a crash cannot detach the new chain from the directory
/// that roots it. A directory that already exists is success and syncs
/// nothing; creation and sync failures are reported with the path they
/// addressed.
pub fn create_dir_all_durable(dir: &Path) -> Result<()> {
    // Building the chain and running its sync barriers both happen off the
    // async executor thread, so a slow disk never stalls the bounded turn.
    crate::blocking::run_blocking(move || {
        let created = missing_directories(dir);
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create the directory {}", dir.display()))?;
        for directory in &created {
            sync_holding_directory(directory)?;
        }
        Ok(())
    })
}

/// The directories of `dir` that do not exist yet, shallowest first.
///
/// `std::fs::create_dir_all` reports nothing about which entries it added, so
/// the barrier derives them from the filesystem before the call: an entry
/// that does not exist yet is exactly the entry the call is about to add. The
/// walk stops at the first ancestor that exists, because every ancestor above
/// it exists as well; an empty ancestor, which is how a bare relative path
/// terminates, ends the walk too.
fn missing_directories(dir: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    for ancestor in dir.ancestors() {
        if ancestor.as_os_str().is_empty() || ancestor.exists() {
            break;
        }
        missing.push(ancestor.to_path_buf());
    }
    missing.reverse();
    missing
}
/// Stages `contents` into `staging`, syncs it, carries the permissions of an
/// existing `target` onto the staged copy, renames it over `target`, and syncs
/// the directory that holds the result.
fn stage_and_rename(staging: &Path, target: &Path, contents: &[u8]) -> Result<()> {
    stage_bytes(staging, contents)
        .with_context(|| format!("Failed to stage content at {}", staging.display()))?;
    preserve_permissions_of(staging, target);
    std::fs::rename(staging, target).with_context(|| {
        format!(
            "Failed to atomically rename {} to {}",
            staging.display(),
            target.display()
        )
    })?;

    // The file's own `sync_all` covers its data and inode, not the directory
    // entry that now points at it. Syncing the parent directory makes the
    // rename itself durable, so a power loss cannot roll it back.
    sync_holding_directory(target)
}

/// The directory that holds `target`.
///
/// A bare file name reports an empty parent that cannot be opened; the current
/// directory holds it instead.
#[cfg(unix)]
fn parent_directory(target: &Path) -> &Path {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Opens the directory that holds `target` and syncs it.
///
/// A rename is recorded in the parent directory, so the file's own `sync_all`
/// does not make it durable. Opening the parent is therefore part of the
/// barrier and not a preliminary: a directory that admits the rename (write
/// and execute) but rejects the read-only open (read) publishes the new entry
/// with no descriptor left to sync, and that lost barrier is reported.
///
/// The `sync_all` outcome is where a filesystem without directory-sync support
/// answers, so only the kinds that name an unsupported operation are absorbed
/// and such a filesystem keeps the weaker durability it offers. Every other
/// failure is reported, because the durability of the rename is then unknown.
#[cfg(unix)]
fn sync_parent_directory(target: &Path) -> std::io::Result<()> {
    let parent = parent_directory(target);

    // The rename already reached the directory, so an unopenable parent cannot
    // be recovered from later; report it instead of claiming a barrier that
    // never ran.
    let directory = std::fs::File::open(parent)?;

    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error) if rejects_directory_sync(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether `error` reports a filesystem that does not implement directory
/// syncing.
///
/// Only the two kinds that name an unsupported operation qualify: `EINVAL` is
/// the answer POSIX permits for a directory `fsync` a filesystem does not
/// implement, and `ENOTSUP`/`EOPNOTSUPP` reports the same refusal explicitly.
/// A permission denial is deliberately absent: a sandbox or a filtering layer
/// produces it as readily as a filesystem can, and answering "synced" for a
/// call that never ran would hide a barrier the caller believes in.
#[cfg(unix)]
fn rejects_directory_sync(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
    )
}

/// Directory syncing is a POSIX durability step with no portable equivalent.
#[cfg(not(unix))]
fn sync_parent_directory(_target: &Path) -> std::io::Result<()> {
    Ok(())
}
/// Syncs the directory that records the change performed on `entry`.
///
/// An unlink, a rename, and a new directory are all recorded in the parent
/// directory of the entry they address, so every barrier of this module
/// reaches durability through this one step.
fn sync_holding_directory(entry: &Path) -> Result<()> {
    sync_parent_directory(entry)
        .with_context(|| format!("Failed to sync the directory of {}", entry.display()))
}
/// Builds a staging path for `target` that is unique within this process.
///
/// The sibling carries [`STAGING_TEMP_MARKER`], the marker of the crate's
/// single staging-artifact contract, so an interrupted replacement leaves an
/// orphan that the `*.tauqe-tmp*` Git exclude rules ignore, that
/// [`staging_temp_original_name`] resolves back to the file it belongs to, and
/// that crash recovery reclaims.
fn staging_path(target: &Path) -> PathBuf {
    unique_sibling_path(target, STAGING_TEMP_MARKER)
}

/// Builds a sibling path of `target` that carries `marker` and is unique within
/// this process.
///
/// The sibling lives next to its target so every rename stays on a single
/// filesystem and is therefore atomic.
fn unique_sibling_path(target: &Path, marker: &str) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("file");
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(
        "{file_name}{marker}.{}.{sequence}",
        std::process::id()
    ))
}

/// Marker appended to a file name to name the sibling that stages new content.
pub const STAGING_TEMP_MARKER: &str = ".tauqe-tmp";

/// Marker appended to a file name to name the sibling that keeps the original
/// content while a change set is committed.
pub const STAGING_BACKUP_MARKER: &str = ".tauqe-bak";

/// A staging sibling of another file, parsed from its name.
///
/// A sibling's name is its whole identity: it names the file the sibling
/// stages or backs up and, when the sibling was written by this policy, the
/// process that created it. Reclaiming a live writer's sibling deletes the
/// staged content that writer is about to publish or moves aside the live
/// backup it still owns, so the parse exposes the writer next to the file it
/// belongs to and [`is_process_alive`] answers whether that writer is gone.
#[derive(Debug, PartialEq, Eq)]
pub struct StagingSibling<'a> {
    /// The file name the sibling stages or backs up.
    pub original: &'a str,
    /// The process id of the writer that created the sibling.
    ///
    /// A sibling this policy writes always embeds its writer's process id;
    /// `None` names a sibling of the earlier fixed-name policy, which
    /// attributes its writer to nobody.
    pub owner_pid: Option<u32>,
}

/// Parses `name` as a staging temp sibling, yielding the file it stages and
/// the writer that created it.
///
/// Returns `None` for every name that is not a sibling carrying
/// [`STAGING_TEMP_MARKER`].
pub fn staging_temp_sibling(name: &str) -> Option<StagingSibling<'_>> {
    staging_sibling(name, STAGING_TEMP_MARKER)
}

/// Parses `name` as a staging backup sibling, yielding the file it backs up
/// and the writer that created it.
///
/// Returns `None` for every name that is not a sibling carrying
/// [`STAGING_BACKUP_MARKER`].
pub fn staging_backup_sibling(name: &str) -> Option<StagingSibling<'_>> {
    staging_sibling(name, STAGING_BACKUP_MARKER)
}

/// Returns the original file name of a staging temp sibling of `name`.
pub fn staging_temp_original_name(name: &str) -> Option<&str> {
    staging_temp_sibling(name).map(|sibling| sibling.original)
}

/// Returns the original file name of a staging backup sibling of `name`.
pub fn staging_backup_original_name(name: &str) -> Option<&str> {
    staging_backup_sibling(name).map(|sibling| sibling.original)
}

/// Parses `name` as a sibling carrying `marker`, yielding the file it stages
/// or backs up and the writer that created it.
///
/// The sibling-name grammar lives here and nowhere else: the tail after the
/// marker is empty for a sibling of the earlier fixed-name policy, or a
/// `.<pid>.<sequence>` pair naming the writer and its staging order. Any
/// other tail — including a process id too wide for `u32` — names no sibling.
fn staging_sibling<'a>(name: &'a str, marker: &str) -> Option<StagingSibling<'a>> {
    let position = name.rfind(marker)?;
    let original = &name[..position];
    if original.is_empty() {
        return None;
    }
    let suffix = &name[position + marker.len()..];
    let owner_pid = if suffix.is_empty() {
        // The earlier fixed-name policy attributes its writer to nobody.
        None
    } else {
        let (pid, sequence) = suffix.strip_prefix('.')?.split_once('.')?;
        if !is_ascii_digits(pid) || !is_ascii_digits(sequence) {
            return None;
        }
        Some(pid.parse().ok()?)
    };
    Some(StagingSibling {
        original,
        owner_pid,
    })
}

/// Reports whether `part` is a non-empty run of ASCII digits.
fn is_ascii_digits(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

/// Reports whether a process with the id `pid` is currently running.
///
/// On Unix this is the POSIX zero-signal probe: `kill(pid, 0)` delivers no
/// signal, it only asks whether a process with that id exists and may be
/// signaled by the caller. `EPERM` therefore reports a process that exists
/// but may not be touched, and only `ESRCH` ("no such process") reports a
/// writer that is gone — every other answer is conservative, because
/// reporting a live writer dead invites a reclaim pass to delete the staged
/// content or move aside the live backup that writer still owns.
///
/// A process id is reused once its process exits and is reaped, so the probe
/// answers whether some process runs under the id, not whether it is the
/// writer that created a given sibling; a reused id is reported alive, which
/// errs on the side of leaving artifacts in place.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    // Safety: `kill` with signal `0` inspects the process table and delivers
    // nothing, so the probe cannot disturb any process.
    let probe = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if probe == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Process liveness is a POSIX capability with no portable equivalent: a
/// non-Unix platform cannot probe a process id, so every sibling is reported
/// as owned by a live writer and attributed siblings are never reclaimed
/// there.
#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

/// The result of a no-replace move of a staging sibling onto its target.
///
/// These are the two answers a reclaim pass acts on: the move published the
/// source content under the destination name, or another writer already owns
/// the destination.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NoReplaceMove {
    /// The source content now sits at the destination and the source name is
    /// gone.
    Moved,
    /// The destination already existed and kept every byte it carried; the
    /// source name was left untouched.
    Occupied,
}

/// Moves `source` onto `destination` without ever replacing an existing
/// destination.
///
/// A reclaim pass restores the backup sibling of a writer that is provably
/// gone, yet it runs in the multi-writer model the ownership gates declare:
/// another harness process may publish the destination while the pass runs.
/// An existence probe followed by [`std::fs::rename`] is not one operation —
/// on POSIX the rename replaces a destination created in between — so the
/// sweep would silently overwrite a file a live process believes it just
/// published. This primitive therefore decides restore-versus-yield in a
/// single atomic step: [`std::fs::hard_link`] fails with
/// [`std::io::ErrorKind::AlreadyExists`] exactly when the destination exists,
/// and the source name is removed only after the link is in place.
///
/// A staging sibling is always created next to its target, so the link never
/// crosses a filesystem. An existing destination keeps every byte it carries,
/// and every genuine I/O failure is propagated so the caller leaves the
/// sibling in place for a later sweep.
pub(crate) fn move_without_replace(
    source: &Path,
    destination: &Path,
) -> std::io::Result<NoReplaceMove> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => {
            // The destination carries the source content now, so dropping the
            // source name completes the move. A failure here leaves both names
            // holding the same bytes while the destination is already
            // published.
            std::fs::remove_file(source)?;
            Ok(NoReplaceMove::Moved)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(NoReplaceMove::Occupied)
        }
        Err(error) => Err(error),
    }
}

/// Copies the permissions of `target` onto `staged` when `target` exists.
///
/// A replacement must never change who may access the file it overwrites: a
/// journal deliberately restricted to its owner stays restricted after a
/// rewrite. Both durability paths of this module apply the rule before their
/// rename, so the atomic replacement and the sibling commit cannot disagree.
/// A target that does not exist yet keeps the process default mode, so a newly
/// created document is unaffected, and a filesystem that rejects the permission
/// change keeps that default instead of failing the replacement.
fn preserve_permissions_of(staged: &Path, target: &Path) {
    if let Ok(metadata) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(staged, metadata.permissions());
    }
}

/// Writes `content` into `staged` and syncs it to disk.
///
/// Without the `sync_all` barrier a crash after the rename could publish a
/// zero-length or truncated file, so the staged payload is flushed and synced
/// before any rename can observe it.
///
/// The staging file itself carries the process default mode (`0666` masked by
/// the umask). The permission contract of a replacement is applied by the
/// caller through [`preserve_permissions_of`] before the rename, so this
/// function stays concerned with bytes only.
fn stage_bytes(staged: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(staged)?;
    file.write_all(content)?;
    file.flush()?;
    file.sync_all()
}

/// Rejects a target that is a directory.
///
/// The sibling commit moves an original aside and renames staged content into
/// its place, which only makes sense for a file. Checking here keeps the
/// primitive from silently relying on validation performed by its caller.
fn reject_directory_target(target: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path is a directory",
        )),
        _ => Ok(()),
    }
}

/// Removes the ancestors of `target` that the change set left empty.
///
/// A change set that relocates or deletes files leaves their source
/// directories behind with no member inside them. Git tracks files rather than
/// directories, so an emptied directory stays in the working tree while being
/// invisible to the index, and every later status query reports it as an
/// unexplained artifact. The deepest ancestor is removed first, which lets a
/// whole chain the change set emptied collapse, and the walk stops at the first
/// directory that still holds something and at `root` itself, which is never
/// removed.
///
/// A removal that fails — because the directory is not empty, because a
/// concurrent writer put something back, or because the filesystem refuses it —
/// ends the walk instead of being reported: the committed change set is already
/// durable, and a surviving empty directory never invalidates it.
pub fn prune_empty_parents(root: &Path, target: &Path) {
    let mut current = target.parent();
    while let Some(dir) = current {
        if dir == root || !dir.starts_with(root) {
            return;
        }
        if std::fs::remove_dir(dir).is_err() {
            return;
        }
        current = dir.parent();
    }
}

/// One target of an all-or-nothing change set, prepared for a sibling commit.
///
/// A write stages its new content in a unique sibling file and syncs it, so a
/// crash cannot publish a zero-length or truncated file. The commit moves any
/// existing target aside to a unique backup sibling and moves the staged
/// content into place; every step is a rename on a single filesystem, so the
/// original's permissions, timestamps and symlinks survive and rollback never
/// depends on the file size. A removal stages no content and only moves the
/// original aside.
pub struct SiblingCommit {
    target: PathBuf,
    backup: PathBuf,
    /// Staged new content, present only for a write.
    staged: Option<PathBuf>,
    /// Set once the original was moved aside to `backup`.
    original_moved: bool,
    /// Set once the staged content was renamed over `target`.
    published: bool,
}

impl SiblingCommit {
    /// Stages `content` into a unique sibling of `target` and syncs it.
    pub fn stage(target: &Path, content: &[u8]) -> std::io::Result<Self> {
        reject_directory_target(target)?;
        let staged = unique_sibling_path(target, STAGING_TEMP_MARKER);
        stage_bytes(&staged, content)?;
        // Carry the original's permissions over to the staged content so a
        // write never silently widens or narrows access.
        preserve_permissions_of(&staged, target);
        Ok(Self::new(target, Some(staged)))
    }

    /// Prepares the removal of `target`, which stages no content.
    pub fn removal(target: &Path) -> std::io::Result<Self> {
        reject_directory_target(target)?;
        Ok(Self::new(target, None))
    }

    fn new(target: &Path, staged: Option<PathBuf>) -> Self {
        Self {
            target: target.to_path_buf(),
            backup: unique_sibling_path(target, STAGING_BACKUP_MARKER),
            staged,
            original_moved: false,
            published: false,
        }
    }

    /// Moves the original aside (if any) and the staged content into place.
    ///
    /// Both steps are renames on a single filesystem, so a failure at any point
    /// leaves enough information for [`Self::rollback`] to restore the target
    /// exactly. The parent directory is synced afterwards so the renames
    /// themselves survive a crash.
    pub fn commit(&mut self) -> std::io::Result<()> {
        if self.target.symlink_metadata().is_ok() {
            // Drop any stale backup left by an interrupted earlier commit
            // before moving the current original aside.
            let _ = std::fs::remove_file(&self.backup);
            std::fs::rename(&self.target, &self.backup)?;
            self.original_moved = true;
        }
        if let Some(staged) = &self.staged {
            std::fs::rename(staged, &self.target)?;
            // The target now carries the staged content, so a rollback has to
            // remove it again before restoring the original.
            self.published = true;
        }
        sync_parent_directory(&self.target)
    }

    /// Restores the target to its pre-commit state.
    ///
    /// Staged content that reached the target is removed, and an original moved
    /// to a backup is restored by a single rename, which preserves its
    /// permissions, timestamps and symlink target.
    ///
    /// A commit that fails before the staged content reaches the target leaves
    /// the original untouched, so the target is removed only once `published`
    /// reports that staged content is what sits there. Removing it
    /// unconditionally would destroy the intact original on exactly the failure
    /// path this rollback exists to protect.
    pub fn rollback(&self) {
        if let Some(staged) = &self.staged {
            if self.published {
                let _ = std::fs::remove_file(&self.target);
            }
            let _ = std::fs::remove_file(staged);
        }
        if self.original_moved {
            let _ = std::fs::rename(&self.backup, &self.target);
        }
    }

    /// Removes the staged content of a change that never reached its commit.
    pub fn discard_staged_content(&self) {
        if let Some(staged) = &self.staged {
            let _ = std::fs::remove_file(staged);
        }
    }

    /// Drops the backup once the whole change set is committed.
    pub fn finish(self) {
        if self.original_moved {
            let _ = std::fs::remove_file(&self.backup);
            let _ = sync_parent_directory(&self.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        create_dir_all_durable, is_process_alive, move_without_replace, remove_atomic,
        staging_backup_original_name, staging_backup_sibling, staging_path,
        staging_temp_original_name, staging_temp_sibling, write_atomic, NoReplaceMove,
        SiblingCommit,
    };
    use std::path::Path;
    use tempfile::tempdir;

    /// Names of the staging files currently present in `dir`.
    fn staging_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                staging_temp_original_name(name).is_some()
                    || staging_backup_original_name(name).is_some()
            })
            .collect()
    }

    #[test]
    fn replaces_the_target_without_leaving_a_staging_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("document.json");

        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn staging_paths_are_unique_within_a_process() {
        let target = Path::new("/tmp/tauqe-staging-uniqueness.json");

        let first = staging_path(target);
        let second = staging_path(target);

        assert_ne!(first, second);
        assert_eq!(first.parent(), target.parent());
        // The process id separates concurrent processes, the sequence number
        // separates concurrent writers of one process.
        assert!(first
            .to_string_lossy()
            .contains(&std::process::id().to_string()));
        // The sibling carries the crate's staging marker, so an orphan left by
        // an interrupted replacement resolves back to the file it belongs to.
        let name = first
            .file_name()
            .expect("a staging path always carries a file name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            staging_temp_original_name(&name),
            Some("tauqe-staging-uniqueness.json")
        );
    }

    #[test]
    fn concurrent_writes_to_one_target_never_collide() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let target = root.join("shared.json");

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let target = target.clone();
                std::thread::spawn(move || write_atomic(&target, format!("writer-{i}").as_bytes()))
            })
            .collect();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.starts_with("writer-"));
        assert!(
            staging_leftovers(&root).is_empty(),
            "staging files left behind"
        );
    }

    #[test]
    fn a_failed_replacement_removes_the_staging_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("occupied");

        // Renaming a staged file onto an existing directory is rejected, so the
        // replacement fails after the payload was already synced to disk.
        std::fs::create_dir(&target).unwrap();

        assert!(write_atomic(&target, b"payload").is_err());
        assert!(target.is_dir());
        assert!(
            staging_leftovers(dir.path()).is_empty(),
            "a failed write must not leave a staging file behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_directory_that_cannot_be_opened_reports_the_missing_barrier() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let parent = dir.path().join("write_only");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("document.json");
        std::fs::write(&target, "original").unwrap();

        // Write and execute admit the staging file, the rename, and the
        // metadata read; the missing read bit rejects the read-only open of
        // the parent, so no descriptor exists to sync the rename through.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300)).unwrap();
        let replaced = write_atomic(&target, b"updated");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            replaced.is_err(),
            "a barrier that never ran must be reported instead of claimed as success"
        );
        // The rename is already published, so the new content is on disk; what
        // the reported failure states is that the directory entry carrying it
        // was never synced.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
        assert!(staging_leftovers(&parent).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_bare_file_name_resolves_to_the_current_directory() {
        use super::parent_directory;

        assert_eq!(parent_directory(Path::new("document.json")), Path::new("."));
        assert_eq!(
            parent_directory(Path::new("/tmp/document.json")),
            Path::new("/tmp")
        );
    }

    #[test]
    fn a_staged_sibling_publishes_nothing_until_the_commit() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("source.rs");
        std::fs::write(&target, "original").unwrap();

        let mut change = SiblingCommit::stage(&target, b"updated").unwrap();

        // The new content waits, already synced, in a sibling file while the
        // target keeps its original bytes.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert_eq!(staging_leftovers(dir.path()).len(), 1);

        change.commit().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
        assert_eq!(
            staging_leftovers(dir.path()).len(),
            1,
            "the original is kept aside for a rollback"
        );

        change.rollback();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_commit_never_deletes_the_untouched_original() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let parent = dir.path().join("locked");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("source.rs");
        std::fs::write(&target, "original").unwrap();

        let mut change = SiblingCommit::stage(&target, b"updated").unwrap();

        // A read-only parent rejects the first rename of the commit (original
        // to backup), so the failure happens before any staged content could
        // reach the target and the original is still the file on disk.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let commit = change.commit();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(commit.is_err(), "the commit must observe the failed rename");

        change.rollback();

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original",
            "a rollback must never delete an original the commit never replaced"
        );
        assert!(staging_leftovers(&parent).is_empty());
    }

    #[test]
    fn finishing_a_committed_sibling_drops_its_backup() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("source.rs");
        std::fs::write(&target, "original").unwrap();

        let mut change = SiblingCommit::stage(&target, b"updated").unwrap();
        change.commit().unwrap();
        change.finish();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn two_staged_changes_of_one_path_own_distinct_siblings() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("source.rs");
        std::fs::write(&target, "original").unwrap();

        let _first = SiblingCommit::stage(&target, b"first").unwrap();
        let _second = SiblingCommit::stage(&target, b"second").unwrap();

        let mut staged: Vec<String> = staging_leftovers(dir.path())
            .into_iter()
            .filter(|name| staging_temp_original_name(name).is_some())
            .map(|name| std::fs::read_to_string(dir.path().join(name)).unwrap())
            .collect();
        staged.sort();

        // A shared staging name would let one writer truncate the other's
        // content before either rename observed it.
        assert_eq!(staged, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn staging_sibling_names_resolve_back_to_their_original_name() {
        let pid = std::process::id();

        for original in ["source.rs", "a.tauqe-bak"] {
            let temp = format!("{original}.tauqe-tmp.{pid}.7");
            assert_eq!(staging_temp_original_name(&temp), Some(original));
            assert_eq!(staging_backup_original_name(&temp), None);
            let sibling = staging_temp_sibling(&temp).unwrap();
            assert_eq!(sibling.original, original);
            assert_eq!(sibling.owner_pid, Some(pid));

            let backup = format!("{original}.tauqe-bak.{pid}.8");
            assert_eq!(staging_backup_original_name(&backup), Some(original));
            assert_eq!(staging_temp_original_name(&backup), None);
            let sibling = staging_backup_sibling(&backup).unwrap();
            assert_eq!(sibling.original, original);
            assert_eq!(sibling.owner_pid, Some(pid));
        }

        // A sibling named by the earlier fixed-name policy still resolves, so
        // crash recovery keeps matching artifacts written before the change.
        assert_eq!(
            staging_temp_original_name("source.rs.tauqe-tmp"),
            Some("source.rs")
        );
        assert_eq!(
            staging_backup_original_name("source.rs.tauqe-bak"),
            Some("source.rs")
        );

        // The fixed-name policy attributes its writer to nobody.
        let sibling = staging_temp_sibling("source.rs.tauqe-tmp").unwrap();
        assert_eq!(sibling.original, "source.rs");
        assert_eq!(sibling.owner_pid, None);
        let sibling = staging_backup_sibling("source.rs.tauqe-bak").unwrap();
        assert_eq!(sibling.original, "source.rs");
        assert_eq!(sibling.owner_pid, None);

        // An ordinary file is not a staging sibling.
        assert_eq!(staging_temp_original_name("source.rs"), None);
        assert_eq!(staging_backup_original_name("source.rs"), None);
        assert_eq!(staging_temp_sibling("source.rs"), None);
        assert_eq!(staging_backup_sibling("source.rs"), None);
    }

    #[test]
    fn a_no_replace_move_publishes_the_source_content_onto_an_absent_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("notes.md.tauqe-bak.4242.1");
        let destination = dir.path().join("notes.md");
        std::fs::write(&source, "original content").unwrap();

        let outcome = move_without_replace(&source, &destination).unwrap();

        assert_eq!(outcome, NoReplaceMove::Moved);
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "original content"
        );
        assert!(!source.exists(), "the source name is gone after the move");
    }

    #[test]
    fn a_no_replace_move_preserves_an_occupied_destination() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("notes.md.tauqe-bak.4242.2");
        let destination = dir.path().join("notes.md");
        std::fs::write(&source, "stale original").unwrap();
        std::fs::write(&destination, "freshly committed").unwrap();

        let outcome = move_without_replace(&source, &destination).unwrap();

        assert_eq!(outcome, NoReplaceMove::Occupied);
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "freshly committed",
            "an occupied destination keeps every byte it carries"
        );
        assert!(source.exists(), "the occupied move leaves the source alone");
    }

    #[test]
    fn a_no_replace_move_of_a_missing_source_reports_the_failure() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("missing.md.tauqe-bak.4242.3");
        let destination = dir.path().join("missing.md");

        let outcome = move_without_replace(&source, &destination);

        assert!(
            outcome.is_err(),
            "a missing source is an I/O failure, never a move"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn the_current_process_is_reported_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    // `git` is a hard prerequisite of this crate's suite, so the probe is
    // exercised against a real short-lived child.
    #[cfg(unix)]
    #[test]
    fn a_reaped_child_process_is_reported_dead() {
        let mut child = std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("git is installed");
        let pid = child.id();
        child.wait().expect("the child exits on its own");
        assert!(!is_process_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn rewriting_a_restricted_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("history.jsonl");
        std::fs::write(&target, "first").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&target, b"second").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a rewrite must never widen the mode of the file it replaces"
        );
        assert!(staging_leftovers(dir.path()).is_empty());
    }
    #[test]
    fn a_durable_removal_deletes_the_target_and_leaves_no_staging_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("active-transaction.json");
        std::fs::write(&target, "{}").unwrap();

        remove_atomic(&target).unwrap();

        assert!(!target.exists());
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn a_durable_removal_of_an_absent_target_is_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("active-transaction.json");

        // An absent target is the state a completed removal leaves behind, so
        // the barrier reports success instead of an error.
        remove_atomic(&target).unwrap();

        assert!(!target.exists());
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_removal_the_parent_directory_rejects_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let parent = dir.path().join("read_only");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("active-transaction.json");
        std::fs::write(&target, "{}").unwrap();

        // Without the write bit the parent records no unlink at all, so the
        // removal fails before any directory sync could run.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let removed = remove_atomic(&target);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(removed.is_err(), "a refused removal is reported");
        assert!(target.is_file(), "a refused removal keeps the target");
    }

    #[test]
    fn a_durable_creation_builds_the_missing_directory_chain() {
        let dir = tempdir().unwrap();
        let chain = dir.path().join("state").join("nested").join("deep");

        create_dir_all_durable(&chain).unwrap();

        assert!(chain.is_dir());
        assert!(dir.path().join("state").join("nested").is_dir());
    }

    #[test]
    fn a_durable_creation_of_an_existing_directory_is_success() {
        let dir = tempdir().unwrap();
        let existing = dir.path().join("state");
        std::fs::create_dir(&existing).unwrap();

        // The directory already exists, so the call creates nothing and has
        // nothing to sync.
        create_dir_all_durable(&existing).unwrap();

        assert!(existing.is_dir());
    }

    #[test]
    fn a_directory_chain_blocked_by_a_file_reports_the_creation_error() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("state");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        let chain = blocker.join("nested");

        let created = create_dir_all_durable(&chain);

        assert!(created.is_err(), "a blocked chain is reported");
        assert!(blocker.is_file());
        assert!(!chain.exists());
    }
}
