/// Resolves a candidate file path against a list of permitted editable paths.
pub fn resolve_target_path(candidate: &str, editable_paths: &[String]) -> String {
    let cleaned = candidate.trim().trim_matches('`').trim();
    let cleaned = cleaned.strip_prefix("./").unwrap_or(cleaned);
    let cleaned = cleaned.strip_prefix('/').unwrap_or(cleaned);

    for ed in editable_paths {
        let ed_clean = ed.strip_prefix("./").unwrap_or(ed);
        if cleaned == ed_clean
            || cleaned.ends_with(&format!("/{}", ed_clean))
            || ed_clean.ends_with(&format!("/{}", cleaned))
        {
            return ed.clone();
        }
    }
    cleaned.to_string()
}

/// Normalizes line breaks and whitespace around content.
pub fn normalize_content(raw: &str) -> String {
    let s = if let Some(stripped) = raw.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = raw.strip_prefix('\n') {
        stripped
    } else {
        raw
    };

    if let Some(last_nl) = s.rfind('\n') {
        let trailing = &s[last_nl + 1..];
        if trailing.chars().all(|c| c == ' ' || c == '\t') {
            return s[..=last_nl].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_target_path() {
        let editable = vec!["crates/core/src/lib.rs".to_string()];
        assert_eq!(
            resolve_target_path("crates/core/src/lib.rs", &editable),
            "crates/core/src/lib.rs"
        );
        assert_eq!(
            resolve_target_path("src/lib.rs", &editable),
            "crates/core/src/lib.rs"
        );
        assert_eq!(
            resolve_target_path("other/file.rs", &editable),
            "other/file.rs"
        );
    }

    #[test]
    fn test_normalize_content() {
        assert_eq!(normalize_content("\nhello\n   "), "hello\n");
        assert_eq!(normalize_content("\r\nhello"), "hello");
    }
}
