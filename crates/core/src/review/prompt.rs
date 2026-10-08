use crate::context::ContextFileContent;
use tauqe_protocol::ReviewItem;

/// Builds the system prompt for the isolated code review session.
/// Focuses purely on architectural soundness, safety, edge cases, and performance.
pub fn build_review_system_prompt() -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an expert Principal Systems Architect and Code Reviewer.\n\n");
    prompt.push_str("## Mission\n");
    prompt.push_str("Your task is to conduct an in-depth, rigorous, and actionable code review of the provided codebase files.\n");
    prompt.push_str("Focus on:\n");
    prompt.push_str("1. Architectural integrity, separation of concerns, and cohesion.\n");
    prompt.push_str("2. Concurrency, data races, deadlocks, resource leaks, and panic safety.\n");
    prompt.push_str("3. Edge-case and error handling robustness.\n");
    prompt.push_str("4. Performance bottlenecks and unnecessary allocations.\n");
    prompt.push_str("5. Idiomatic conventions and testability.\n\n");

    prompt.push_str("## Language Invariant\n");
    prompt.push_str("Formulate finding titles, descriptions, and actionable advice in the primary natural language established by the user's instructions or prompt, while keeping code identifiers, symbols, and keywords exact.\n\n");

    prompt.push_str("## Output Format Specification\n");
    prompt.push_str("You MUST format each finding as a structured section with at most 2 levels of hierarchy:\n\n");
    prompt.push_str("## [SEVERITY] Concise Finding Title\n");
    prompt.push_str("- **File**: relative/path/to/file.rs:start_line-end_line\n");
    prompt.push_str("- **Details**:\n");
    prompt.push_str("Clear explanation of the problem, why it occurs, and concrete recommendations for how to resolve it.\n\n");

    prompt.push_str("SEVERITY must be exactly one of:\n");
    prompt.push_str("- `CRITICAL`: Severe bug, panic, data corruption, memory leak, or security flaw.\n");
    prompt.push_str("- `WARNING`: Architecture violation, potential runtime issue, or poor error handling.\n");
    prompt.push_str("- `SUGGESTION`: Code cleanup, optimization, or idiomatic improvement.\n");
    prompt.push_str("- `INFO`: General observation or architectural insight.\n\n");

    prompt.push_str("If there are no critical issues, state that clearly and focus on architectural suggestions.\n");
    prompt.push_str("Do NOT output diffs or XML edit tags; this is an audit-only review.\n");

    prompt
}

/// Builds the user prompt containing pure context files without history or repo map.
pub fn build_review_user_prompt(
    files: &[ContextFileContent],
    custom_prompt: Option<&str>,
) -> String {
    let mut out = String::new();

    if let Some(user_req) = custom_prompt {
        let trimmed = user_req.trim();
        if !trimmed.is_empty() {
            out.push_str("## Review Focus & User Instructions\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    out.push_str("## Code Files Under Review\n\n");
    if files.is_empty() {
        out.push_str("No files provided in context.\n");
    } else {
        for f in files {
            out.push_str(&format!("<file path=\"{}\">\n", f.path));
            out.push_str(&f.content);
            if !f.content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</file>\n\n");
        }
    }

    out.push_str("Please provide your structured code review findings following the requested specification.\n");
    out
}

/// Formats active (checked) review findings for injection into the Develop prompt.
pub fn format_active_review_findings(items: &[ReviewItem]) -> Option<String> {
    let checked: Vec<&ReviewItem> = items.iter().filter(|i| i.is_checked).collect();
    if checked.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("<active_review_findings>\n");
    out.push_str("The developer selected the following code review findings to address in this turn. Focus on resolving them cleanly:\n\n");

    for item in checked {
        let file_info = match (&item.file_path, item.line_range) {
            (Some(path), Some((start, end))) if start == end => format!(" ({}:{})", path, start),
            (Some(path), Some((start, end))) => format!(" ({}:{}-{})", path, start, end),
            (Some(path), None) => format!(" ({})", path),
            (None, _) => String::new(),
        };

        out.push_str(&format!(
            "- [{:?}] {}{}:\n",
            item.severity, item.title, file_info
        ));

        for line in item.body.lines() {
            out.push_str(&format!("  {}\n", line));
        }
        out.push('\n');
    }

    out.push_str("</active_review_findings>");
    Some(out)
}
