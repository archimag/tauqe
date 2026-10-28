//! The `.tauqe/mockups/` sandbox for throwaway prototypes and drafts.
//!
//! A mockup is a sketch such as a prototype module, a scratch design, or a
//! generated sample. It exists to be written, read, discussed, and discarded,
//! so it is not part of the project: committing it would burden the repository
//! with an artifact nobody maintains.
//!
//! The sandbox gives such sketches a place to live without relaxing an
//! invariant the harness enforces. It sits inside `.tauqe/`, which the harness
//! already keeps out of Git, so a mockup never reaches a checkpoint commit, a
//! step commit, or the final commit of a turn, and never joins the change set a
//! turn applies transactionally. Virtual staging admits a write here only while
//! Git really ignores the path, because a sandbox file Git tracked would join
//! the very change set it was meant to stay out of.
//!
//! The module owns the sandbox path and its predicate, so every consumer names
//! the sandbox through one declaration instead of spelling the path itself.

use std::path::Path;

/// Repository-relative directory holding throwaway mockups.
pub const MOCKUPS_DIR: &str = ".tauqe/mockups";

/// Reports whether `path` names the sandbox itself or anything beneath it.
///
/// The comparison runs over whole path components, so a sibling whose name
/// merely starts with the same prefix (`.tauqe/mockups-archive/`) is not a
/// sandbox path, while the directory itself and every descendant are. `path`
/// arrives in the normalized repository-relative form produced by
/// `ContextManager::normalize_path`, which carries no `.` or `..` component.
pub fn is_mockups_path(path: &str) -> bool {
    Path::new(path).starts_with(MOCKUPS_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_covers_itself_and_every_descendant() {
        assert!(is_mockups_path(MOCKUPS_DIR));
        assert!(is_mockups_path(".tauqe/mockups/prototype.rs"));
        assert!(is_mockups_path(".tauqe/mockups/nested/deep/design.md"));
    }

    #[test]
    fn a_sibling_sharing_the_prefix_is_not_the_sandbox() {
        assert!(!is_mockups_path(".tauqe/mockups-archive/old.rs"));
        assert!(!is_mockups_path(".tauqe/mockups.rs"));
        assert!(!is_mockups_path(".tauqe/refs/refs.md"));
        assert!(!is_mockups_path("crates/core/src/mockups.rs"));
        assert!(!is_mockups_path(""));
    }
}
