//! Trust boundaries enforced during virtual staging.
//!
//! A repository declares the paths a turn must never modify in
//! `[project.boundaries.protected]` (`tauqe_protocol::ProjectBoundaries`). Such a
//! path stays tracked by Git and readable as a reference, yet a turn cannot
//! create, rewrite, move, or delete it: the task manifest, the specifications,
//! and the generated sources are exactly the artifacts a change set is verified
//! against, so a turn able to rewrite them could also make verification agree
//! with itself.
//!
//! Enforcement belongs to virtual staging, before any byte reaches disk and
//! before the toolchain starts. A rejected operation therefore travels through
//! the ordinary patch-retry path as a typed
//! [`crate::edits::EditError::ProtectedPath`], in the same pass that validates
//! every other edit, instead of surfacing as a Git failure after the whole
//! change set was applied.
//!
//! The declaration is compiled once per change set, and a repository that
//! declares no boundary keeps every path editable.

use std::path::Path;

use tauqe_protocol::{GlobPattern, ProjectConfig};

/// Compiles the patterns declared by `[project.boundaries.protected]`.
///
/// The section is read once per staging call, so a change set touching many
/// files still costs a single configuration read. Reading goes through
/// [`crate::config::load_section`], the single section reader of the harness, so
/// the declaration is decoded exactly like the rest of the configuration. A file
/// that cannot be read or parsed degrades to the default section there, and the
/// configuration loader reports the reason through `AppConfig::config_error`,
/// which keeps a broken boundary declaration from blocking an otherwise valid
/// turn.
///
/// A blank entry would compile into a matcher that matches nothing, so the
/// boundary it was meant to declare would silently disappear; such an entry is
/// reported and skipped instead.
pub(crate) fn protected_patterns(repo_root: &Path) -> Vec<GlobPattern> {
    let project: ProjectConfig = crate::config::load_section(repo_root, "project");
    project
        .boundaries
        .protected
        .iter()
        .filter(|pattern| {
            let usable = !pattern.trim().is_empty();
            if !usable {
                tracing::warn!(
                    "the [project.boundaries.protected] declaration holds a blank pattern"
                );
            }
            usable
        })
        .map(|pattern| GlobPattern::new(pattern))
        .collect()
}

/// Reports whether `path` matches one of the declared protected patterns.
///
/// Both inputs are already normalized: [`protected_patterns`] compiles the
/// declaration, and callers pass the repository-relative path produced by
/// `ContextManager::normalize_path`. [`GlobPattern::matches`] normalizes its own
/// argument as well, so a pattern written with a trailing separator still
/// compares correctly against a path that carries none.
pub(crate) fn is_protected(patterns: &[GlobPattern], path: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Declares the protected patterns of the repository under test.
    fn declare_protected_patterns(root: &Path, patterns: &[&str]) {
        let declared = patterns
            .iter()
            .map(|pattern| format!("\"{pattern}\""))
            .collect::<Vec<String>>()
            .join(", ");
        std::fs::write(
            root.join("tauqe.toml"),
            format!("[project.boundaries]\nprotected = [{declared}]\n"),
        )
        .unwrap();
    }

    #[test]
    fn declared_patterns_compile_into_one_matcher_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        declare_protected_patterns(dir.path(), &["Justfile", "docs/specs/**"]);

        let patterns = protected_patterns(dir.path());
        assert_eq!(patterns.len(), 2);
        assert!(is_protected(&patterns, "Justfile"));
        assert!(is_protected(&patterns, "docs/specs/auth.md"));
        assert!(is_protected(&patterns, "docs/specs/nested/deep.md"));
        assert!(!is_protected(&patterns, "Justfile.bak"));
        assert!(!is_protected(&patterns, "docs/specs.md"));
        assert!(!is_protected(&patterns, "src/lib.rs"));
    }

    #[test]
    fn a_declaration_without_a_boundary_subsection_protects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[project.verify]\ncommand = \"cargo\"\nargs = [\"test\"]\n",
        )
        .unwrap();

        assert!(protected_patterns(dir.path()).is_empty());
        assert!(!is_protected(&[], "Justfile"));
        assert!(!is_protected(&[], "docs/specs/auth.md"));
    }
}
