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

/// Stages `contents` into `staging`, syncs it, renames it over `target`, and
/// syncs the directory that holds the result.
fn stage_and_rename(staging: &Path, target: &Path, contents: &[u8]) -> Result<()> {
    {
        let mut file = std::fs::File::create(staging)
            .with_context(|| format!("Failed to create temp file at {}", staging.display()))?;
        file.write_all(contents)
            .with_context(|| format!("Failed to write temp file at {}", staging.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file at {}", staging.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temp file at {}", staging.display()))?;
    }

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
    sync_parent_directory(target)
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
fn sync_parent_directory(target: &Path) -> Result<()> {
    let parent = parent_directory(target);

    let directory = match std::fs::File::open(parent) {
        Ok(directory) => directory,
        Err(error) if rejects_directory_sync(&error) => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open directory {} for sync", parent.display()))
        }
    };

    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error) if rejects_directory_sync(&error) => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to sync directory {} after renaming into it",
                parent.display()
            )
        }),
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
fn sync_parent_directory(_target: &Path) -> Result<()> {
    Ok(())
}

/// Builds a staging path for `target` that is unique within this process.
fn staging_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("file");
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!("{file_name}.tmp.{}.{sequence}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{staging_path, write_atomic};
    use std::path::Path;
    use tempfile::tempdir;

    /// Names of the staging files currently present in `dir`.
    fn staging_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
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
}
