//! Local preview of the review path pattern.
//!
//! The review launch dialog narrows the audited scope with path patterns that
//! the server resolves against the repository file list. The dialog has to know
//! that answer before it sends anything, so this module derives the selectable
//! directories and counts how many files the typed patterns select together.
//!
//! All helpers are pure: they read the repository file list that already
//! arrived with the handshake and never touch disk, Git, or the network. The
//! matcher is `tauqe_protocol::glob::GlobPattern`, the same type the server
//! applies, so a preview can never predict a selection the server resolves
//! differently.

use std::collections::BTreeSet;

use tauqe_protocol::glob::GlobPattern;

/// Path separator, and the terminator that turns a pattern into a directory
/// prefix selecting every path beneath it rather than the directory entry.
const SEPARATOR: char = '/';

/// Lists the repository directories the picker can offer, sorted and unique.
///
/// Every entry ends with `/`, because that is the pattern form the picker
/// inserts into the field and the form the matcher resolves as "every path
/// beneath this directory". Producing the exact text keeps insertion and
/// matching in one place, so the picker cannot build a prefix the counter
/// evaluates differently.
///
/// A directory is derived from the parent segments of each file path, so the
/// list is ordered by `BTreeSet` independently of the order the repository
/// files arrived in.
pub fn collect_directories(repo_files: &[String]) -> Vec<String> {
    let mut directories: BTreeSet<String> = BTreeSet::new();

    for file in repo_files {
        let segments: Vec<&str> = file
            .split(SEPARATOR)
            .filter(|segment| !segment.is_empty() && *segment != ".")
            .collect();

        // The last segment names the file itself; every prefix of the
        // remaining segments is a directory that contains it.
        let parent_count = segments.len().saturating_sub(1);
        let mut prefix = String::new();
        for segment in segments.iter().take(parent_count) {
            prefix.push_str(segment);
            prefix.push(SEPARATOR);
            directories.insert(prefix.clone());
        }
    }

    directories.into_iter().collect()
}

/// Counts the repository files selected by at least one of `patterns`.
///
/// The patterns form an alternation: a file is counted once no matter how many
/// lines select it, and blank lines are ignored because such a line narrows
/// nothing. Returns `None` when no usable pattern remains — every line is blank
/// or none was typed — so the audit covers the whole context. Any other list
/// yields `Some(count)`, including `Some(0)` when the entered patterns select
/// nothing: the honest answer that lets the caller refuse the launch.
pub fn count_multi_matches(patterns: &[String], repo_files: &[String]) -> Option<usize> {
    let compiled: Vec<GlobPattern> = patterns
        .iter()
        .filter(|pattern| !pattern.trim().is_empty())
        .map(|pattern| GlobPattern::new(pattern))
        .collect();

    if compiled.is_empty() {
        return None;
    }

    let matches = repo_files
        .iter()
        .filter(|file| {
            compiled
                .iter()
                .any(|pattern| pattern.matches(file.as_str()))
        })
        .count();

    Some(matches)
}

