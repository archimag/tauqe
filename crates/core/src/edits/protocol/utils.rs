use super::ContextRequest;
use tauqe_protocol::ContextAccess;

/// Parses a structured `context_requests` entry: `path` (read-only),
/// `read_only:path` or `editable:path`. Returns None for empty paths.
pub fn parse_context_request_spec(spec: &str) -> Option<ContextRequest> {
    let spec = spec.trim();
    let (access, rest) = if let Some(rest) = spec.strip_prefix("editable:") {
        (ContextAccess::Editable, rest)
    } else if let Some(rest) = spec.strip_prefix("read_only:") {
        (ContextAccess::ReadOnly, rest)
    } else {
        (ContextAccess::ReadOnly, spec)
    };
    let cleaned = rest.trim().trim_matches('`').trim();
    let cleaned = cleaned.strip_prefix("./").unwrap_or(cleaned);
    let cleaned = cleaned.strip_prefix('/').unwrap_or(cleaned);
    if cleaned.is_empty() {
        None
    } else {
        Some(ContextRequest {
            path: cleaned.to_string(),
            access,
        })
    }
}

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
    fn test_parse_context_request_spec() {
        let ro = parse_context_request_spec(" ./src/a.rs ").unwrap();
        assert_eq!(ro.path, "src/a.rs");
        assert_eq!(ro.access, ContextAccess::ReadOnly);
        let ed = parse_context_request_spec("editable:src/b.rs").unwrap();
        assert_eq!(ed.path, "src/b.rs");
        assert_eq!(ed.access, ContextAccess::Editable);
        assert!(parse_context_request_spec("editable:").is_none());
    }

    #[test]
    fn test_resolve_target_path_for_op_create_does_not_suffix_match() {
        let editable = vec!["crates/core/src/lib.rs".to_string()];
        let resolved = resolve_target_path_for_op("lib.rs", &editable, true).unwrap();
        assert_eq!(resolved, "lib.rs");
    }

    #[test]
    fn test_resolve_target_path_for_op_ambiguity_returns_err() {
        let editable = vec!["crates/a/src/lib.rs".to_string(), "crates/b/src/lib.rs".to_string()];
        let res = resolve_target_path_for_op("lib.rs", &editable, false);
        assert!(res.is_err());
    }

    #[test]
    fn test_normalize_content() {
        assert_eq!(normalize_content("\nhello\n   "), "hello\n");
        assert_eq!(normalize_content("\r\nhello"), "hello");
    }
}
