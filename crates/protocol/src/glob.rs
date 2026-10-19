/// Matches a relative path against a glob pattern.
///
/// Supported syntax:
/// - `*` matches any run of characters within a single path segment.
/// - `?` matches exactly one character.
/// - `[abc]`, `[a-z]` and `[!a-z]` match a single character from a class.
/// - `**` matches zero or more whole path segments.
/// - A trailing `/` matches every path beneath a directory (literal or wildcard,
///   such as `crates/*/` or `src/**/`).
/// - A non-wildcard pattern matches itself or acts as a directory prefix.
/// - A wildcard pattern without `/` matches the basename at any depth and does
///   not act as a directory prefix (use a trailing slash such as `build*/` to
///   match all paths beneath matching directories).
///
/// Leading, trailing and repeated separators (both `/` and `\`), as well as
/// interior `.` segments, are normalized before matching, so `"/src//lib.rs"`,
/// `"src/./lib.rs"` and `"src\\lib.rs"` all match `"src/lib.rs"`.
///
/// This convenience wrapper compiles `pattern` on every call. When matching many
/// paths against one pattern, compile once with [`GlobPattern::new`] and reuse it.
pub fn matches_glob_pattern(pattern: &str, path: &str) -> bool {
    GlobPattern::new(pattern).matches(path)
}

/// A pre-compiled glob pattern.
///
/// Compiling normalizes the pattern, records its structural flags, and
/// precomputes the collapsed `**` segment lists once, so matching many paths
/// against the same pattern never re-normalizes the pattern or re-allocates its
/// segments. Callers that match a single pattern against a large file list
/// (pattern-based context addition, review path selection) should build one
/// `GlobPattern` and call [`GlobPattern::matches`] per path.
pub struct GlobPattern {
    clean: String,
    trailing_slash: bool,
    has_meta: bool,
    has_slash: bool,
    prefix: String,
    segments: Vec<String>,
    dir_segments: Vec<String>,
}

impl GlobPattern {
    /// Compiles `pattern` into a reusable matcher.
    pub fn new(pattern: &str) -> Self {
        let trimmed = pattern.trim();
        let clean = normalize_path(trimmed);
        let trailing_slash = trimmed.ends_with('/') || trimmed.ends_with('\\');
        let has_meta = contains_glob_meta(&clean);
        let has_slash = clean.contains('/');
        let prefix = format!("{clean}/");
        let segments = collapse_double_stars(clean.split('/').map(String::from).collect());
        let dir_segments = collapse_double_stars(
            format!("{clean}/*/**")
                .split('/')
                .map(String::from)
                .collect(),
        );

        Self {
            clean,
            trailing_slash,
            has_meta,
            has_slash,
            prefix,
            segments,
            dir_segments,
        }
    }

    /// Returns `true` when `path` matches the compiled pattern.
    pub fn matches(&self, path: &str) -> bool {
        let clean_path = normalize_path(path);

        if self.clean.is_empty() || clean_path.is_empty() {
            return false;
        }

        // A trailing slash marks a directory prefix, matching every path beneath it.
        if self.trailing_slash {
            if !self.has_meta {
                return clean_path.starts_with(&self.prefix);
            }
            let path_parts: Vec<&str> = clean_path.split('/').collect();
            return match_segments(&self.dir_segments, &path_parts);
        }

        if clean_path == self.clean {
            return true;
        }

        if !self.has_meta {
            return clean_path.starts_with(&self.prefix);
        }

        // A wildcard pattern without a slash matches the basename at any depth.
        if !self.has_slash {
            let basename = clean_path.rsplit('/').next().unwrap_or(clean_path.as_str());
            return match_wildcard_string(&self.clean, basename);
        }

        let path_parts: Vec<&str> = clean_path.split('/').collect();
        match_segments(&self.segments, &path_parts)
    }
}

