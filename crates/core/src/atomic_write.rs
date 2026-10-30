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
//! parent directory is opened and synced after the rename. Filesystems that
//! reject directory syncing keep the weaker durability they offer; a genuine
//! sync failure is reported instead of being hidden.
//!
//! The staging name carries both the process id and a process-wide monotonic
//! sequence number. A fixed `<file>.tmp.<pid>` name would let two writers of
//! one process truncate each other's staging file and publish a corrupted
//! result; that hazard is real for history rewrites, which hold no subsystem
//! lock. This module is the single authoritative implementation of the staging
//! policy, so every storage engine shares it instead of re-deriving it.
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
    sync_parent_directory(target).with_context(|| {
        format!(
            "Failed to sync the directory that holds {}",
            target.display()
        )
    })
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
/// does not make it durable. A rejection is ignored so writes keep working on
/// filesystems that do not sync directories; they simply retain their weaker
/// durability level. A genuine I/O failure is reported, because the durability
/// of the rename is then unknown.
#[cfg(unix)]
fn sync_parent_directory(target: &Path) -> std::io::Result<()> {
    let parent = parent_directory(target);

    let directory = match std::fs::File::open(parent) {
        Ok(directory) => directory,
        Err(error) if rejects_directory_sync(&error) => return Ok(()),
        Err(error) => return Err(error),
    };

    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error) if rejects_directory_sync(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether `error` reports a filesystem that does not sync directories.
#[cfg(unix)]
fn rejects_directory_sync(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Unsupported
    )
}

/// Directory syncing is a POSIX durability step with no portable equivalent.
#[cfg(not(unix))]
fn sync_parent_directory(_target: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Builds a staging path for `target` that is unique within this process.
fn staging_path(target: &Path) -> PathBuf {
    unique_sibling_path(target, ".tmp")
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
pub(crate) const STAGING_TEMP_MARKER: &str = ".tauqe-tmp";

/// Marker appended to a file name to name the sibling that keeps the original
/// content while a change set is committed.
pub(crate) const STAGING_BACKUP_MARKER: &str = ".tauqe-bak";

/// Returns the original file name of a staging temp sibling of `name`.
pub(crate) fn staging_temp_original_name(name: &str) -> Option<&str> {
    staging_sibling_original_name(name, STAGING_TEMP_MARKER)
}

/// Returns the original file name of a staging backup sibling of `name`.
pub(crate) fn staging_backup_original_name(name: &str) -> Option<&str> {
    staging_sibling_original_name(name, STAGING_BACKUP_MARKER)
}

/// Returns the file name `name` stages or backs up, if `name` is a sibling
/// carrying `marker`.
fn staging_sibling_original_name<'a>(name: &'a str, marker: &str) -> Option<&'a str> {
    let position = name.rfind(marker)?;
    let original = &name[..position];
    if original.is_empty() || !is_staging_suffix(&name[position + marker.len()..]) {
        return None;
    }
    Some(original)
}

/// Reports whether `suffix` is the `.<pid>.<sequence>` tail of a sibling name,
/// or empty for a sibling named by the earlier fixed-name policy.
fn is_staging_suffix(suffix: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match suffix.strip_prefix('.') {
        None => suffix.is_empty(),
        Some(rest) => match rest.split_once('.') {
            Some((pid, sequence)) => digits(pid) && digits(sequence),
            None => false,
        },
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

/// One target of an all-or-nothing change set, prepared for a sibling commit.
///
/// A write stages its new content in a unique sibling file and syncs it, so a
/// crash cannot publish a zero-length or truncated file. The commit moves any
/// existing target aside to a unique backup sibling and moves the staged
/// content into place; every step is a rename on a single filesystem, so the
/// original's permissions, timestamps and symlinks survive and rollback never
/// depends on the file size. A removal stages no content and only moves the
/// original aside.
pub(crate) struct SiblingCommit {
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
    pub(crate) fn stage(target: &Path, content: &[u8]) -> std::io::Result<Self> {
        reject_directory_target(target)?;
        let staged = unique_sibling_path(target, STAGING_TEMP_MARKER);
        stage_bytes(&staged, content)?;
        // Carry the original's permissions over to the staged content so a
        // write never silently widens or narrows access.
        preserve_permissions_of(&staged, target);
        Ok(Self::new(target, Some(staged)))
    }

    /// Prepares the removal of `target`, which stages no content.
    pub(crate) fn removal(target: &Path) -> std::io::Result<Self> {
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
    pub(crate) fn commit(&mut self) -> std::io::Result<()> {
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
    pub(crate) fn rollback(&self) {
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
    pub(crate) fn discard_staged_content(&self) {
        if let Some(staged) = &self.staged {
            let _ = std::fs::remove_file(staged);
        }
    }

    /// Drops the backup once the whole change set is committed.
    pub(crate) fn finish(self) {
        if self.original_moved {
            let _ = std::fs::remove_file(&self.backup);
            let _ = sync_parent_directory(&self.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        staging_backup_original_name, staging_path, staging_temp_original_name, write_atomic,
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
                name.contains(".tmp.")
                    || staging_temp_original_name(name).is_some()
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

            let backup = format!("{original}.tauqe-bak.{pid}.8");
            assert_eq!(staging_backup_original_name(&backup), Some(original));
            assert_eq!(staging_temp_original_name(&backup), None);
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

        // An ordinary file is not a staging sibling.
        assert_eq!(staging_temp_original_name("source.rs"), None);
        assert_eq!(staging_backup_original_name("source.rs"), None);
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
}
