use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

use super::ContextManager;

/// Returns the deepest ancestor of `path` that exists on disk, or the symlink
/// itself when it dangles.
///
/// Walking upward stops as soon as a component exists or is present as a
/// symlink, so the result can be canonicalized to learn where a not-yet-created
/// suffix would physically land.
fn deepest_existing_ancestor(path: &Path) -> &Path {
    let mut ancestor = path;
    while !ancestor.exists() && ancestor.symlink_metadata().is_err() {
        match ancestor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => ancestor = parent,
            _ => break,
        }
    }
    ancestor
}

/// Verifies that `relative_path` does not resolve to a symlink outside
/// `repo_root`.
///
/// Every component of the relative path is inspected from the repository root
/// downward with `symlink_metadata` (lstat), which detects a symlink even when
/// its target does not exist yet. Each symlink target is read explicitly with
/// `read_link` and resolved to its deepest existing ancestor, so a link that
/// dangles outside the repository is rejected instead of being silently
/// traversed while missing parent directories are created.
pub fn verify_path_within_repo(repo_root: &Path, relative_path: &str) -> Result<()> {
    let Ok(canonical_root) = repo_root.canonicalize() else {
        // If repo_root cannot be canonicalized (e.g. does not exist in a synthetic test),
        // we cannot inspect symlinks on disk.
        return Ok(());
    };

    let full_path = repo_root.join(relative_path);

    let Ok(relative) = full_path.strip_prefix(repo_root) else {
        bail!("Path '{}' escapes repository root", relative_path);
    };

    let mut current = PathBuf::from(repo_root);
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => current.push(name),
            // `normalize_path` collapses `.`/`..` and rejects absolute paths, so
            // any other component here means an unnormalized path slipped
            // through; treat it as escaping.
            _ => bail!("Path '{}' contains an invalid component", relative_path),
        }

        let Ok(metadata) = current.symlink_metadata() else {
            // This component does not exist yet, so neither can any deeper one.
            // The remaining suffix is created as regular entries.
            break;
        };

        if !metadata.file_type().is_symlink() {
            continue;
        }

        let target = std::fs::read_link(&current).map_err(|err| {
            anyhow!(
                "Path '{}' symlink '{}' cannot be read: {}",
                relative_path,
                current.display(),
                err
            )
        })?;
        let absolute_target = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .map(|parent| parent.join(&target))
                .unwrap_or(target)
        };
        let resolved = deepest_existing_ancestor(&absolute_target)
            .canonicalize()
            .map_err(|err| {
                anyhow!(
                    "Path '{}' symlink '{}' target '{}' cannot be resolved: {}",
                    relative_path,
                    current.display(),
                    absolute_target.display(),
                    err
                )
            })?;
        if !resolved.starts_with(&canonical_root) {
            bail!(
                "Path '{}' escapes repository root via symlink '{}' pointing to '{}'",
                relative_path,
                current.display(),
                resolved.display()
            );
        }
    }

    Ok(())
}

impl ContextManager {
    pub fn normalize_path(&self, p: &str) -> Result<String> {
        let path = Path::new(p);
        let mut components = Vec::new();

        for component in path.components() {
            match component {
                std::path::Component::Normal(c) => components.push(c.to_string_lossy().to_string()),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if components.pop().is_none() {
                        bail!("Path escapes repository root: {}", p);
                    }
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    bail!("Absolute paths are not allowed: {}", p);
                }
            }
        }

        if components.is_empty() {
            bail!("Empty path is not allowed");
        }

        let clean = components.join("/");
        verify_path_within_repo(&self.repo_root, &clean)?;
        Ok(clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tauqe_protocol::ContextAccess;
    use tempfile::tempdir;

    #[test]
    fn test_normalize_path() {
        let cm = ContextManager::new(PathBuf::from("/tmp"));
        assert_eq!(
            cm.normalize_path("crates/core/src/lib.rs").unwrap(),
            "crates/core/src/lib.rs"
        );
        assert_eq!(
            cm.normalize_path("./crates/core/../core/src/lib.rs")
                .unwrap(),
            "crates/core/src/lib.rs"
        );
        assert!(cm.normalize_path("../outside.rs").is_err());
        assert!(cm.normalize_path("/abs/path.rs").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_symlink_escaping_repo_is_rejected() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let outside_root = outside.path().to_path_buf();

        let secret_file = outside_root.join("secret.txt");
        std::fs::write(&secret_file, "super secret").unwrap();

        // Symlink pointing to a file outside repo
        let sym_file = repo_root.join("sym_file.txt");
        std::os::unix::fs::symlink(&secret_file, &sym_file).unwrap();

        // Symlink pointing to a directory outside repo
        let sym_dir = repo_root.join("sym_dir");
        std::os::unix::fs::symlink(&outside_root, &sym_dir).unwrap();

        let mut cm = ContextManager::new(repo_root.clone());

        // File symlink escapes repo
        assert!(cm.normalize_path("sym_file.txt").is_err());
        assert!(cm
            .add_file("sym_file.txt", ContextAccess::ReadOnly)
            .is_err());

        // Directory symlink escapes repo (both existing and non-existing files under it)
        assert!(cm.normalize_path("sym_dir/secret.txt").is_err());
        assert!(cm.normalize_path("sym_dir/new_file.txt").is_err());
        assert!(cm
            .add_file("sym_dir/secret.txt", ContextAccess::ReadOnly)
            .is_err());

        // Symlink pointing inside repo is allowed
        let internal_dir = repo_root.join("src");
        std::fs::create_dir_all(&internal_dir).unwrap();
        let internal_file = internal_dir.join("lib.rs");
        std::fs::write(&internal_file, "pub fn ok() {}").unwrap();

        let internal_sym = repo_root.join("sym_lib.rs");
        std::os::unix::fs::symlink(&internal_file, &internal_sym).unwrap();

        assert!(cm.normalize_path("sym_lib.rs").is_ok());
        assert!(cm.add_file("sym_lib.rs", ContextAccess::ReadOnly).is_ok());

        // Reading context files ignores any file that escapes repo
        let files = cm.read_context_files();
        assert!(files.iter().any(|f| f.path == "sym_lib.rs"));
        assert!(!files.iter().any(|f| f.path == "sym_file.txt"));
    }

    #[test]
    #[cfg(unix)]
    fn test_dangling_symlink_escaping_repo_is_rejected() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let outside_root = outside.path().to_path_buf();

        // A directory symlink whose target does not exist yet and lives outside
        // the repository. lstat still reports it, so it must be rejected rather
        // than blindly traversed.
        let dangling_target = outside_root.join("missing_dir");
        let sym_dir = repo_root.join("dangling_dir");
        std::os::unix::fs::symlink(&dangling_target, &sym_dir).unwrap();

        let mut cm = ContextManager::new(repo_root.clone());

        assert!(cm.normalize_path("dangling_dir/sub/new_file.txt").is_err());
        assert!(cm
            .add_file("dangling_dir/sub/new_file.txt", ContextAccess::Editable)
            .is_err());
        assert!(!outside_root.join("missing_dir").exists());

        // A dangling symlink pointing back inside the repository stays within
        // the boundary and is accepted.
        let internal_target = repo_root.join("generated");
        let internal_sym = repo_root.join("dangling_internal");
        std::os::unix::fs::symlink(&internal_target, &internal_sym).unwrap();
        assert!(cm.normalize_path("dangling_internal/file.txt").is_ok());
    }
}
