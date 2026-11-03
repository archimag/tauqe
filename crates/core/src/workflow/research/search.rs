use std::path::Path;

use regex::RegexBuilder;
use tauqe_protocol::RepoSearchRequest;

use crate::blocking::run_blocking;
use crate::context::path::verify_path_within_repo;
use crate::escape::escape_xml_attr;

use super::ResearchLimits;

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

/// Excluded service directories are a substrate-level path predicate, so the
/// definition lives in `tauqe-tools` and this re-export keeps
/// `search::is_excluded_path` resolvable for the research layer.
pub use tauqe_tools::paths::is_excluded_path;

/// Normalized path scope of one repository search.
///
/// The empty scope denotes the repository root. A subdirectory scope keeps the
/// `path/` prefix its files carry, so resolving the round corpus and testing one
/// query both decide membership without re-joining strings per file.
#[derive(Debug)]
struct SearchScope {
    /// Normalized scope path; empty for the repository root.
    path: String,
    /// `path/` prefix of a subdirectory scope; empty for the root scope.
    prefix: String,
}

impl SearchScope {
    /// Builds a scope from an already normalized search path.
    fn new(path: String) -> Self {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path)
        };
        Self { path, prefix }
    }

    /// Reports whether a repository-relative file lies inside this scope.
    fn admits(&self, candidate: &str) -> bool {
        candidate.starts_with(self.prefix.as_str()) || candidate == self.path.as_str()
    }
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

/// Searchable text of one in-scope repository file.
struct CorpusFile<'a> {
    /// Repository-relative path of the file, borrowed from the round's file list.
    path: &'a str,
    /// Decoded text every query of the round runs its compiled pattern over.
    text: String,
}

/// Searchable corpus of one Research round.
///
/// The corpus holds the scope-invariant half of a search. The path-exclusion
/// rule, the symlink boundary check, the regular-file and size gates, the disk
/// read, the NUL-byte scan, and the UTF-8 validation run once per candidate file
/// for the whole round, and every query then evaluates its already compiled
/// pattern over the same retained texts.
struct SearchCorpus<'a> {
    /// Files admitted by at least one prepared query, with their decoded text.
    files: Vec<CorpusFile<'a>>,
}

impl<'a> SearchCorpus<'a> {
    /// Resolves the corpus over the union of the round's query scopes.
    ///
    /// `in_scope` decides whether any prepared query searches a given file, so a
    /// round whose queries all target one subdirectory never reads the rest of
    /// the repository. A file rejected by any gate contributes no text and is
    /// therefore absent: no query could ever match it.
    fn resolve(
        repo_root: &Path,
        available_files: &'a [String],
        in_scope: impl Fn(&str) -> bool,
    ) -> Self {
        let mut files = Vec::new();
        for candidate in available_files {
            if !in_scope(candidate) || is_excluded_path(candidate) {
                continue;
            }
            if let Some(text) = read_searchable_text(repo_root, candidate) {
                files.push(CorpusFile {
                    path: candidate.as_str(),
                    text,
                });
            }
        }
        Self { files }
    }
}

/// Reads the searchable text of one repository file.
///
/// Every gate that decides whether a file may be searched lives here, so the
/// round applies it exactly once per file instead of once per query: a symlink
/// escaping the repository, a non-regular file, a file larger than
/// [`MAX_SEARCHABLE_FILE_SIZE`], a body carrying a NUL byte, and a body that is
/// not valid UTF-8 are all skipped.
fn read_searchable_text(repo_root: &Path, candidate: &str) -> Option<String> {
    let full_path = repo_root.join(candidate);

    let link_meta = std::fs::symlink_metadata(&full_path).ok()?;
    if link_meta.is_symlink() && verify_path_within_repo(repo_root, candidate).is_err() {
        return None;
    }

    let file_meta = std::fs::metadata(&full_path).ok()?;
    if !file_meta.is_file() || file_meta.len() > MAX_SEARCHABLE_FILE_SIZE {
        return None;
    }

    let bytes = std::fs::read(&full_path).ok()?;
    if bytes.contains(&0) {
        return None;
    }

    String::from_utf8(bytes).ok()
}

