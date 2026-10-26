use anyhow::{anyhow, bail, Result};
#[cfg(unix)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

use super::ContextManager;

/// Upper bound on the cached canonical repository roots.
///
/// A session works on one repository at a time, so a handful of entries is
/// enough. Clearing the map on overflow keeps memory bounded without an
/// eviction policy.
#[cfg(unix)]
const CANONICAL_ROOT_CACHE_LIMIT: usize = 8;

/// Identity of the directory a path resolves to.
#[cfg(unix)]
type DirIdentity = (u64, u64);

/// A canonical repository root together with the identity of the directory it
/// described when it was resolved.
///
/// The identity keeps a stale entry from outliving the directory it was
/// computed for: once the root is replaced, the identities differ and the
/// canonical form is recomputed instead of reused.
#[cfg(unix)]
struct CachedCanonicalRoot {
    canonical: PathBuf,
    identity: DirIdentity,
}

#[cfg(unix)]
fn cached_canonical_roots() -> &'static Mutex<HashMap<PathBuf, CachedCanonicalRoot>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedCanonicalRoot>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Identifies the directory `path` resolves to, or `None` when it is not an
/// existing directory.
#[cfg(unix)]
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    Some((metadata.dev(), metadata.ino()))
}

/// Resolves the canonical form of `repo_root`, reusing a previous resolution
/// while the directory still carries the identity it had when it was cached.
///
/// `canonicalize` walks every component of the path with `lstat` and resolves
/// each symlink it meets. Verification runs for every context mutation and for
/// every pinned candidate, so resolving the root once per repository — kept
/// honest by one cheap `stat` — removes the dominant syscall cost of the
/// context layer.
fn canonical_repo_root(repo_root: &Path) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let identity = dir_identity(repo_root);
        if let (Some(identity), Ok(cache)) = (identity, cached_canonical_roots().lock()) {
            if let Some(cached) = cache.get(repo_root) {
                if cached.identity == identity {
                    return Ok(cached.canonical.clone());
                }
            }
        }

        let canonical = repo_root.canonicalize()?;

        if let Some(identity) = identity {
            if let Ok(mut cache) = cached_canonical_roots().lock() {
                if cache.len() >= CANONICAL_ROOT_CACHE_LIMIT {
                    cache.clear();
                }
                cache.insert(
                    repo_root.to_path_buf(),
                    CachedCanonicalRoot {
                        canonical: canonical.clone(),
                        identity,
                    },
                );
            }
        }

        Ok(canonical)
    }

    #[cfg(not(unix))]
    {
        repo_root.canonicalize()
    }
}

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
    let Ok(canonical_root) = canonical_repo_root(repo_root) else {
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

    #[test]
    #[cfg(unix)]
    fn canonical_root_cache_does_not_outlive_a_replaced_repository_root() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(repo_root.join("inside.rs"), "fn inside() {}").unwrap();

        // Warm the canonical root cache for the original directory.
        assert!(verify_path_within_repo(&repo_root, "inside.rs").is_ok());

        // Replace the root with a symlink onto another directory, so the cached
        // identity no longer describes what the path resolves to.
        std::fs::rename(&repo_root, dir.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &repo_root).unwrap();

        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "super secret").unwrap();
        std::os::unix::fs::symlink(&secret, outside.path().join("link.rs")).unwrap();

        // The refreshed canonical root is the outside directory itself, so a
        // symlink resolving inside it stays inside the boundary. A stale cached
        // root would compare against the previous location and reject it.
        assert!(verify_path_within_repo(&repo_root, "link.rs").is_ok());
    }

    #[test]
    fn test_repeated_verification_is_stable() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/lib.rs"), "pub fn ok() {}").unwrap();

        // Repeated verification against the cached root keeps the same answer.
        for _ in 0..3 {
            assert!(verify_path_within_repo(&repo_root, "src/lib.rs").is_ok());
        }
        assert!(verify_path_within_repo(&repo_root, "../escape.rs").is_err());
    }
}
