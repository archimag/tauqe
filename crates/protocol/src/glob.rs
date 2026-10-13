/// Matches a relative path against a glob pattern.
///
/// Supported syntax:
/// - `*` matches any run of characters within a single path segment.
/// - `?` matches exactly one character.
/// - `[abc]`, `[a-z]` and `[!a-z]` match a single character from a class.
/// - `**` matches zero or more whole path segments.
/// - A trailing `/` matches every path beneath a directory.
/// - A non-wildcard pattern matches itself or acts as a directory prefix.
/// - A wildcard pattern without `/` matches the basename at any depth.
pub fn matches_glob_pattern(pattern: &str, path: &str) -> bool {
    let clean_pat = pattern.trim().trim_start_matches("./");
    let clean_path = path.trim().trim_start_matches("./");

    if clean_pat.is_empty() {
        return false;
    }

    if clean_path == clean_pat {
        return true;
    }

    if clean_pat.ends_with('/') {
        let dir = clean_pat.trim_end_matches('/');
        return clean_path.starts_with(&format!("{}/", dir));
    }

    if !contains_glob_meta(clean_pat) {
        return clean_path.starts_with(&format!("{}/", clean_pat));
    }

    // A wildcard pattern without a slash matches the basename at any depth.
    if !clean_pat.contains('/') {
        let basename = clean_path.rsplit('/').next().unwrap_or(clean_path);
        return match_wildcard_string(clean_pat, basename);
    }

    let pat_parts = collapse_double_stars(clean_pat.split('/').collect());
    let path_parts: Vec<&str> = clean_path.split('/').collect();

    match_segments(&pat_parts, &path_parts)
}

/// Returns `true` when the pattern contains any glob metacharacter.
fn contains_glob_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| matches!(c, '*' | '?' | '[' | ']'))
}

/// Collapses runs of adjacent `**` segments into a single `**`.
///
/// `**` already matches any number of segments, so consecutive `**` segments
/// are redundant. Collapsing them prevents exponential backtracking in
/// [`match_segments`] for patterns such as `a/**/**/b`.
fn collapse_double_stars(parts: Vec<&str>) -> Vec<&str> {
    let mut collapsed: Vec<&str> = Vec::with_capacity(parts.len());
    for part in parts {
        if part == "**" && collapsed.last().copied() == Some("**") {
            continue;
        }
        collapsed.push(part);
    }
    collapsed
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }

    if pat[0] == "**" {
        if match_segments(&pat[1..], path) {
            return true;
        }
        if !path.is_empty() && match_segments(pat, &path[1..]) {
            return true;
        }
        return false;
    }

    if path.is_empty() {
        return false;
    }

    if match_wildcard_string(pat[0], path[0]) {
        return match_segments(&pat[1..], &path[1..]);
    }

    false
}

/// Matches a single path segment against a glob fragment.
///
/// Operates on Unicode scalar values, so `?` and character classes consume a
/// whole character rather than a single byte.
fn match_wildcard_string(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();

    let mut p_idx = 0;
    let mut s_idx = 0;
    let mut star_p: Option<usize> = None;
    let mut star_s = 0;

    while s_idx < t.len() {
        if p.get(p_idx) == Some(&'*') {
            star_p = Some(p_idx);
            star_s = s_idx;
            p_idx += 1;
        } else if let Some(next_p) = match_single(&p, p_idx, t[s_idx]) {
            p_idx = next_p;
            s_idx += 1;
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            star_s += 1;
            s_idx = star_s;
        } else {
            return false;
        }
    }

    while p.get(p_idx) == Some(&'*') {
        p_idx += 1;
    }

    p_idx == p.len()
}

/// Matches one pattern element starting at `p[p_idx]` against `ch`.
///
/// Returns the pattern index just past the consumed element on a match, or
/// `None` otherwise. The caller handles `*` before calling this helper.
fn match_single(p: &[char], p_idx: usize, ch: char) -> Option<usize> {
    match p.get(p_idx)? {
        '?' => Some(p_idx + 1),
        '[' => match_char_class(p, p_idx, ch),
        c if *c == ch => Some(p_idx + 1),
        _ => None,
    }
}

/// Matches the character class beginning at `p[p_idx] == '['` against `ch`.
///
/// Supports plain members (`[abc]`), ranges (`[a-z]`) and negation (`[!a-z]`
/// or `[^a-z]`). Returns the pattern index just past the closing `]` on a
/// match, or `None` otherwise. An unterminated class degrades to a literal `[`.
fn match_char_class(p: &[char], p_idx: usize, ch: char) -> Option<usize> {
    let mut idx = p_idx + 1;
    let negated = matches!(p.get(idx), Some(&'!') | Some(&'^'));
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut first = true;
    while idx < p.len() {
        let c = p[idx];
        if c == ']' && !first {
            return if matched != negated { Some(idx + 1) } else { None };
        }
        first = false;

        if idx + 2 < p.len() && p[idx + 1] == '-' && p[idx + 2] != ']' {
            let (lo, hi) = (c, p[idx + 2]);
            if (lo..=hi).contains(&ch) {
                matched = true;
            }
            idx += 3;
        } else {
            if c == ch {
                matched = true;
            }
            idx += 1;
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
}