/// One query of a search round, resolved before the corpus is read.
///
/// A query that names an unreachable path or an invalid pattern never reaches
/// the corpus: it keeps its diagnostic and is reported in the position it was
/// declared, exactly like a query the round limit rejected.
enum RoundQuery<'a> {
    /// Query whose scope and compiled pattern resolved successfully.
    Prepared {
        /// Request the reported result belongs to.
        request: &'a RepoSearchRequest,
        /// Scope the query searches in.
        scope: SearchScope,
        /// Compiled pattern of the query.
        matcher: regex::Regex,
    },
    /// Query rejected before any file was read.
    Rejected {
        /// Request the rejected result belongs to.
        request: &'a RepoSearchRequest,
        /// Diagnostic reported back to the model.
        error: String,
    },
}

impl RoundQuery<'_> {
    /// Reports whether the query searches in the given repository file.
    fn admits(&self, candidate: &str) -> bool {
        match self {
            Self::Prepared { scope, .. } => scope.admits(candidate),
            Self::Rejected { .. } => false,
        }
    }

    /// Evaluates the query against the corpus resolved for the round.
    ///
    /// Only the file texts are shared: the scope predicate, the count-descending
    /// ordering, the truncation against `max_matched_files_per_search`, and the
    /// diagnostic of a rejected query stay the property of the query itself.
    fn evaluate(self, corpus: &SearchCorpus<'_>, limits: ResearchLimits) -> RepoSearchResult {
        match self {
            Self::Prepared {
                request,
                scope,
                matcher,
            } => {
                let mut matches = Vec::new();
                for file in &corpus.files {
                    if !scope.admits(file.path) {
                        continue;
                    }
                    let count = matcher.find_iter(&file.text).count();
                    if count > 0 {
                        matches.push(FileSearchMatch {
                            path: file.path.to_string(),
                            count,
                        });
                    }
                }
                accounted_search_result(request.clone(), matches, limits)
            }
            Self::Rejected { request, error } => failed_search_result(request.clone(), error),
        }
    }
}

/// Builds the round entry of a query rejected before any file was read.
fn rejected_query<'a>(request: &'a RepoSearchRequest, error: String) -> RoundQuery<'a> {
    RoundQuery::Rejected { request, error }
}

/// Resolves the scope and the compiled pattern of one query.
fn prepare_query<'a>(repo_root: &Path, query: &'a RepoSearchRequest) -> RoundQuery<'a> {
    let path = match normalize_search_path(repo_root, &query.path) {
        Ok(path) => path,
        Err(error) => return rejected_query(query, error),
    };
    match compile_search_regex(&query.regex) {
        Ok(matcher) => RoundQuery::Prepared {
            request: query,
            scope: SearchScope::new(path),
            matcher,
        },
        Err(error) => rejected_query(query, error),
    }
}

/// Sorts, truncates, and accounts the matches of one query.
fn accounted_search_result(
    query: RepoSearchRequest,
    mut matches: Vec<FileSearchMatch>,
    limits: ResearchLimits,
) -> RepoSearchResult {
    // Deterministic order: highest occurrence count first, then alphabetical.
    matches.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));

    let total_matched_files = matches.len();
    let total_matches: usize = matches.iter().map(|matched| matched.count).sum();
    let omitted_files = total_matched_files.saturating_sub(limits.max_matched_files_per_search);
    matches.truncate(limits.max_matched_files_per_search);

    RepoSearchResult {
        query,
        matches,
        total_matched_files,
        total_matches,
        omitted_files,
        error: None,
    }
}

/// Builds the result of a query that never reached the corpus.
fn failed_search_result(query: RepoSearchRequest, error: String) -> RepoSearchResult {
    RepoSearchResult {
        query,
        matches: Vec::new(),
        total_matched_files: 0,
        total_matches: 0,
        omitted_files: 0,
        error: Some(error),
    }
}

/// Synchronous internal routine executing a single search on disk.
///
/// A single query is a one-member round: it resolves its scope and its compiled
/// pattern, resolves the searchable corpus over that scope through the very same
/// shared pass the multi-query round uses, and evaluates once.
fn execute_repo_search_sync(
    repo_root: &Path,
    query: &RepoSearchRequest,
    available_files: &[String],
    limits: ResearchLimits,
) -> RepoSearchResult {
    let prepared = prepare_query(repo_root, query);
    let in_scope = |candidate: &str| prepared.admits(candidate);
    let corpus = SearchCorpus::resolve(repo_root, available_files, in_scope);
    prepared.evaluate(&corpus, limits)
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
    limits: ResearchLimits,
) -> RepoSearchResult {
    run_blocking(|| execute_repo_search_sync(repo_root, query, available_files, limits))
}

