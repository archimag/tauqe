use anyhow::{bail, Result};
use std::path::Path;

use super::ContextManager;

/// Verifies that `relative_path` does not traverse or resolve to a symlink
/// outside `repo_root`.
///
/// If `relative_path` (or its deepest existing ancestor) points outside the
/// canonical repository root, returns an error.
pub fn verify_path_within_repo(repo_root: &Path, relative_path: &str) -> Result<()> {
    let Ok(canonical_root) = repo_root.canonicalize() else {
        // If repo_root cannot be canonicalized (e.g. does not exist in a synthetic test),
        // we cannot inspect symlinks on disk.
        return Ok(());
    };

    let full_path = repo_root.join(relative_path);

    // 1. If full_path exists (or is a symlink on disk):
    // symlink_metadata().is_ok() detects broken symlinks as well as existing files/dirs.
    if full_path.symlink_metadata().is_ok() {
        match full_path.canonicalize() {
            Ok(canonical_target) => {
                if !canonical_target.starts_with(&canonical_root) {
                    bail!(
                        "Path '{}' escapes repository root via symlink pointing to '{}'",
                        relative_path,
                        canonical_target.display()
                    );
                }
            }
            Err(err) => {
                bail!(
                    "Path '{}' cannot be canonicalized (broken symlink or inaccessible): {}",
                    relative_path,
                    err
                );
            }
        }
    } else {
        // 2. If full_path does not exist yet (e.g. a new file to create):
        // Walk up to find the deepest existing ancestor directory.
        let mut ancestor = full_path.as_path();
        while !ancestor.exists() && ancestor.symlink_metadata().is_err() {
            match ancestor.parent() {
                Some(p) if !p.as_os_str().is_empty() => ancestor = p,
                _ => break,
            }
        }

        if ancestor.exists() || ancestor.symlink_metadata().is_ok() {
            match ancestor.canonicalize() {
                Ok(canonical_ancestor) => {
                    if !canonical_ancestor.starts_with(&canonical_root) {
                        bail!(
                            "Path '{}' parent directory escapes repository root via symlink pointing to '{}'",
                            relative_path,
                            canonical_ancestor.display()
                        );
                    }
                }
                Err(err) => {
                    bail!(
                        "Path '{}' ancestor directory '{}' cannot be canonicalized: {}",
                        relative_path,
                        ancestor.display(),
                        err
                    );
                }
            }
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
        assert!(cm.add_file("sym_file.txt", ContextAccess::ReadOnly).is_err());

        // Directory symlink escapes repo (both existing and non-existing files under it)
        assert!(cm.normalize_path("sym_dir/secret.txt").is_err());
        assert!(cm.normalize_path("sym_dir/new_file.txt").is_err());
        assert!(cm.add_file("sym_dir/secret.txt", ContextAccess::ReadOnly).is_err());

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
}