/// Normalizes a path or pattern into a canonical slash-separated form.
///
/// Surrounding whitespace is trimmed, Windows-style backslash separators are
/// converted to slashes, and empty segments (from leading, trailing or
/// repeated separators) and `.` segments are dropped. As a result
/// `"/src//lib.rs"`, `"src/./lib.rs"` and `"src\\lib.rs"` all canonicalize to
/// `"src/lib.rs"`.
fn normalize_path(input: &str) -> String {
    let normalized = input.trim().replace('\\', "/");
    normalized
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<&str>>()
        .join("/")
}

/// Returns `true` when the pattern contains any glob metacharacter.
///
/// A lone `]` is a literal: a character class always begins with `[`, so only
/// `*`, `?` and `[` change matching semantics. Treating `]` as a wildcard would
/// flip a literal name such as `foo]` from a directory prefix into a
/// basename-only wildcard.
fn contains_glob_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Collapses runs of adjacent `**` segments into a single `**`.
///
/// `**` already matches any number of segments, so consecutive `**` segments
/// are redundant. Collapsing them prevents exponential backtracking in
/// [`match_segments`] for patterns such as `a/**/**/b`.
fn collapse_double_stars(parts: Vec<String>) -> Vec<String> {
    let mut collapsed: Vec<String> = Vec::with_capacity(parts.len());
    for part in parts {
        if part == "**" && collapsed.last().map(String::as_str) == Some("**") {
            continue;
        }
        collapsed.push(part);
    }
    collapsed
}

fn match_segments(pat: &[String], path: &[&str]) -> bool {
    // Without `**` every pattern segment matches exactly one path segment, so
    // the match is a straight positional comparison that needs no backtracking
    // memo. The memo is only allocated when a `**` segment is actually present.
    if !pat.iter().any(|segment| segment == "**") {
        return pat.len() == path.len()
            && pat
                .iter()
                .zip(path)
                .all(|(segment, path_segment)| match_wildcard_string(segment, path_segment));
    }

    let cols = path.len() + 1;
    let mut visited = vec![false; (pat.len() + 1) * cols];
    match_segments_at(pat, path, 0, 0, cols, &mut visited)
}

/// Index-based segment matcher backed by a visited-state memo.
///
/// `**` may consume any number of segments, so a naive recursion splits at
/// every boundary and degrades to `O(2^k)` in the number of `**` groups. Each
/// `(pat_idx, path_idx)` state is a pure function of its indices, and any state
/// that succeeds short-circuits the whole match, so states are only ever
/// recomputed after a failure. Recording them collapses the search to
/// `O(pat_len * path_len)`.
///
/// The memo is a single flat `bool` buffer indexed as `pat_idx * cols +
/// path_idx`, where `cols = path.len() + 1`, so no per-row heap allocation is
/// required.
fn match_segments_at(
    pat: &[String],
    path: &[&str],
    pat_idx: usize,
    path_idx: usize,
    cols: usize,
    visited: &mut [bool],
) -> bool {
    let state = pat_idx * cols + path_idx;
    if visited[state] {
        return false;
    }
    visited[state] = true;

    if pat_idx == pat.len() {
        return path_idx == path.len();
    }

    if pat[pat_idx] == "**" {
        if match_segments_at(pat, path, pat_idx + 1, path_idx, cols, visited) {
            return true;
        }
        if path_idx < path.len()
            && match_segments_at(pat, path, pat_idx, path_idx + 1, cols, visited)
        {
            return true;
        }
        return false;
    }

    if path_idx == path.len() {
        return false;
    }

    if match_wildcard_string(&pat[pat_idx], path[path_idx]) {
        return match_segments_at(pat, path, pat_idx + 1, path_idx + 1, cols, visited);
    }

    false
}

