//! Compact directory-tree outline of the tracked repository files.
//!
//! When the eager global repository map is disabled, the model still needs to
//! know where subsystems live. This module renders a deterministic folder tree
//! from the tracked file list without listing every file, keeping the prompt
//! small while preserving navigation.
//!
//! Two categories of files are too important to hide behind folder names alone.
//! Repository root files (build manifests, project configuration, readmes) are
//! listed explicitly, because a model that cannot see `Cargo.toml` or
//! `tauqe.toml` cannot reason about the project at all. Files anchored by the
//! pinned context layer are marked with a `[pinned]` suffix, so the model knows
//! that authoritative conventions already travelled with the prompt instead of
//! searching for them again.

use std::collections::{BTreeMap, BTreeSet};

use crate::workflow::discovery::search::is_excluded_path;

/// One folder in the tree, holding ordered child folders and pinned files.
#[derive(Debug, Default)]
struct DirNode {
    children: BTreeMap<String, DirNode>,
    /// File names rendered inside this folder: pinned files only.
    files: BTreeSet<String>,
}

/// A single rendered outline entry resolved by the depth-first walk.
enum OutlineEntry<'a> {
    Dir {
        name: &'a str,
        node: &'a DirNode,
        depth: usize,
    },
    File {
        name: &'a str,
        depth: usize,
        pinned: bool,
    },
}

/// Builds an indented tree of the tracked folders and repository root files.
///
/// Only folders, root files, and pinned files are rendered. The output ends
/// with a newline, and an empty input yields an empty string.
pub fn generate_repo_dirs(available_files: &[String]) -> String {
    generate_repo_dirs_with_pinned(available_files, &[])
}

/// Builds the tree and marks every file matching a pinned pattern.
///
/// Root files are rendered first in alphabetical order, then the folder tree.
/// Each folder occupies its own line, indented by two spaces per nesting level,
/// and appears directly after its parent. Sibling folders are sorted
/// alphabetically, so identical input always yields identical output regardless
/// of file order. A pinned file inside a folder is rendered as an extra child
/// line; an unpinned nested file stays hidden, so the outline never degenerates
/// into a full file listing. Paths carrying service directories (`.git`,
/// `.tauqe`, `target`, `node_modules`) are dropped, on the root level as well.
pub fn generate_repo_dirs_with_pinned(
    available_files: &[String],
    pinned_patterns: &[String],
) -> String {
    let patterns: Vec<&str> = pinned_patterns
        .iter()
        .map(|pattern| normalize_pinned_pattern(pattern))
        .filter(|pattern| !pattern.is_empty())
        .collect();

    let mut root = DirNode::default();
    let mut root_files: BTreeSet<String> = BTreeSet::new();

    for file in available_files {
        if is_excluded_path(file) {
            continue;
        }
        let parts = split_components(file);
        let Some((file_name, dirs)) = parts.split_last() else {
            continue;
        };
        let file_name = *file_name;
        if dirs.is_empty() {
            root_files.insert(file_name.to_string());
            continue;
        }
        let pinned = matches_pinned_pattern(file.as_str(), &patterns);
        if pinned {
            insert_path(&mut root, dirs)
                .files
                .insert(file_name.to_string());
        } else {
            insert_path(&mut root, dirs);
        }
    }

    let mut out = String::new();
    render_outline(&root, &root_files, &patterns, &mut out);
    out
}

/// Splits a repository-relative path into its components.
///
/// Empty and `.` components are skipped, so `./a/./b/c.rs` yields
/// `["a", "b", "c.rs"]`. The final component is the file name, the preceding
/// ones are the folders leading to it.
fn split_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

/// Inserts one folder path into the tree, creating missing ancestor folders, and
/// returns the deepest folder so a pinned file can be recorded there.
fn insert_path<'a>(root: &'a mut DirNode, components: &[&str]) -> &'a mut DirNode {
    let mut current = root;
    for name in components {
        current = current.children.entry((*name).to_string()).or_default();
    }
    current
}

/// Normalizes one pinned pattern into the repository-relative form used for
/// matching, dropping the cosmetic `./` and leading `/` markers.
fn normalize_pinned_pattern(pattern: &str) -> &str {
    pattern
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
}

