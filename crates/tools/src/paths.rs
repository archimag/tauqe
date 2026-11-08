//! Deterministic path predicates shared by the substrate.
//!
//! Two questions about a repository-relative path are answered here once, so
//! every subsystem that asks them agrees on the answer:
//!
//! - whether the path belongs to a service directory that no scan, outline, or
//!   search may traverse ([`is_excluded_path`]);
//! - whether the path resolves, through any symbolic link along its components,
//!   outside the repository boundary ([`verify_path_within_repo`]).
//!
//! The boundary check inspects every component with `symlink_metadata` (lstat)
//! instead of following it, so a link that leaves the repository is detected
//! even when its target does not exist yet.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

/// Determines whether a repository relative path belongs to an excluded directory.
pub fn is_excluded_path(path: &str) -> bool {
    let p = Path::new(path);
    for component in p.components() {
        if let std::path::Component::Normal(name) = component {
            if let Some(s) = name.to_str() {
                if matches!(s, ".git" | ".tauqe" | "target" | "node_modules") {
                    return true;
                }
            }
        }
    }
    false
}

/// Normalizes a repository-relative path, rejecting an escape.
///
/// The normalized form collapses `.` components, resolves `..` without leaving the
/// repository, rejects an absolute path, and verifies the result against the
/// boundary, so every later consumer addresses one canonical spelling of the path.
/// The boundary check is the read-side one: normalization inspects a path but
/// writes nothing.
pub fn normalize_repo_path(repo_root: &Path, relative_path: &str) -> Result<String> {
    let path = Path::new(relative_path);
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::Normal(c) => components.push(c.to_string_lossy().to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if components.pop().is_none() {
                    bail!("Path escapes repository root: {}", relative_path);
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("Absolute paths are not allowed: {}", relative_path);
            }
        }
    }

    if components.is_empty() {
        bail!("Empty path is not allowed");
    }

    let clean = components.join("/");
    verify_path_within_repo(repo_root, &clean)?;
    Ok(clean)
}
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
/// each symlink it meets. Verification runs for every tracked path that enters
/// the context and for every pinned candidate, so resolving the root once per
/// repository — kept honest by one cheap `stat` — removes the dominant syscall
/// cost of the boundary checks.
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

/// Resolves the canonical repository root both boundary checks compare against.
///
/// Sharing the resolution keeps the two entry points in agreement about whether
/// the boundary is inspectable at all, and records the degradation exactly once
/// per check.
fn boundary_canonical_root(repo_root: &Path) -> Option<PathBuf> {
    match canonical_repo_root(repo_root) {
        Ok(canonical) => Some(canonical),
        Err(err) => {
            tracing::warn!(
                repo_root = %repo_root.display(),
                error = %err,
                "repository root cannot be canonicalized; \
                 the symlink escape boundary cannot be inspected"
            );
            None
        }
    }
}

/// Verifies that `relative_path` does not resolve to a symlink outside
/// `repo_root`, for read-only inspection and for paths already in context.
///
/// Every component of the relative path is inspected from the repository root
/// downward with `symlink_metadata` (lstat), which detects a symlink even when
/// its target does not exist yet. Each symlink target is read explicitly with
/// `read_link` and resolved to its deepest existing ancestor, so a link that
/// dangles outside the repository is rejected instead of being silently
/// traversed while missing parent directories are created.
///
/// Degradation contract: when `repo_root` cannot be canonicalized — it was
/// removed, renamed, or made inaccessible, or a parent directory denies search
/// permission — the boundary cannot be inspected at all. The read-side check
/// then reports success and records the degradation through the semantic
/// journal, because accepting a path here writes nothing and a blanket refusal
/// would break read-only inspection and the synthetic repositories of unit
/// tests. Every write admission calls [`verify_path_within_repo_for_write`]
/// instead, which fails closed on the same failure.
pub fn verify_path_within_repo(repo_root: &Path, relative_path: &str) -> Result<()> {
    match boundary_canonical_root(repo_root) {
        Some(canonical_root) => verify_path_components(repo_root, relative_path, &canonical_root),
        None => Ok(()),
    }
}

/// Verifies `relative_path` for write admission, failing closed when the
/// repository root cannot be inspected.
///
/// The escape check itself is identical to [`verify_path_within_repo`], but a
/// path that is about to be written is admitted only while the boundary is
/// actually inspectable. A write the check should have refused cannot be undone
/// afterwards, so an uncanonicalizable root is reported as an error instead of
/// being accepted.
pub fn verify_path_within_repo_for_write(repo_root: &Path, relative_path: &str) -> Result<()> {
    match boundary_canonical_root(repo_root) {
        Some(canonical_root) => verify_path_components(repo_root, relative_path, &canonical_root),
        None => bail!(
            "Path '{}' cannot be verified for writing: repository root '{}' \
             cannot be canonicalized",
            relative_path,
            repo_root.display()
        ),
    }
}