/// Executes multiple repository searches over a corpus resolved once per round.
///
/// The round resolves every query into its scope and compiled pattern first,
/// then reads the union of those scopes exactly once, and finally evaluates each
/// query over the shared texts.
fn execute_repo_searches_sync(
    repo_root: &Path,
    queries: &[RepoSearchRequest],
    available_files: &[String],
    limits: ResearchLimits,
) -> Vec<RepoSearchResult> {
    let prepared: Vec<RoundQuery<'_>> = queries
        .iter()
        .enumerate()
        .map(|(idx, query)| {
            if idx >= limits.max_search_requests_per_round {
                rejected_query(
                    query,
                    format!(
                        "Search query limit exceeded (maximum {} queries per round allowed)",
                        limits.max_search_requests_per_round
                    ),
                )
            } else {
                prepare_query(repo_root, query)
            }
        })
        .collect();

    let in_scope = |candidate: &str| prepared.iter().any(|query| query.admits(candidate));
    let corpus = SearchCorpus::resolve(repo_root, available_files, in_scope);

    prepared
        .into_iter()
        .map(|query| query.evaluate(&corpus, limits))
        .collect()
}

/// Executes multiple repository searches with round limits and offloaded blocking I/O.
pub fn execute_repo_searches(
    repo_root: &Path,
    queries: &[RepoSearchRequest],
    available_files: &[String],
    limits: ResearchLimits,
) -> Vec<RepoSearchResult> {
    run_blocking(|| execute_repo_searches_sync(repo_root, queries, available_files, limits))
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
        let limits = ResearchLimits::default();

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
        let res = execute_repo_search(root, &query, &available, ResearchLimits::default());
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
        let res = execute_repo_search(root, &query, &available, ResearchLimits::default());
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
        let limits = ResearchLimits {
            max_matched_files_per_search: 50,
            ..ResearchLimits::default()
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
        let narrow = ResearchLimits {
            max_matched_files_per_search: 3,
            ..ResearchLimits::default()
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
        let res = execute_repo_search(root, &query_lookaround, &[], ResearchLimits::default());
        let err = res.error.expect("Expected syntax error");
        assert!(err.contains("Lookaround assertions are not supported"));

        // General invalid regex
        let query_broken = RepoSearchRequest::new("", r"([a-z");
        let res_broken = execute_repo_search(root, &query_broken, &[], ResearchLimits::default());
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

        let limits = ResearchLimits {
            max_search_requests_per_round: 2,
            ..ResearchLimits::default()
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
    fn test_a_multi_query_round_resolves_each_query_against_its_own_scope() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("root.rs"), "needle needle\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "needle\n").unwrap();
        std::fs::write(root.join("target/generated.rs"), "needle\n").unwrap();
        std::fs::write(root.join("binary.dat"), b"needle\0").unwrap();

        let available = vec![
            "root.rs".to_string(),
            "src/lib.rs".to_string(),
            "target/generated.rs".to_string(),
            "binary.dat".to_string(),
        ];

        // One round: a repository-root query, a subdirectory query, an invalid
        // pattern, and a query the round limit rejects. The corpus is resolved
        // once, yet every query keeps its own scope, ordering, and diagnostic.
        let queries = vec![
            RepoSearchRequest::new("", "needle"),
            RepoSearchRequest::new("src", "needle"),
            RepoSearchRequest::new("", "([a-z"),
            RepoSearchRequest::new("", "needle"),
        ];
        let limits = ResearchLimits {
            max_search_requests_per_round: 3,
            ..ResearchLimits::default()
        };

        let results = execute_repo_searches(root, &queries, &available, limits);
        assert_eq!(results.len(), 4);

        // The shared corpus never carries the ignored directory or the binary
        // body, and the higher occurrence count orders first.
        assert!(results[0].error.is_none());
        assert_eq!(results[0].total_matched_files, 2);
        assert_eq!(results[0].matches[0].path, "root.rs");
        assert_eq!(results[0].matches[0].count, 2);
        assert_eq!(results[0].matches[1].path, "src/lib.rs");

        // The subdirectory query is narrowed by its own scope predicate.
        assert!(results[1].error.is_none());
        assert_eq!(results[1].total_matched_files, 1);
        assert_eq!(results[1].matches[0].path, "src/lib.rs");

        // An invalid pattern is reported in the position it was declared.
        let syntax_error = results[2].error.as_ref().expect("expected a syntax error");
        assert!(syntax_error.contains("Regex syntax error"));

        // A query beyond the round limit is rejected without reading any file.
        let limit_error = results[3].error.as_ref().expect("expected a limit error");
        assert!(limit_error.contains("Search query limit exceeded"));
        assert!(limit_error.contains("maximum 3 queries"));
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