/// Reports whether a repository-relative path matches one pinned pattern.
///
/// A pattern is matched at the repository root and, because the pinned context
/// layer resolves patterns hierarchically against every ancestor directory, at
/// every nesting level: `Conventions.md` matches both `Conventions.md` and
/// `crates/core/Conventions.md`, while `docs/src/SUMMARY.md` matches its own
/// path at any depth. The match is component-wise, so a file that merely shares
/// a textual stem (`MyConventions.md`) never matches.
fn matches_pinned_pattern(path: &str, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|pattern| match path.strip_suffix(pattern) {
            Some(prefix) => prefix.is_empty() || prefix.ends_with('/'),
            None => false,
        })
}

/// Renders the root files and the folder tree depth-first, using an explicit
/// stack so deeply nested hierarchies cannot exhaust the call stack.
fn render_outline(
    root: &DirNode,
    root_files: &BTreeSet<String>,
    patterns: &[&str],
    out: &mut String,
) {
    let mut stack: Vec<OutlineEntry<'_>> = Vec::new();

    // Entries are pushed in reverse display order, so the stack pops them in the
    // order they must appear: root files first, then the folder tree. The stack
    // pops last-in-first-out, so the folder tree is pushed first and the root
    // files are pushed last.
    for (name, node) in root.children.iter().rev() {
        stack.push(OutlineEntry::Dir {
            name: name.as_str(),
            node,
            depth: 0,
        });
    }
    for name in root_files.iter().rev() {
        stack.push(OutlineEntry::File {
            name: name.as_str(),
            depth: 0,
            pinned: matches_pinned_pattern(name.as_str(), patterns),
        });
    }

    while let Some(entry) = stack.pop() {
        match entry {
            OutlineEntry::Dir { name, node, depth } => {
                push_indent(out, depth);
                out.push_str(name);
                out.push('\n');
                // Children are pushed before files, so a folder renders its
                // pinned files first and its subfolders afterwards.
                for (child_name, child_node) in node.children.iter().rev() {
                    stack.push(OutlineEntry::Dir {
                        name: child_name.as_str(),
                        node: child_node,
                        depth: depth + 1,
                    });
                }
                for file in node.files.iter().rev() {
                    stack.push(OutlineEntry::File {
                        name: file.as_str(),
                        depth: depth + 1,
                        pinned: true,
                    });
                }
            }
            OutlineEntry::File {
                name,
                depth,
                pinned,
            } => {
                push_indent(out, depth);
                out.push_str(name);
                if pinned {
                    out.push_str(" [pinned]");
                }
                out.push('\n');
            }
        }
    }
}

