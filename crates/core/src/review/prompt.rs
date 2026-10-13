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

/// Builds a corrective reprompt for a review finding turn that ended without
/// valid code edits and without a terminal success tag.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback (such as a missing edit block or toolchain verification failure),
/// then explicitly demands strict adherence to the active edit protocol on the
/// next turn.
pub fn format_review_step_retry_prompt(assistant_text: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("# Correction Required: Review Finding Turn Incomplete\n\n");
    out.push_str("Your previous turn ended without applying any verified code changes and without emitting the terminal success tag (`<success>`). This violates the execution protocol.\n\n");

    out.push_str("## Previous Assistant Response:\n");
    let previous = assistant_text.trim();
    if previous.is_empty() {
        out.push_str("(empty response)\n\n");
    } else {
        out.push_str("<previous_response>\n");
        out.push_str(previous);
        out.push_str("\n</previous_response>\n\n");
    }

    if let Some(err) = error {
        let trimmed = err.trim();
        if !trimmed.is_empty() {
            out.push_str("## Diagnostic Feedback:\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    out.push_str("## Required Actions:\n");
    out.push_str("1. If the finding requires code changes, emit a valid edit block that strictly conforms to the active edit protocol.\n");
    out.push_str("2. If no code changes are required to resolve the finding, confirm completion explicitly by emitting the terminal success tag.\n");
    out.push_str("3. Do NOT replace protocol operations with conversational text, and do NOT end the turn without either a valid edit block or the terminal success tag.\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_review_step_retry_prompt() {
        let prompt = format_review_step_retry_prompt(
            "I inspected the file and it looks correct.",
            Some("No edits were applied"),
        );
        assert!(prompt.contains("# Correction Required: Review Finding Turn Incomplete"));
        assert!(prompt.contains("<previous_response>"));
        assert!(prompt.contains("I inspected the file and it looks correct."));
        assert!(prompt.contains("## Diagnostic Feedback:"));
        assert!(prompt.contains("No edits were applied"));
        assert!(prompt.contains("emit a valid edit block"));
        assert!(prompt.contains("terminal success tag"));

        let empty = format_review_step_retry_prompt("   ", None);
        assert!(empty.contains("(empty response)"));
        assert!(!empty.contains("<previous_response>"));
        assert!(!empty.contains("Diagnostic Feedback"));
    }
}

    