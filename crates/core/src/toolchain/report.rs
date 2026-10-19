//! Compact semantic reporting for executed toolchain commands.
//!
//! A failed command is summarized rather than dumped: the journal records the
//! number of diagnostic `error` lines, a bounded head of those lines, and the
//! distinct source paths they reference. Raw build noise stays out of the log
//! while the summary stays analytically useful.

use super::ToolchainCheckResult;

/// Maximum number of diagnostic `error` lines captured in a compact failure summary.
const FAILURE_SUMMARY_MAX_ERROR_LINES: usize = 5;
/// Maximum number of distinct source paths captured in a compact failure summary.
const FAILURE_SUMMARY_MAX_PATHS: usize = 10;
/// Maximum characters captured from a single diagnostic line.
const FAILURE_SUMMARY_LINE_CHARS: usize = 200;

/// Records the outcome of a single command as a structured log record.
pub(super) fn log_toolchain_check_result(result: &ToolchainCheckResult, duration_ms: u64) {
    if result.is_cancelled {
        tracing::info!(
            command = %result.command,
            duration_ms,
            cancelled = true,
            "toolchain command cancelled"
        );
    } else if result.success {
        tracing::info!(
            command = %result.command,
            duration_ms,
            success = true,
            "toolchain command finished"
        );
    } else {
        tracing::warn!(
            command = %result.command,
            duration_ms,
            success = false,
            summary = %summarize_toolchain_failure(result),
            "toolchain command failed"
        );
    }
}

/// Builds a compact, UTF-8-safe summary of a failed command.
///
/// The summary records the number of diagnostic `error` lines, a small number of
/// leading error lines truncated on a Unicode scalar boundary, and the distinct
/// source paths referenced by the output. The full `combined_output` is never
/// embedded, so raw build noise and source fragments stay out of the log.
fn summarize_toolchain_failure(result: &ToolchainCheckResult) -> String {
    let output = if result.combined_output.trim().is_empty() {
        result.stderr.trim()
    } else {
        result.combined_output.trim()
    };

    let error_lines: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("error"))
        .collect();

    let mut summary = format!(
        "command='{}' error_lines={}",
        result.command,
        error_lines.len()
    );

    if error_lines.is_empty() {
        if let Some(first) = output.lines().find(|line| !line.trim().is_empty()) {
            summary.push_str("; first_line=");
            summary.push_str(&truncate_on_char_boundary(
                first.trim(),
                FAILURE_SUMMARY_LINE_CHARS,
            ));
        }
    } else {
        let head: Vec<String> = error_lines
            .iter()
            .take(FAILURE_SUMMARY_MAX_ERROR_LINES)
            .map(|line| truncate_on_char_boundary(line.trim(), FAILURE_SUMMARY_LINE_CHARS))
            .collect();
        summary.push_str("; first_errors=");
        summary.push_str(&head.join(" | "));
    }

    let paths = extract_failure_paths(output);
    if !paths.is_empty() {
        summary.push_str("; error_paths=");
        summary.push_str(&paths.join(","));
    }

    summary
}

/// Truncates a single line on a Unicode scalar boundary, appending an ellipsis when clipped.
fn truncate_on_char_boundary(line: &str, max_chars: usize) -> String {
    let mut truncated: String = line.chars().take(max_chars).collect();
    if line.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

/// Extracts distinct source paths referenced in compiler, linter, or test output.
fn extract_failure_paths(output: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();

    for line in output.lines() {
        for word in line.split_whitespace() {
            let trimmed = word.trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>'
                )
            });
            let candidate = trimmed.strip_prefix("./").unwrap_or(trimmed);
            let path_part = candidate.split(':').next().unwrap_or("");
            if is_source_path(path_part)
                && seen.insert(path_part.to_string())
                && paths.len() < FAILURE_SUMMARY_MAX_PATHS
            {
                paths.push(path_part.to_string());
            }
        }
    }

    paths
}

/// Reports whether a token looks like a repository-relative source path.
fn is_source_path(candidate: &str) -> bool {
    if candidate.is_empty() || !candidate.contains('/') {
        return false;
    }
    candidate.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext,
            "rs" | "go"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "py"
                | "rb"
                | "java"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "cxx"
                | "hpp"
                | "cs"
                | "kt"
                | "swift"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarize_toolchain_failure_is_compact_and_utf8_safe() {
        let output = "\
error: used `unwrap()` on an `Option` value
  --> crates/core/src/lib.rs:10:5
   |
10 |     let x = y.unwrap();
   |             ^^^^^^^^^^

error: mismatched types
  --> crates/core/src/toolchain.rs:20:9

error: aborting due to 2 previous errors
warning: unused import: `std::fmt`
";
        let result = ToolchainCheckResult {
            success: false,
            is_cancelled: false,
            command: "cargo check --all-targets".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: output.to_string(),
        };
        let summary = summarize_toolchain_failure(&result);
        assert!(summary.contains("error_lines=3"), "summary: {summary}");
        assert!(
            summary.contains("crates/core/src/lib.rs"),
            "summary: {summary}"
        );
        assert!(
            summary.contains("crates/core/src/toolchain.rs"),
            "summary: {summary}"
        );
        assert!(!summary.contains("unused import"), "summary: {summary}");
    }

    #[test]
    fn test_summarize_toolchain_failure_truncates_multibyte_on_boundary() {
        let long_line = "error: ".to_string() + &"ошибка ".repeat(100);
        let result = ToolchainCheckResult {
            success: false,
            is_cancelled: false,
            command: "cargo build".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            combined_output: long_line,
        };
        let summary = summarize_toolchain_failure(&result);
        assert!(summary.contains("first_errors="));
        assert!(summary.contains('…'));
    }
}