/// Writes two spaces of indentation per nesting level.
fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn test_empty_input_yields_empty() {
        assert_eq!(generate_repo_dirs(&[]), "");
        assert_eq!(
            generate_repo_dirs_with_pinned(&[], &owned(&["Conventions.md"])),
            ""
        );
    }

    #[test]
    fn test_root_files_are_listed_alphabetically() {
        let files = owned(&["tauqe.toml", "Cargo.toml", "README.md"]);
        assert_eq!(
            generate_repo_dirs(&files),
            "Cargo.toml\nREADME.md\ntauqe.toml\n"
        );
    }

    #[test]
    fn test_root_files_precede_the_folder_tree() {
        let files = owned(&[
            "src/main.rs",
            "Cargo.toml",
            "crates/core/src/lib.rs",
            "crates/protocol/src/lib.rs",
        ]);
        let expected = "\
Cargo.toml
crates
  core
    src
  protocol
    src
src
";
        assert_eq!(generate_repo_dirs(&files), expected);
    }

    #[test]
    fn test_service_root_entries_are_dropped() {
        let files = owned(&["target", ".git", "Cargo.toml"]);
        assert_eq!(generate_repo_dirs(&files), "Cargo.toml\n");
    }

    #[test]
    fn test_pinned_root_files_are_marked() {
        let files = owned(&["Vision.md", "Conventions.md", "README.md"]);
        let pinned = owned(&["Vision.md", "./Conventions.md"]);
        assert_eq!(
            generate_repo_dirs_with_pinned(&files, &pinned),
            "Conventions.md [pinned]\nREADME.md\nVision.md [pinned]\n"
        );
    }

    #[test]
    fn test_unpinned_nested_files_stay_hidden() {
        let files = owned(&["crates/core/src/lib.rs"]);
        assert_eq!(
            generate_repo_dirs_with_pinned(&files, &owned(&["Conventions.md"])),
            "crates\n  core\n    src\n"
        );
    }

    #[test]
    fn test_pinned_nested_files_are_marked_inside_their_folders() {
        let files = owned(&[
            "crates/core/Conventions.md",
            "crates/core/src/lib.rs",
            "crates/tui/Conventions.md",
        ]);
        let expected = "\
crates
  core
    Conventions.md [pinned]
    src
  tui
    Conventions.md [pinned]
";
        assert_eq!(
            generate_repo_dirs_with_pinned(&files, &owned(&["Conventions.md"])),
            expected
        );
    }

    #[test]
    fn test_pinned_pattern_with_directory_prefix_matches_its_own_path() {
        let files = owned(&["docs/src/SUMMARY.md", "docs/src/ch01.md"]);
        assert_eq!(
            generate_repo_dirs_with_pinned(&files, &owned(&["docs/src/SUMMARY.md"])),
            "docs\n  src\n    SUMMARY.md [pinned]\n"
        );
    }

    #[test]
    fn test_pinned_pattern_requires_a_whole_path_component() {
        let files = owned(&["MyConventions.md", "Conventions.md.bak", "Conventions.md"]);
        assert_eq!(
            generate_repo_dirs_with_pinned(&files, &owned(&["Conventions.md"])),
            "Conventions.md [pinned]\nConventions.md.bak\nMyConventions.md\n"
        );
    }

    #[test]
    fn test_nested_tree_is_indented_and_sorted() {
        let files = owned(&[
            "crates/core/src/lib.rs",
            "crates/core/src/repomap.rs",
            "crates/protocol/src/lib.rs",
            "crates/tui/src/main.rs",
        ]);
        let output = generate_repo_dirs(&files);
        let expected = "\
crates
  core
    src
  protocol
    src
  tui
    src
";
        assert_eq!(output, expected);
    }

    #[test]
    fn test_order_independence_and_determinism() {
        let a = owned(&[
            "Cargo.toml",
            "Conventions.md",
            "a/b/c.rs",
            "a/d/e.rs",
            "x/y.rs",
        ]);
        let mut b = a.clone();
        b.reverse();
        let pinned = owned(&["Conventions.md"]);
        assert_eq!(
            generate_repo_dirs_with_pinned(&a, &pinned),
            generate_repo_dirs_with_pinned(&b, &pinned)
        );
        assert_eq!(
            generate_repo_dirs_with_pinned(&a, &pinned),
            generate_repo_dirs_with_pinned(&a, &pinned)
        );
    }

    #[test]
    fn test_excludes_service_directories() {
        let files = owned(&[
            ".git/HEAD",
            ".tauqe/server.log",
            "target/debug/app",
            "node_modules/pkg/index.js",
            "src/main.rs",
        ]);
        assert_eq!(generate_repo_dirs(&files), "src\n");
    }

    #[test]
    fn test_normalizes_dot_components() {
        let files = owned(&["./a/./b/file.rs"]);
        assert_eq!(generate_repo_dirs(&files), "a\n  b\n");
    }

    #[test]
    fn test_deduplicates_shared_ancestors() {
        let files = owned(&["a/b/x.rs", "a/b/y.rs", "a/c/z.rs"]);
        assert_eq!(generate_repo_dirs(&files), "a\n  b\n  c\n");
    }

    #[test]
    fn test_deep_nesting_does_not_overflow() {
        let depth = 500;
        let mut path = String::new();
        for i in 0..depth {
            path.push_str(&format!("d{}/", i));
        }
        path.push_str("leaf.rs");
        let files = vec![path];

        let output = generate_repo_dirs(&files);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), depth);

        let indent = "  ".repeat(depth - 1);
        assert!(lines[depth - 1].starts_with(indent.as_str()));
        assert_eq!(lines[depth - 1].trim(), format!("d{}", depth - 1).as_str());
    }
}
