/// Extracts repository file paths mentioned in compilation, linter, or test failure output.
pub fn extract_error_paths(report: &str, available_files: &[String]) -> Vec<String> {
    let available_set: std::collections::HashSet<&str> =
        available_files.iter().map(|s| s.as_str()).collect();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in report.lines() {
        for word in line.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>'
                )
            });
            let candidate = trimmed.strip_prefix("./").unwrap_or(trimmed);
            let path_part = candidate.split(':').next().unwrap_or("");
            if !path_part.is_empty()
                && available_set.contains(path_part)
                && seen.insert(path_part.to_string())
            {
                found.push(path_part.to_string());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_error_paths_from_clippy_report() {
        let report = r#"
error: used `unwrap()` on an `Option` value
  --> crates/protocol/src/context.rs:88:20
   |
88 |         let file = self.files.get(path).unwrap();
   |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error
"#;
        let available = vec![
            "crates/protocol/src/context.rs".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/protocol/src/context.rs"]);
    }

    #[test]
    fn test_extract_error_paths_ignores_unknown_files() {
        let report = "--> unknown/file.rs:10:5\n  crates/core/src/lib.rs:20:1";
        let available = vec!["crates/core/src/lib.rs".to_string()];
        let paths = extract_error_paths(report, &available);
        assert_eq!(paths, vec!["crates/core/src/lib.rs"]);
    }
}