/// Matches a single path segment against a glob fragment.
///
/// Operates on Unicode scalar values, so `?` and character classes consume a
/// whole character rather than a single byte. The pattern and the segment are
/// walked in place by byte offset, so no intermediate `Vec<char>` is allocated
/// per segment.
fn match_wildcard_string(pattern: &str, s: &str) -> bool {
    let mut p_idx = 0;
    let mut s_idx = 0;
    let mut star_p: Option<usize> = None;
    let mut star_s = 0;

    while s_idx < s.len() {
        let Some(s_ch) = s[s_idx..].chars().next() else {
            break;
        };

        if pattern[p_idx..].starts_with('*') {
            star_p = Some(p_idx);
            star_s = s_idx;
            p_idx += 1;
        } else if let Some(next_p) = match_single(pattern, p_idx, s_ch) {
            p_idx = next_p;
            s_idx += s_ch.len_utf8();
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            star_s += s[star_s..].chars().next().map_or(0, |c| c.len_utf8());
            s_idx = star_s;
        } else {
            return false;
        }
    }

    while pattern[p_idx..].starts_with('*') {
        p_idx += 1;
    }

    p_idx == pattern.len()
}

/// Matches one pattern element starting at byte `p_idx` against `ch`.
///
/// Returns the pattern byte offset just past the consumed element on a match,
/// or `None` otherwise. The caller handles `*` before calling this helper.
fn match_single(pattern: &str, p_idx: usize, ch: char) -> Option<usize> {
    match pattern[p_idx..].chars().next()? {
        '?' => Some(p_idx + 1),
        '[' => match_char_class(pattern, p_idx, ch),
        c if c == ch => Some(p_idx + c.len_utf8()),
        _ => None,
    }
}

