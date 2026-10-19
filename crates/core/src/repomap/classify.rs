//! Deterministic classification of tracked repository paths.
//!
//! Git tracks symbolic links as ordinary entries, so the raw tracked file list
//! cannot tell a regular file apart from a link. Every path is therefore
//! classified through `symlink_metadata` (lstat), which reports the entry itself
//! without dereferencing it: the caller learns what physically lives on disk
//! instead of following the link to an unrelated file.

use std::path::Path;

use crate::context::path::verify_path_within_repo;

/// Resolution status of a symbolic link found among tracked repository paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkStatus {
    /// The link resolves to an existing path inside the repository.
    Internal,
    /// The link resolves to an existing path that escapes the repository root.
    OutsideRepository,
    /// The link target does not exist.
    Broken,
}

/// A tracked symbolic link: its stored target and its boundary status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkTarget {
    /// Link target exactly as stored on disk, empty when it cannot be read.
    pub target: String,
    /// Whether the link stays inside the repository or is broken.
    pub status: SymlinkStatus,
}

/// Classification of a tracked repository path without following symbolic links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoPathKind {
    /// A regular file that is not a symbolic link.
    RegularFile,
    /// A symbolic link with its target and boundary status.
    Symlink(SymlinkTarget),
    /// Anything else: a directory, a missing entry, or an unreadable path.
    Other,
}

/// Classifies `relative_path` inside `repo_root` using `symlink_metadata` (lstat).
///
/// The entry is inspected as it exists on disk, so a symbolic link is never
/// implicitly dereferenced into the regular file it points to.
pub fn classify_repo_path(repo_root: &Path, relative_path: &str) -> RepoPathKind {
    let full_path = repo_root.join(relative_path);
    let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
        return RepoPathKind::Other;
    };

    if metadata.file_type().is_symlink() {
        RepoPathKind::Symlink(classify_symlink(repo_root, relative_path, &full_path))
    } else if metadata.is_file() {
        RepoPathKind::RegularFile
    } else {
        RepoPathKind::Other
    }
}

fn classify_symlink(repo_root: &Path, relative_path: &str, full_path: &Path) -> SymlinkTarget {
    let Ok(target) = std::fs::read_link(full_path) else {
        return SymlinkTarget {
            target: String::new(),
            status: SymlinkStatus::Broken,
        };
    };

    let status = if !full_path.exists() {
        SymlinkStatus::Broken
    } else if verify_path_within_repo(repo_root, relative_path).is_err() {
        SymlinkStatus::OutsideRepository
    } else {
        SymlinkStatus::Internal
    };

    SymlinkTarget {
        target: target.to_string_lossy().into_owned(),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn classifies_regular_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.rs"), "pub fn main() {}\n").unwrap();

        assert_eq!(
            classify_repo_path(root, "main.rs"),
            RepoPathKind::RegularFile
        );
    }

    #[test]
    fn classifies_missing_path_as_other() {
        let dir = tempdir().unwrap();

        assert_eq!(
            classify_repo_path(dir.path(), "missing.rs"),
            RepoPathKind::Other
        );
    }

    #[test]
    fn classifies_directory_as_other() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();

        assert_eq!(classify_repo_path(dir.path(), "src"), RepoPathKind::Other);
    }

    #[test]
    #[cfg(unix)]
    fn classifies_internal_symlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", root.join("alias.rs")).unwrap();

        match classify_repo_path(root, "alias.rs") {
            RepoPathKind::Symlink(link) => {
                assert_eq!(link.target, "src/lib.rs");
                assert_eq!(link.status, SymlinkStatus::Internal);
            }
            other => panic!("expected symlink classification, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn classifies_symlink_escaping_repository() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = dir.path();
        let outside_file = outside.path().join("secret.rs");
        std::fs::write(&outside_file, "pub fn secret() {}\n").unwrap();
        std::os::unix::fs::symlink(&outside_file, root.join("secret.rs")).unwrap();

        match classify_repo_path(root, "secret.rs") {
            RepoPathKind::Symlink(link) => {
                assert_eq!(link.status, SymlinkStatus::OutsideRepository);
                assert_eq!(link.target, outside_file.to_string_lossy());
            }
            other => panic!("expected symlink classification, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn classifies_broken_symlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink("missing.rs", root.join("dangling.rs")).unwrap();

        match classify_repo_path(root, "dangling.rs") {
            RepoPathKind::Symlink(link) => {
                assert_eq!(link.target, "missing.rs");
                assert_eq!(link.status, SymlinkStatus::Broken);
            }
            other => panic!("expected symlink classification, got {other:?}"),
        }
    }
}
