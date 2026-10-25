use std::path::Path;

use regex::RegexBuilder;
use tauqe_protocol::RepoSearchRequest;

use crate::blocking::run_blocking;
use crate::context::path::verify_path_within_repo;
use crate::escape::escape_xml_attr;

use super::DiscoveryLimits;

/// Maximum file size eligible for regex scanning (2 MiB).
pub const MAX_SEARCHABLE_FILE_SIZE: u64 = 2 * 1024 * 1024;

/// Compiled regex size limit to prevent ReDoS and excessive memory usage (10 MiB).
pub const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;

/// Compiled regex DFA size limit (10 MiB).
pub const REGEX_DFA_SIZE_LIMIT: usize = 10 * 1024 * 1024;

/// A single matched repository file with its occurrence count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchMatch {
    pub path: String,
    pub count: usize,
}

/// Aggregated outcome of executing a single [`RepoSearchRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSearchResult {
    pub query: RepoSearchRequest,
    pub matches: Vec<FileSearchMatch>,
    pub total_matched_files: usize,
    pub total_matches: usize,
    pub omitted_files: usize,
    pub error: Option<String>,
}

/// Normalizes and validates a requested search path against repository boundaries.
///
/// Empty strings, `"."`, and `"./"` denote the repository root (`""`). Absolute paths,
/// directory traversal components (`..`), NUL bytes, and symlinks escaping the repository
/// are rejected with descriptive diagnostic errors.
pub fn normalize_search_path(repo_root: &Path, raw_path: &str) -> Result<String, String> {
    if raw_path.contains('\0') {
        return Err(format!("Path contains NUL byte: {:?}", raw_path));
    }

    let trimmed = raw_path.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "./" {
        return Ok(String::new());
    }

    let path = Path::new(trimmed);
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(c) => components.push(c.to_string_lossy().to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(format!(
                    "Directory traversal ('..') is not allowed in search path: '{}'",
                    raw_path
                ));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "Absolute paths are not allowed in search path: '{}'",
                    raw_path
                ));
            }
        }
    }

    if components.is_empty() {
        return Ok(String::new());
    }

    let clean = components.join("/");
    if let Err(err) = verify_path_within_repo(repo_root, &clean) {
        return Err(format!("Search path escapes repository root: {}", err));
    }

    Ok(clean)
}

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

/// Checks if a candidate file matches the scope defined by the normalized search path.
fn file_matches_scope(candidate: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    candidate == scope || candidate.starts_with(&format!("{}/", scope))
}

