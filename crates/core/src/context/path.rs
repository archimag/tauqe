use anyhow::Result;

use super::ContextManager;

/// Boundary checks and repository path normalization are deterministic substrate
/// and live in `tauqe-tools` (`tauqe_tools::paths`), beside every other pure path
/// predicate. The re-exports keep `crate::context::path::{verify_path_within_repo,
/// verify_path_within_repo_for_write}` resolvable for the context, edit, and
/// research layers while the transition lasts.
pub use tauqe_tools::paths::{verify_path_within_repo, verify_path_within_repo_for_write};

impl ContextManager {
    pub fn normalize_path(&self, p: &str) -> Result<String> {
        tauqe_tools::paths::normalize_repo_path(&self.repo_root, p)
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
    /// A root that cannot be canonicalized must not silently disable the
    /// boundary: the read-side check stays permissive for inspection but leaves
    /// an observable record naming the root and the failure.
    #[test]
    fn canonicalization_failure_is_reported_for_read_only_verification() {
        let dir = tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");

        let (buffer, _guard) = crate::blocking::log_capture::install();
        assert!(verify_path_within_repo(&missing_root, "src/lib.rs").is_ok());

        let logged = crate::blocking::log_capture::contents(&buffer);
        assert!(
            logged.contains("the symlink escape boundary cannot be inspected"),
            "the degradation must be observable: {logged}"
        );
        assert!(
            logged.contains(&missing_root.display().to_string()),
            "the record must name the repository root: {logged}"
        );
    }

    /// The same failure fails closed for write admission: a path about to be
    /// written is refused while the boundary cannot be inspected, and the
    /// identical path is admitted once the root resolves again.
    #[test]
    fn write_admission_fails_closed_while_the_root_cannot_be_canonicalized() {
        let dir = tempdir().unwrap();
        let missing_root = dir.path().join("no-such-repository");

        let error = verify_path_within_repo_for_write(&missing_root, "lib.rs")
            .expect_err("a write path is refused while the boundary is uninspectable");
        assert!(
            error.to_string().contains("cannot be verified for writing"),
            "unexpected error: {error}"
        );

        std::fs::create_dir_all(&missing_root).unwrap();
        std::fs::write(missing_root.join("lib.rs"), "pub fn ok() {}").unwrap();
        assert!(verify_path_within_repo_for_write(&missing_root, "lib.rs").is_ok());
    }
}
