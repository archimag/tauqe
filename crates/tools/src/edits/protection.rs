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
//! The declaration is compiled once per change set from the effective
//! `ProjectConfig` a caller supplies. Resolving `tauqe.toml` into that value
//! belongs to the crate that owns the harness configuration, so this module
//! reads no configuration file of its own. A repository that declares no
//! boundary keeps every path editable. A directory move names two directories
//! but relocates a whole subtree, so the boundary is asked about every path the
//! operation really changes rather than about the two names it was written with.

use tauqe_protocol::{GlobPattern, ProjectConfig};

/// Compiles the patterns declared by `[project.boundaries.protected]`.
///
/// The declaration is read once per staging call by the caller, so a change set
/// touching many files still costs a single configuration read. A blank entry
/// would compile into a matcher that matches nothing, so the boundary it was
/// meant to declare would silently disappear; such an entry is reported and
/// skipped instead.
pub fn protected_patterns(project: &ProjectConfig) -> Vec<GlobPattern> {
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
/// [`crate::paths::normalize_repo_path`] once it exists, or by the context
/// normalization of the caller. [`GlobPattern::matches`] normalizes its own
/// argument as well, so a pattern written with a trailing separator still
/// compares correctly against a path that carries none.
pub fn is_protected(patterns: &[GlobPattern], path: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(path))
}

/// Reports the first path a directory move would touch that lies on a declared
/// boundary.
///
/// A directory move names two directories and relocates the tracked files
/// between them, so the boundary must be asked about every path the operation
/// really changes. The directory names cover a boundary declared on a whole
/// subtree (`vendor/generated`), while the expanded file pairs cover a boundary
/// declared on its contents (`docs/specs/**`). Asking for both directions at
/// once keeps a move from carrying a protected file out of the directory that
/// guards it or dropping one into it, and the first offending path is reported
/// so the retry names the path the model actually has to leave alone.
pub fn protected_dir_move_path<'a>(
    patterns: &[GlobPattern],
    directories: [&'a str; 2],
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<&'a str> {
    directories
        .into_iter()
        .chain(files.into_iter().flat_map(|(from, to)| [from, to]))
        .find(|path| is_protected(patterns, path))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Declares the protected patterns of the project under test.
    fn declared(patterns: &[&str]) -> ProjectConfig {
        let declared = patterns
            .iter()
            .map(|pattern| format!("\"{pattern}\""))
            .collect::<Vec<String>>()
            .join(", ");
        serde_json::from_str(&format!(
            "{{\"boundaries\": {{\"protected\": [{declared}]}}}}"
        ))
        .expect("the boundary declaration parses")
    }

    #[test]
    fn declared_patterns_compile_into_one_matcher_per_entry() {
        let patterns = protected_patterns(&declared(&["Justfile", "docs/specs/**"]));
        assert_eq!(patterns.len(), 2);
        assert!(is_protected(&patterns, "Justfile"));
        assert!(is_protected(&patterns, "docs/specs/auth.md"));
        assert!(is_protected(&patterns, "docs/specs/nested/deep.md"));
        assert!(!is_protected(&patterns, "Justfile.bak"));
        assert!(!is_protected(&patterns, "docs/specs.md"));
        assert!(!is_protected(&patterns, "src/lib.rs"));
    }

    #[test]
    fn a_blank_entry_is_skipped_instead_of_matching_nothing() {
        // A blank pattern would compile into a matcher that never matches, so
        // the boundary it was meant to declare would silently disappear. The
        // entry is reported and skipped instead.
        let patterns = protected_patterns(&declared(&["Justfile", "   "]));
        assert_eq!(patterns.len(), 1);
        assert!(is_protected(&patterns, "Justfile"));
    }

    #[test]
    fn a_declaration_without_a_boundary_subsection_protects_nothing() {
        let project: ProjectConfig =
            serde_json::from_str("{\"verify\":{\"command\":\"cargo\"}}").unwrap();
        assert!(protected_patterns(&project).is_empty());
        assert!(protected_patterns(&ProjectConfig::default()).is_empty());
        assert!(!is_protected(&[], "Justfile"));
        assert!(!is_protected(&[], "docs/specs/auth.md"));
    }

    #[test]
    fn a_directory_move_is_checked_on_both_sides_of_every_file() {
        let patterns = protected_patterns(&declared(&["vendor/generated", "docs/specs/**"]));

        // A boundary declared on a directory itself is caught through the two
        // directory names, before any file of the subtree is considered.
        assert_eq!(
            protected_dir_move_path(
                &patterns,
                ["vendor/generated", "vendor/copied"],
                [("vendor/generated/a.rs", "vendor/copied/a.rs")],
            ),
            Some("vendor/generated")
        );

        // A boundary declared on the contents of a subtree is caught through
        // the source side of the expanded file pair.
        assert_eq!(
            protected_dir_move_path(
                &patterns,
                ["src/old", "src/new"],
                [("docs/specs/auth.md", "src/new/auth.md")],
            ),
            Some("docs/specs/auth.md")
        );

        // The destination side is asked the same question, so a move cannot
        // drop an unprotected file into a guarded subtree either.
        assert_eq!(
            protected_dir_move_path(
                &patterns,
                ["src/old", "src/new"],
                [("src/old/auth.md", "docs/specs/auth.md")],
            ),
            Some("docs/specs/auth.md")
        );

        // A move that stays outside every declared boundary passes.
        assert_eq!(
            protected_dir_move_path(
                &patterns,
                ["src/old", "src/new"],
                [("src/old/a.rs", "src/new/a.rs")],
            ),
            None
        );
    }
}