/// Compiles a regex with strict size limits and provides friendly diagnostics on failure.
fn compile_search_regex(pattern: &str) -> Result<regex::Regex, String> {
    RegexBuilder::new(pattern)
        .multi_line(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
        .build()
        .map_err(|err| {
            let mut diag = format!("Regex syntax error: {}", err);
            if pattern.contains("(?=")
                || pattern.contains("(?!")
                || pattern.contains("(?<=")
                || pattern.contains("(?<!")
            {
                diag.push_str(" (Note: Lookaround assertions are not supported in Rust regex)");
            } else {
                diag.push_str(
                    " (Note: Tauqe uses Rust regex; lookahead/lookbehind and backreferences are not supported)",
                );
            }
            diag
        })
}

/// Synchronous internal routine executing search across repository files on disk.
fn execute_repo_search_sync(
    repo_root: &Path,
    query: &RepoSearchRequest,
    available_files: &[String],
    limits: DiscoveryLimits,
) -> RepoSearchResult {
    let normalized_path = match normalize_search_path(repo_root, &query.path) {
        Ok(p) => p,
        Err(err) => {
            return RepoSearchResult {
                query: query.clone(),
                matches: Vec::new(),
                total_matched_files: 0,
                total_matches: 0,
                omitted_files: 0,
                error: Some(err),
            };
        }
    };

    let compiled = match compile_search_regex(&query.regex) {
        Ok(re) => re,
        Err(err) => {
            return RepoSearchResult {
                query: query.clone(),
                matches: Vec::new(),
                total_matched_files: 0,
                total_matches: 0,
                omitted_files: 0,
                error: Some(err),
            };
        }
    };

    let mut matches = Vec::new();
    for candidate in available_files {
        if is_excluded_path(candidate) {
            continue;
        }

        if !file_matches_scope(candidate, &normalized_path) {
            continue;
        }

        let full_path = repo_root.join(candidate);

        if let Ok(meta) = std::fs::symlink_metadata(&full_path) {
            if meta.is_symlink() && verify_path_within_repo(repo_root, candidate).is_err() {
                continue;
            }
        } else {
            continue;
        }

        let file_meta = match std::fs::metadata(&full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if !file_meta.is_file() || file_meta.len() > MAX_SEARCHABLE_FILE_SIZE {
            continue;
        }

        let bytes = match std::fs::read(&full_path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if bytes.contains(&0) {
            continue;
        }

        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let count = compiled.find_iter(text).count();
        if count > 0 {
            matches.push(FileSearchMatch {
                path: candidate.clone(),
                count,
            });
        }
    }

    // Sort deterministically: highest occurrence count first, then alphabetical by path.
    matches.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));

    let total_matched_files = matches.len();
    let total_matches: usize = matches.iter().map(|m| m.count).sum();
    let omitted_files = total_matched_files.saturating_sub(limits.max_matched_files_per_search);

    matches.truncate(limits.max_matched_files_per_search);

    RepoSearchResult {
        query: query.clone(),
        matches,
        total_matched_files,
        total_matches,
        omitted_files,
        error: None,
    }
}

/// Executes a single repository search, offloading blocking file I/O via [`run_blocking`].
///
/// The number of reported matching files is bounded by the project's
/// `[context].max_matched_files_per_search`, so a project tunes how much of a
/// broad match set reaches the model instead of recompiling the engine.
pub fn execute_repo_search(
    repo_root: &Path,
    query: &RepoSearchRequest,
    available_files: &[String],
    limits: DiscoveryLimits,
) -> RepoSearchResult {
    run_blocking(|| execute_repo_search_sync(repo_root, query, available_files, limits))
}

/// Executes multiple repository searches with round limits and offloaded blocking I/O.
pub fn execute_repo_searches(
    repo_root: &Path,
    queries: &[RepoSearchRequest],
    available_files: &[String],
    limits: DiscoveryLimits,
) -> Vec<RepoSearchResult> {
    run_blocking(|| {
        queries
            .iter()
            .enumerate()
            .map(|(idx, query)| {
                if idx >= limits.max_search_requests_per_round {
                    RepoSearchResult {
                        query: query.clone(),
                        matches: Vec::new(),
                        total_matched_files: 0,
                        total_matches: 0,
                        omitted_files: 0,
                        error: Some(format!(
                            "Search query limit exceeded (maximum {} queries per round allowed)",
                            limits.max_search_requests_per_round
                        )),
                    }
                } else {
                    execute_repo_search_sync(repo_root, query, available_files, limits)
                }
            })
            .collect()
    })
}

/// Formats a single [`RepoSearchResult`] into the `<search_feedback>` XML block.
pub fn format_single_search_feedback(result: &RepoSearchResult) -> String {
    let mut out = String::new();
    let path_attr = if result.query.path.is_empty() {
        "."
    } else {
        &result.query.path
    };

    out.push_str(&format!(
        "<search_feedback path=\"{}\" regex=\"{}\">\n",
        escape_xml_attr(path_attr),
        escape_xml_attr(&result.query.regex)
    ));

    if let Some(err) = &result.error {
        out.push_str(&format!("Error: {}\n", err));
    } else if result.matches.is_empty() {
        out.push_str("No matches found.\n");
    } else {
        for m in &result.matches {
            let label = if m.count == 1 { "match" } else { "matches" };
            out.push_str(&format!("{}: {} {}\n", m.path, m.count, label));
        }
        if result.omitted_files > 0 {
            out.push_str(&format!(
                "[{} more matching files omitted]\n",
                result.omitted_files
            ));
        }
    }

    out.push_str("</search_feedback>");
    out
}

/// Formats multiple [`RepoSearchResult`] instances into combined `<search_feedback>` blocks.
pub fn format_search_feedback(results: &[RepoSearchResult]) -> String {
    results
        .iter()
        .map(format_single_search_feedback)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_normalize_search_path_root_and_subdirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        assert_eq!(normalize_search_path(root, "").unwrap(), "");
        assert_eq!(normalize_search_path(root, "  ").unwrap(), "");
        assert_eq!(normalize_search_path(root, ".").unwrap(), "");
        assert_eq!(normalize_search_path(root, "./").unwrap(), "");

        assert_eq!(
            normalize_search_path(root, "crates/core").unwrap(),
            "crates/core"
        );
        assert_eq!(
            normalize_search_path(root, "./crates/core/").unwrap(),
            "crates/core"
        );
    }

    #[test]
    fn test_normalize_search_path_rejects_traversal_and_abs() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        assert!(normalize_search_path(root, "../outside").is_err());
        assert!(normalize_search_path(root, "crates/../../outside").is_err());
        assert!(normalize_search_path(root, "/abs/path").is_err());
        assert!(normalize_search_path(root, "crates\0core").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_normalize_search_path_rejects_external_symlink() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = dir.path();

        let ext_file = outside.path().join("secret.txt");
        std::fs::write(&ext_file, "secret").unwrap();

        let sym_file = root.join("sym_secret.txt");
        std::os::unix::fs::symlink(&ext_file, &sym_file).unwrap();

        assert!(normalize_search_path(root, "sym_secret.txt").is_err());
    }

    #[test]
    fn test_search_by_root_and_subdirectory() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        std::fs::write(root.join("root.rs"), "pub fn root_action() {}\n").unwrap();
        std::fs::write(
            src_dir.join("lib.rs"),
            "pub fn lib_action() {}\npub fn other() {}\n",
        )
        .unwrap();

        let available = vec!["root.rs".to_string(), "src/lib.rs".to_string()];
        let limits = DiscoveryLimits::default();

        // Search from root: finds both
        let query_root = RepoSearchRequest::new(".", r"pub fn \w+");
        let res_root = execute_repo_search(root, &query_root, &available, limits);
        assert!(res_root.error.is_none());
        assert_eq!(res_root.total_matched_files, 2);
        assert_eq!(res_root.matches[0].path, "src/lib.rs");
        assert_eq!(res_root.matches[0].count, 2);
        assert_eq!(res_root.matches[1].path, "root.rs");
        assert_eq!(res_root.matches[1].count, 1);

        // Search inside 'src': finds only 'src/lib.rs'
        let query_src = RepoSearchRequest::new("src", r"pub fn \w+");
        let res_src = execute_repo_search(root, &query_src, &available, limits);
        assert!(res_src.error.is_none());
        assert_eq!(res_src.total_matched_files, 1);
        assert_eq!(res_src.matches[0].path, "src/lib.rs");
    }

    #[test]
    fn test_search_skips_excluded_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let git_dir = root.join(".git");
        let target_dir = root.join("target");
        let tauqe_dir = root.join(".tauqe");
        let modules_dir = root.join("node_modules");

        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&tauqe_dir).unwrap();
        std::fs::create_dir_all(&modules_dir).unwrap();

        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/master").unwrap();
        std::fs::write(target_dir.join("debug.log"), "compiled target").unwrap();
        std::fs::write(tauqe_dir.join("session.json"), "session data").unwrap();
        std::fs::write(modules_dir.join("index.js"), "module.exports = {}").unwrap();

        let available = vec![
            ".git/HEAD".to_string(),
            "target/debug.log".to_string(),
            ".tauqe/session.json".to_string(),
            "node_modules/index.js".to_string(),
        ];

        let query = RepoSearchRequest::new("", r"\w+");
        let res = execute_repo_search(root, &query, &available, DiscoveryLimits::default());
        assert_eq!(res.total_matched_files, 0);
        assert!(res.matches.is_empty());
    }

    #[test]
    fn test_search_skips_binary_and_large_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Binary with NUL byte
        std::fs::write(root.join("bin.dat"), b"data\0hidden").unwrap();
        // Invalid UTF-8
        std::fs::write(root.join("bad_utf8.bin"), [0xFF, 0xFE, 0xFD]).unwrap();
        // Normal text
        std::fs::write(root.join("ok.txt"), "hello world").unwrap();

        let available = vec![
            "bin.dat".to_string(),
            "bad_utf8.bin".to_string(),
            "ok.txt".to_string(),
        ];

        let query = RepoSearchRequest::new(".", "hello|data");
        let res = execute_repo_search(root, &query, &available, DiscoveryLimits::default());
        assert_eq!(res.total_matched_files, 1);
        assert_eq!(res.matches[0].path, "ok.txt");
    }

    #[test]
    fn test_search_truncation_and_omitted_count() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let mut available = Vec::new();
        for i in 0..60 {
            let name = format!("file_{:02}.txt", i);
            std::fs::write(root.join(&name), "needle in haystack").unwrap();
            available.push(name);
        }

        let query = RepoSearchRequest::new("", "needle");
        let limits = DiscoveryLimits {
            max_matched_files_per_search: 50,
            ..DiscoveryLimits::default()
        };
        let res = execute_repo_search(root, &query, &available, limits);
        assert_eq!(res.total_matched_files, 60);
        assert_eq!(res.matches.len(), 50);
        assert_eq!(res.omitted_files, 10);

        let feedback = format_single_search_feedback(&res);
        assert!(feedback.contains("[10 more matching files omitted]"));
        assert!(feedback.contains("file_00.txt: 1 match"));

        // The cap is configuration-driven, so a project narrows how much of a
        // broad match set reaches the model without recompiling.
        let narrow = DiscoveryLimits {
            max_matched_files_per_search: 3,
            ..DiscoveryLimits::default()
        };
        let res_narrow = execute_repo_search(root, &query, &available, narrow);
        assert_eq!(res_narrow.total_matched_files, 60);
        assert_eq!(res_narrow.matches.len(), 3);
        assert_eq!(res_narrow.omitted_files, 57);
    }

    #[test]
    fn test_search_regex_syntax_error_diagnostics() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Unsupported lookaround assertion
        let query_lookaround = RepoSearchRequest::new("", r"(?=pattern)");
        let res = execute_repo_search(root, &query_lookaround, &[], DiscoveryLimits::default());
        let err = res.error.expect("Expected syntax error");
        assert!(err.contains("Lookaround assertions are not supported"));

        // General invalid regex
        let query_broken = RepoSearchRequest::new("", r"([a-z");
        let res_broken = execute_repo_search(root, &query_broken, &[], DiscoveryLimits::default());
        let err_broken = res_broken.error.expect("Expected syntax error");
        assert!(err_broken.contains("Regex syntax error"));
    }

    #[test]
    fn test_search_query_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let queries = (0..5)
            .map(|i| RepoSearchRequest::new("", format!("query_{}", i)))
            .collect::<Vec<_>>();

        let limits = DiscoveryLimits {
            max_search_requests_per_round: 2,
            ..DiscoveryLimits::default()
        };
        let results = execute_repo_searches(root, &queries, &[], limits);
        assert_eq!(results.len(), 5);
        for res in results.iter().take(2) {
            assert!(res.error.is_none());
        }
        for res in results.iter().skip(2) {
            let err = res.error.as_ref().expect("Expected limit error");
            assert!(err.contains("Search query limit exceeded"));
            assert!(err.contains("maximum 2 queries"));
        }
    }

    #[test]
    fn test_format_search_feedback_empty_and_error() {
        let no_matches = RepoSearchResult {
            query: RepoSearchRequest::new("src", "missing"),
            matches: Vec::new(),
            total_matched_files: 0,
            total_matches: 0,
            omitted_files: 0,
            error: None,
        };
        let feedback = format_single_search_feedback(&no_matches);
        assert!(feedback.contains("<search_feedback path=\"src\" regex=\"missing\">"));
        assert!(feedback.contains("No matches found."));

        let err_res = RepoSearchResult {
            query: RepoSearchRequest::new("../escape", "foo"),
            matches: Vec::new(),
            total_matched_files: 0,
            total_matches: 0,
            omitted_files: 0,
            error: Some("Directory traversal ('..') is not allowed".to_string()),
        };
        let err_feedback = format_single_search_feedback(&err_res);
        assert!(err_feedback.contains("Error: Directory traversal ('..') is not allowed"));
    }
}
