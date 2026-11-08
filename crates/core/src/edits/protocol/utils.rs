use std::path::Path;

/// Resolves a candidate file path against a list of permitted editable paths.
/// For `create` operations, suffix matching is disabled to prevent accidental overwrites.
/// For `replace`/`delete`, exact match is preferred; ambiguous suffix matches return an error.
pub fn resolve_target_path_for_op(
    candidate: &str,
    editable_paths: &[String],
    is_create: bool,
) -> Result<String, String> {
    let cleaned = candidate.trim().trim_matches('`').trim();
    let cleaned = cleaned.strip_prefix("./").unwrap_or(cleaned);
    let cleaned = cleaned.strip_prefix('/').unwrap_or(cleaned);

    if is_create {
        return Ok(cleaned.to_string());
    }

    // 1. Exact match
    for ed in editable_paths {
        let ed_clean = ed.strip_prefix("./").unwrap_or(ed);
        if cleaned == ed_clean {
            return Ok(ed.clone());
        }
    }

    // 2. Suffix match
    let mut matches = Vec::new();
    for ed in editable_paths {
        let ed_clean = ed.strip_prefix("./").unwrap_or(ed);
        if ed_clean.ends_with(&format!("/{}", cleaned))
            || cleaned.ends_with(&format!("/{}", ed_clean))
        {
            matches.push(ed.clone());
        }
    }

    match matches.len() {
        0 => Ok(cleaned.to_string()),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "Path '{}' is ambiguous and matches multiple editable files: {}",
            cleaned,
            matches.join(", ")
        )),
    }
}

/// Resolves a candidate file path against editable paths (defaulting to non-create semantics).
pub fn resolve_target_path(candidate: &str, editable_paths: &[String]) -> String {
    resolve_target_path_for_op(candidate, editable_paths, false).unwrap_or_else(|_| {
        let cleaned = candidate.trim().trim_matches('`').trim();
        let cleaned = cleaned.strip_prefix("./").unwrap_or(cleaned);
        cleaned.strip_prefix('/').unwrap_or(cleaned).to_string()
    })
}

/// Resolves a model-named path the streaming preview is about to read from disk.
///
/// The streaming filters resolve a path before the authoritative staging engine
/// validates the change set, and they read the source of the preview from the
/// working tree. The candidate therefore passes the repository boundary the engine
/// enforces: the decoration a model may wrap it in is stripped, `.` and `..`
/// collapse without climbing above the repository root, an absolute path is
/// refused, and a symbolic link that resolves outside the repository is refused. A
/// path the check refuses never reaches the file system, so a change that names a
/// location above the root cannot disclose a file outside the repository through a
/// streamed preview, even though the staging engine rejects that same change
/// afterwards.
pub(crate) fn resolve_preview_read_path(
    repo_root: &Path,
    candidate: &str,
) -> Result<String, String> {
    let cleaned = candidate.trim().trim_matches('`').trim();
    let cleaned = cleaned.strip_prefix("./").unwrap_or(cleaned);
    tauqe_tools::paths::normalize_repo_path(repo_root, cleaned).map_err(|reason| {
        format!("Path '{candidate}' cannot be read for a streaming preview: {reason}")
    })
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
    fn test_resolve_target_path_for_op_create_does_not_suffix_match() {
        let editable = vec!["crates/core/src/lib.rs".to_string()];
        let resolved = resolve_target_path_for_op("lib.rs", &editable, true).unwrap();
        assert_eq!(resolved, "lib.rs");
    }

    #[test]
    fn test_resolve_target_path_for_op_ambiguity_returns_err() {
        let editable = vec![
            "crates/a/src/lib.rs".to_string(),
            "crates/b/src/lib.rs".to_string(),
        ];
        let res = resolve_target_path_for_op("lib.rs", &editable, false);
        assert!(res.is_err());
    }

    #[test]
    fn test_normalize_content() {
        assert_eq!(normalize_content("\nhello\n   "), "hello\n");
        assert_eq!(normalize_content("\r\nhello"), "hello");
    }

    /// A previewed path keeps the segments that stay inside the repository and
    /// is refused when it names an absolute location or climbs above the root.
    #[test]
    fn preview_read_path_collapses_dot_segments_and_refuses_an_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert_eq!(
            resolve_preview_read_path(root, "./src/../lib.rs").unwrap(),
            "lib.rs"
        );
        assert!(resolve_preview_read_path(root, "/etc/passwd").is_err());
        assert!(resolve_preview_read_path(root, "../lib.rs").is_err());
    }
}
