use crate::context::ContextFileContent;

pub use tauqe_protocol::review::{
    extract_review_step_execution_id, extract_review_step_execution_target,
    format_review_discussion_prompt, format_review_discussion_prompt_items,
    format_review_step_execution_prompt, is_discussion_prompt, is_review_discussion_prompt,
    is_review_step_execution_prompt, ReviewStepTarget, REVIEW_DISCUSSION_PREFIX,
    REVIEW_STEP_EXECUTION_PREFIX,
};

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

    