/// Counts the repository files the single pattern selects.
///
/// Returns `None` when the pattern is empty or whitespace: such a pattern
/// narrows nothing, and reporting it as zero matches would make the dialog
/// block a launch that imposes no restriction. Any other pattern yields
/// `Some(count)`, including `Some(0)` for a pattern that matches nothing — the
/// honest answer that lets the caller refuse the launch.
pub fn count_matches(pattern: &str, repo_files: &[String]) -> Option<usize> {
    let patterns = [pattern.to_string()];
    count_multi_matches(&patterns, repo_files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    fn patterns(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn test_collect_directories_yields_unique_sorted_prefixes() {
        let files = repo_files(&[
            "src/lib.rs",
            "src/a/b.rs",
            "crates/tui/src/main.rs",
            "README.md",
        ]);

        assert_eq!(
            collect_directories(&files),
            vec![
                "crates/".to_string(),
                "crates/tui/".to_string(),
                "crates/tui/src/".to_string(),
                "src/".to_string(),
                "src/a/".to_string(),
            ]
        );
    }

    #[test]
    fn test_collect_directories_terminates_every_entry_with_a_slash() {
        let files = repo_files(&["src/lib.rs", "crates/tui/src/main.rs"]);

        for directory in collect_directories(&files) {
            assert!(
                directory.ends_with('/'),
                "directory {directory:?} must end with a slash"
            );
        }
    }

    #[test]
    fn test_collected_directory_selects_files_beneath_it() {
        let files = repo_files(&[
            "src/lib.rs",
            "src/a/b.rs",
            "crates/tui/src/main.rs",
            "README.md",
        ]);

        for directory in collect_directories(&files) {
            let count = count_matches(&directory, &files).unwrap_or_default();
            assert!(
                count > 0,
                "directory {directory:?} must select at least one file"
            );
        }
    }

    #[test]
    fn test_wildcard_without_slash_matches_basename_at_any_depth() {
        let files = repo_files(&["src/lib.rs", "crates/tui/src/main.rs", "README.md"]);

        assert_eq!(count_matches("*.rs", &files), Some(2));
    }

    #[test]
    fn test_empty_file_list_has_no_directories_and_no_matches() {
        let files: Vec<String> = Vec::new();

        assert!(collect_directories(&files).is_empty());
        assert_eq!(count_matches("*.rs", &files), Some(0));
    }

    #[test]
    fn test_blank_pattern_narrows_nothing() {
        let files = repo_files(&["src/lib.rs"]);

        assert_eq!(count_matches("", &files), None);
        assert_eq!(count_matches("   ", &files), None);
        assert_eq!(count_matches("\t\n", &files), None);
    }

    #[test]
    fn test_count_multi_matches_counts_a_file_once_across_overlapping_patterns() {
        let files = repo_files(&["src/lib.rs", "src/main.rs", "docs/guide.md"]);

        // `src/` and `*.rs` select the same two source files, so the union
        // stays at two instead of summing the selections.
        assert_eq!(
            count_multi_matches(&patterns(&["src/", "*.rs"]), &files),
            Some(2)
        );
        assert_eq!(
            count_multi_matches(&patterns(&["src/", "*.md"]), &files),
            Some(3)
        );
    }

    #[test]
    fn test_count_multi_matches_ignores_blank_lines() {
        let files = repo_files(&["src/lib.rs", "src/main.rs"]);

        assert_eq!(
            count_multi_matches(&patterns(&["", "  ", "*.rs"]), &files),
            Some(2)
        );
    }

    #[test]
    fn test_count_multi_matches_without_a_usable_pattern_has_no_count() {
        let files = repo_files(&["src/lib.rs"]);

        assert_eq!(count_multi_matches(&[], &files), None);
        assert_eq!(
            count_multi_matches(&patterns(&["", "   ", "\t\n"]), &files),
            None
        );
    }

    #[test]
    fn test_count_multi_matches_reports_zero_when_the_union_selects_nothing() {
        let files = repo_files(&["src/lib.rs", "README.md"]);

        assert_eq!(
            count_multi_matches(&patterns(&["*.py", "docs/"]), &files),
            Some(0)
        );
    }

    #[test]
    fn test_count_multi_matches_over_an_empty_repository_selects_nothing() {
        let files: Vec<String> = Vec::new();

        assert_eq!(count_multi_matches(&patterns(&["*.rs"]), &files), Some(0));
    }

    #[test]
    fn test_single_entry_list_agrees_with_the_single_pattern_counter() {
        let files = repo_files(&["src/lib.rs", "crates/tui/src/main.rs", "README.md"]);

        for pattern in ["src/", "*.rs", "src/lib.rs", "/absolute/", ""] {
            assert_eq!(
                count_multi_matches(&patterns(&[pattern]), &files),
                count_matches(pattern, &files),
                "pattern {pattern:?}"
            );
        }
    }

    #[test]
    fn test_absolute_path_matches_no_relative_file() {
        let files = repo_files(&["src/lib.rs", "src/main.rs"]);

        assert_eq!(count_matches("/home/dev/repo/src/", &files), Some(0));
        assert_eq!(count_matches("/home/dev/repo/src/*.rs", &files), Some(0));
    }
}