/// Matches the character class beginning at `pattern[p_idx] == '['` against
/// `ch`.
///
/// Supports plain members (`[abc]`), ranges (`[a-z]`) and negation (`[!a-z]`
/// or `[^a-z]`). Returns the pattern byte offset just past the closing `]` on a
/// match, or `None` otherwise. An unterminated class degrades to a literal `[`.
fn match_char_class(pattern: &str, p_idx: usize, ch: char) -> Option<usize> {
    let mut idx = p_idx + 1;
    let negated = matches!(pattern[idx..].chars().next(), Some('!') | Some('^'));
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut first = true;
    let mut chars = pattern[idx..].char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let c_idx = idx + offset;
        if c == ']' && !first {
            return if matched != negated {
                Some(c_idx + 1)
            } else {
                None
            };
        }
        first = false;

        if let Some(&(dash_offset, '-')) = chars.peek() {
            let hi_idx = idx + dash_offset + 1;
            if let Some(hi) = pattern[hi_idx..].chars().next() {
                if hi != ']' {
                    chars.next();
                    chars.next();
                    if (c..=hi).contains(&ch) {
                        matched = true;
                    }
                    continue;
                }
            }
        }

        if c == ch {
            matched = true;
        }
    }

    // Unterminated class: degrade to a literal '[' comparison.
    if ch == '[' {
        Some(p_idx + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_and_prefix_match() {
        assert!(matches_glob_pattern("src/lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("./src/lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src/", "src/lib.rs"));
        assert!(!matches_glob_pattern("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn test_extension_wildcard() {
        assert!(matches_glob_pattern("*.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("*.rs", "main.rs"));
        assert!(!matches_glob_pattern("*.rs", "main.toml"));
    }

    #[test]
    fn test_extension_wildcard_with_meta_characters() {
        assert!(matches_glob_pattern("*.r?", "main.rc"));
        assert!(matches_glob_pattern("*.r?", "src/main.rc"));
        assert!(!matches_glob_pattern("*.r?", "main.r"));
        assert!(matches_glob_pattern("*.*", "src/lib.rs"));
        assert!(!matches_glob_pattern("*.*", "Makefile"));
        assert!(matches_glob_pattern("*.[a-z]", "main.c"));
        assert!(!matches_glob_pattern("*.[a-z]", "main.C"));
    }

    #[test]
    fn test_doublestar_glob() {
        assert!(matches_glob_pattern("src/**/*.rs", "src/a/b/c.rs"));
        assert!(matches_glob_pattern("**/test.rs", "crates/foo/test.rs"));
        assert!(!matches_glob_pattern("src/**/*.rs", "docs/guide.md"));
    }

    #[test]
    fn test_repeated_double_star_segments_collapse() {
        assert!(matches_glob_pattern("**/**/*.rs", "a/b/c.rs"));
        assert!(matches_glob_pattern("src/**/**/lib.rs", "src/a/b/lib.rs"));
        assert!(!matches_glob_pattern("src/**/**/lib.rs", "docs/lib.rs"));
    }

    #[test]
    fn test_question_mark_wildcard() {
        assert!(matches_glob_pattern("te?t.rs", "test.rs"));
        assert!(matches_glob_pattern("te?t.rs", "text.rs"));
        assert!(!matches_glob_pattern("te?t.rs", "toast.rs"));
    }

    #[test]
    fn test_question_mark_matches_unicode_scalar() {
        assert!(matches_glob_pattern("?.rs", "ф.rs"));
        assert!(matches_glob_pattern("??.rs", "фы.rs"));
        assert!(!matches_glob_pattern("?.rs", "ab.rs"));
    }

    #[test]
    fn test_character_class_ranges_and_negation() {
        assert!(matches_glob_pattern("file[0-9].rs", "file7.rs"));
        assert!(!matches_glob_pattern("file[0-9].rs", "filex.rs"));
        assert!(matches_glob_pattern("file[!0-9].rs", "filex.rs"));
        assert!(!matches_glob_pattern("file[!0-9].rs", "file7.rs"));
    }

    #[test]
    fn test_empty_pattern() {
        assert!(!matches_glob_pattern("", "src/lib.rs"));
        assert!(!matches_glob_pattern("   ", "src/lib.rs"));
    }

    #[test]
    fn test_leading_repeated_and_dot_slash_normalization() {
        assert!(matches_glob_pattern("/src/lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src/lib.rs", "/src/lib.rs"));
        assert!(matches_glob_pattern("src//lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src/./lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("././src/lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src//*.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src/*.rs", "src//lib.rs"));
        assert!(matches_glob_pattern("src//", "src/lib.rs"));
        assert!(!matches_glob_pattern("src/", "src"));
    }

    #[test]
    fn test_non_adjacent_double_star_matching() {
        assert!(matches_glob_pattern("**/a/**/b/**/c.rs", "x/a/y/b/z/c.rs"));
        assert!(matches_glob_pattern("**/a/**/b/**/c.rs", "a/b/c.rs"));
        assert!(!matches_glob_pattern("**/a/**/b/**/c.rs", "a/b/d.rs"));
    }

    #[test]
    fn test_many_double_star_groups_terminate_promptly() {
        // Naive recursion over multiple `**` groups backtracks exponentially;
        // the memoized matcher must resolve this in polynomial time.
        let pattern = format!("{}never", "**/x/".repeat(32));
        let path = format!("{}y", "x/".repeat(32));
        assert!(!matches_glob_pattern(&pattern, &path));
    }

    #[test]
    fn test_trailing_slash_with_glob_meta() {
        assert!(matches_glob_pattern("src/**/", "src/a/b.rs"));
        assert!(matches_glob_pattern("src/**/", "src/lib.rs"));
        assert!(!matches_glob_pattern("src/**/", "src"));
        assert!(matches_glob_pattern(
            "crates/*/",
            "crates/protocol/src/lib.rs"
        ));
        assert!(matches_glob_pattern("crates/*/", "crates/foo/Cargo.toml"));
        assert!(!matches_glob_pattern("crates/*/", "crates/foo"));
        assert!(!matches_glob_pattern("crates/*/", "crates"));
        assert!(matches_glob_pattern("file[0-9]/", "file1/test.rs"));
        assert!(!matches_glob_pattern("file[0-9]/", "file1"));
        assert!(matches_glob_pattern("build*/", "build-out/x.o"));
        assert!(!matches_glob_pattern("build*/", "build-out"));
    }

    #[test]
    fn test_wildcard_without_slash_matches_basename_not_dir_prefix() {
        assert!(matches_glob_pattern("build*", "src/build-helper.rs"));
        assert!(matches_glob_pattern("build*", "build.rs"));
        assert!(!matches_glob_pattern("build*", "build-out/x.o"));
        assert!(matches_glob_pattern("build*/", "build-out/x.o"));
        assert!(matches_glob_pattern("build", "build/x.o"));
    }

    #[test]
    fn test_compiled_pattern_matches_like_wrapper() {
        let patterns = [
            "src/*.rs",
            "**/test.rs",
            "crates/*/",
            "build*",
            "src/lib.rs",
        ];
        let paths = [
            "src/lib.rs",
            "src/main.rs",
            "crates/foo/test.rs",
            "build.rs",
            "build-out/x.o",
            "docs/guide.md",
        ];
        for pattern in patterns {
            let compiled = GlobPattern::new(pattern);
            for path in paths {
                assert_eq!(
                    compiled.matches(path),
                    matches_glob_pattern(pattern, path),
                    "pattern {pattern:?} path {path:?}"
                );
            }
        }
    }

    #[test]
    fn test_compiled_pattern_reuses_precomputed_segments() {
        let pattern = GlobPattern::new("src/**/lib.rs");
        assert!(pattern.matches("src/lib.rs"));
        assert!(pattern.matches("src/a/b/lib.rs"));
        assert!(!pattern.matches("docs/lib.rs"));
    }

    #[test]
    fn test_lone_closing_bracket_is_literal_not_a_wildcard() {
        // A lone `]` must not flip the pattern into wildcard mode: `foo]` is a
        // literal directory name, so it acts as a directory prefix.
        assert!(matches_glob_pattern("foo]", "foo]/bar.rs"));
        assert!(matches_glob_pattern("foo]", "foo]"));
        assert!(!matches_glob_pattern("foo]", "food/bar.rs"));
    }

    #[test]
    fn test_backslash_separators_are_normalized() {
        assert!(matches_glob_pattern("src\\lib.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src/lib.rs", "src\\lib.rs"));
        assert!(matches_glob_pattern("src\\*.rs", "src/lib.rs"));
        assert!(matches_glob_pattern("src\\**\\*.rs", "src/a/b.rs"));
        assert!(!matches_glob_pattern("src\\main.rs", "src/lib.rs"));
    }

    #[test]
    fn test_backslash_trailing_separator_marks_directory() {
        assert!(matches_glob_pattern("src\\", "src/lib.rs"));
        assert!(!matches_glob_pattern("src\\", "src"));
        assert!(matches_glob_pattern("crates\\*\\", "crates/foo/src/lib.rs"));
        assert!(!matches_glob_pattern("crates\\*\\", "crates/foo"));
    }

    #[test]
    fn test_negated_char_class_with_leading_dash() {
        // `[!-]` is a negated class whose only member is a literal `-`.
        assert!(!matches_glob_pattern("[!-]", "-"));
        assert!(matches_glob_pattern("[!-]", "a"));
        assert!(matches_glob_pattern("[!-]", "ф"));
    }

    #[test]
    fn test_unterminated_negated_char_class_degrades_to_literal_bracket() {
        // Without a closing `]` the class degrades to a literal `[` prefix.
        assert!(matches_glob_pattern("[!abc", "[!abc"));
        assert!(!matches_glob_pattern("[!abc", "abc"));
        assert!(!matches_glob_pattern("[!abc", "x"));
    }

    #[test]
    fn test_caret_negation_marker() {
        assert!(matches_glob_pattern("[^a-z]", "1"));
        assert!(!matches_glob_pattern("[^a-z]", "q"));
    }
}