/// Reports the ancestor of `relative_path` that blocks its creation.
///
/// [`verify_path_within_repo_for_write`] answers whether the destination is
/// reachable, not whether its missing suffix can be created: the walk stops at
/// the first component it cannot inspect and treats the remainder as absent,
/// which holds for a directory that does not exist yet but not for one that
/// already exists as a regular file. A destination such as `src/new/a.rs`
/// beneath a regular file `src/new` therefore passes the boundary check and
/// fails only when the disk commit creates its staging sibling, with `ENOTDIR`
/// and an opaque filesystem error instead of a typed, per-member refusal.
///
/// Only the strict ancestors of the destination are asked, because the
/// destination itself is the file an overwrite or a replacement addresses
/// legitimately. A symbolic link is followed, since a link to a directory is a
/// usable parent and the boundary check has already proved that it resolves
/// inside the repository. A component that cannot be inspected proves nothing
/// and ends the walk, exactly as the boundary check degrades.
///
/// The answer is the repository-relative spelling of the blocking component.
pub fn blocking_ancestor(repo_root: &Path, relative_path: &str) -> Option<String> {
    let components: Vec<_> = Path::new(relative_path).components().collect();
    let (_, ancestors) = components.split_last()?;
    let mut current = PathBuf::from(repo_root);
    let mut names: Vec<String> = Vec::with_capacity(ancestors.len());
    for component in ancestors {
        // Normalized repository paths carry normal components only, so any
        // other kind means an unnormalized path slipped through and the walk
        // has no answer to give.
        let std::path::Component::Normal(name) = component else {
            return None;
        };
        names.push(name.to_string_lossy().into_owned());
        current.push(name);
        match std::fs::metadata(&current) {
            // A directory, or a symbolic link to one, is a usable parent.
            Ok(metadata) if metadata.is_dir() => {}
            // The component exists and is a file, so nothing beneath it can
            // ever be created.
            Ok(_) => return Some(names.join("/")),
            // A missing component leaves the rest of the chain equally absent,
            // and one that cannot be inspected proves nothing.
            Err(_) => return None,
        }
    }
    None
}

/// Walks every component of `relative_path` and rejects a symlink that resolves
/// outside `canonical_root`.
fn verify_path_components(
    repo_root: &Path,
    relative_path: &str,
    canonical_root: &Path,
) -> Result<()> {
    let full_path = repo_root.join(relative_path);

    let Ok(relative) = full_path.strip_prefix(repo_root) else {
        bail!("Path '{}' escapes repository root", relative_path);
    };

    let mut current = PathBuf::from(repo_root);
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => current.push(name),
            // Normalized repository paths collapse `.`/`..` and reject absolute
            // paths, so any other component here means an unnormalized path
            // slipped through; treat it as escaping.
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
        if !resolved.starts_with(canonical_root) {
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

#[cfg(test)]
mod tests {
    use super::{blocking_ancestor, is_excluded_path};

    #[test]
    fn rejects_service_directories_at_any_depth() {
        assert!(is_excluded_path(".git/HEAD"));
        assert!(is_excluded_path(".tauqe/server.log"));
        assert!(is_excluded_path("crates/core/target/debug/app"));
        assert!(is_excluded_path("node_modules/pkg/index.js"));
        assert!(!is_excluded_path("crates/core/src/lib.rs"));
        assert!(!is_excluded_path("docs/target-design.md"));
    }
    #[test]
    fn only_a_strict_ancestor_that_is_not_a_directory_blocks_a_destination() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("file"), b"content").unwrap();
        std::fs::create_dir(root.join("dir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("dir", root.join("link")).unwrap();

        // The destination itself is the file a write addresses legitimately, so
        // it never blocks its own creation.
        assert_eq!(blocking_ancestor(root, "file"), None);
        // A directory, an absent chain, and a link to a directory are usable
        // parents.
        assert_eq!(blocking_ancestor(root, "dir/child.rs"), None);
        assert_eq!(
            blocking_ancestor(root, "dir/missing/child.rs"),
            None,
            "a chain that stops at a missing component proves nothing"
        );
        #[cfg(unix)]
        assert_eq!(blocking_ancestor(root, "link/child.rs"), None);
        // A component that exists as a file blocks everything beneath it.
        assert_eq!(
            blocking_ancestor(root, "file/child.rs").as_deref(),
            Some("file")
        );
        assert_eq!(
            blocking_ancestor(root, "dir/file/child.rs"),
            None,
            "the walk stops at the first component that does not exist"
        );
    }
}
