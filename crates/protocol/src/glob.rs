/// Matches a relative path against a glob pattern.
pub fn matches_glob_pattern(pattern: &str, path: &str) -> bool {
    let clean_pat = pattern.trim().trim_start_matches("./");
    let clean_path = path.trim().trim_start_matches("./");

    if clean_pat.is_empty() {
        return false;
    }

    if clean_pat.ends_with('/') {
        let dir = clean_pat.trim_end_matches('/');
        return clean_path.starts_with(&format!("{}/", dir));
    }

    if !clean_pat.contains('*')
        && !clean_pat.contains('?')
        && (clean_path == clean_pat || clean_path.starts_with(&format!("{}/", clean_pat)))
    {
        return true;
    }

    if !clean_pat.contains('/') && clean_pat.starts_with("*.") {
        let ext = &clean_pat[1..];
        return clean_path.ends_with(ext);
    }

    let pat_parts: Vec<&str> = clean_pat.split('/').collect();
    let path_parts: Vec<&str> = clean_path.split('/').collect();

    match_segments(&pat_parts, &path_parts)
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

fn match_wildcard_string(pattern: &str, s: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let s_bytes = s.as_bytes();
    let (mut p_idx, mut s_idx) = (0, 0);
    let mut star_idx = None;
    let mut match_idx = 0;

    while s_idx < s_bytes.len() {
        if p_idx < p_bytes.len() && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == s_bytes[s_idx]) {
            p_idx += 1;
            s_idx += 1;
        } else if p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
            star_idx = Some(p_idx);
            match_idx = s_idx;
            p_idx += 1;
        } else if let Some(star) = star_idx {
            p_idx = star + 1;
            match_idx += 1;
            s_idx = match_idx;
        } else {
            return false;
        }
    }

    while p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == p_bytes.len()
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
    fn test_doublestar_glob() {
        assert!(matches_glob_pattern("src/**/*.rs", "src/a/b/c.rs"));
        assert!(matches_glob_pattern("**/test.rs", "crates/foo/test.rs"));
        assert!(!matches_glob_pattern("src/**/*.rs", "docs/guide.md"));
    }

    #[test]
    fn test_question_mark_wildcard() {
        assert!(matches_glob_pattern("te?t.rs", "test.rs"));
        assert!(matches_glob_pattern("te?t.rs", "text.rs"));
        assert!(!matches_glob_pattern("te?t.rs", "toast.rs"));
    }

    #[test]
    fn test_empty_pattern() {
        assert!(!matches_glob_pattern("", "src/lib.rs"));
        assert!(!matches_glob_pattern("   ", "src/lib.rs"));
    }
}
