use crate::context::ContextFileContent;
use tauqe_protocol::review::ReviewSession;

pub use tauqe_protocol::review::{
    format_review_discussion_prompt, format_review_discussion_prompt_items,
    REVIEW_DISCUSSION_PREFIX,
};

/// Returns true if the prompt represents an explicit review discussion turn.
pub fn is_review_discussion_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(REVIEW_DISCUSSION_PREFIX)
}

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
    prompt.push_str("Formulate the `summary` Markdown narrative and all finding titles and descriptions in the primary natural language established by the user's instructions or prompt, while keeping code identifiers, symbols, and keywords exact.\n\n");

    prompt.push_str("## Output Format Specification\n");
    prompt.push_str("You MUST respond with a single JSON object that conforms to the enforced structured output schema. Do not wrap the JSON in markdown code fences and do not add any prose outside the JSON object.\n\n");
    prompt.push_str("The object contains exactly two fields, emitted in this order:\n");
    prompt.push_str("- `summary`: a high-level conclusion formatted as Markdown. Before enumerating concrete findings, synthesize the overall assessment of architectural integrity, separation of concerns, concurrency and safety risks, error handling, and reliability across the reviewed files. Do not merely restate the findings list.\n");
    prompt.push_str("- `findings`: an array of finding objects with the following fields:\n");
    prompt.push_str("- `title`: concise finding title.\n");
    prompt.push_str("- `severity`: exactly one of the following string values:\n");
    prompt.push_str("  - `critical`: severe bug, panic, data corruption, memory leak, or security flaw.\n");
    prompt.push_str("  - `warning`: architecture violation, potential runtime issue, or poor error handling.\n");
    prompt.push_str("  - `suggestion`: code cleanup, optimization, or idiomatic improvement.\n");
    prompt.push_str("  - `info`: general observation or architectural insight.\n");
    prompt.push_str("- `file_path`: repository-relative path of the affected file, or null when the finding is not file-specific.\n");
    prompt.push_str("- `line_start` and `line_end`: 1-based start and end line of the affected range, or null when unknown.\n");
    prompt.push_str("- `body`: clear explanation of the problem, why it occurs, and concrete recommendations for how to resolve it.\n\n");

    prompt.push_str("If there are no issues, respond with an empty `findings` array.\n");
    prompt.push_str("Do NOT output markdown headers, diffs, or XML edit tags; this is an audit-only review.\n");

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

pub const REVIEW_STEP_EXECUTION_PREFIX: &str = "# Target Review Step Execution:";

/// Target review and item identifiers parsed from an isolated review step execution prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStepTarget {
    pub review_id: Option<String>,
    pub item_id: u32,
}

/// Returns true if the prompt represents an isolated review step execution turn.
pub fn is_review_step_execution_prompt(prompt: &str) -> bool {
    prompt
        .trim_start()
        .starts_with(REVIEW_STEP_EXECUTION_PREFIX)
}

/// Extracts the target review ID and item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_target(prompt: &str) -> Option<ReviewStepTarget> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with(REVIEW_STEP_EXECUTION_PREFIX) {
        return None;
    }
    let rest = trimmed
        .strip_prefix(REVIEW_STEP_EXECUTION_PREFIX)?
        .trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some((rev_id, item_id_str)) = spec.split_once(':') {
            let rev_id = rev_id.trim();
            let item_id: u32 = item_id_str.trim().parse().ok()?;
            return Some(ReviewStepTarget {
                review_id: if rev_id.is_empty() {
                    None
                } else {
                    Some(rev_id.to_string())
                },
                item_id,
            });
        } else if let Ok(item_id) = spec.parse::<u32>() {
            return Some(ReviewStepTarget {
                review_id: None,
                item_id,
            });
        }
    }
    None
}

/// Extracts the target item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_id(prompt: &str) -> Option<u32> {
    extract_review_step_execution_target(prompt).map(|t| t.item_id)
}

/// Formats a targeted prompt for resolving an isolated review finding.
///
/// This builder embeds the concrete edit-protocol markers and therefore lives in
/// the core engine beside the sibling plan step prompt rather than in the
/// dependency-light wire crate.
pub fn format_review_step_execution_prompt(
    session: &ReviewSession,
    item_id: u32,
    marker_suffix: &str,
) -> Result<String, String> {
    let item = session
        .items
        .iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| {
            format!(
                "Review item #{} not found in review session '{}'",
                item_id, session.id
            )
        })?;

    let mut out = String::new();
    let clean_title = item.title.replace(['\r', '\n'], " ").trim().to_string();
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        REVIEW_STEP_EXECUTION_PREFIX, session.id, item.id, clean_title
    ));

    let title = if session.title.trim().is_empty() {
        &session.id
    } else {
        session.title.trim()
    };
    out.push_str(&format!("## Review Session: {}\n", title));
    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", trimmed));
        }
    }

    out.push_str("### Target Finding to Resolve:\n");
    out.push_str(&format!("- **ID**: `#{}`\n", item.id));
    out.push_str(&format!("- **Severity**: {}\n", item.severity));
    out.push_str(&format!("- **Title**: {}\n", item.title));
    if let Some(path) = &item.file_path {
        if let Some((start, end)) = item.line_range {
            out.push_str(&format!("- **Location**: `{}:{}-{}`\n", path, start, end));
        } else {
            out.push_str(&format!("- **Location**: `{}`\n", path));
        }
    }
    if !item.body.trim().is_empty() {
        out.push_str(&format!(
            "- **Details / Recommendation**:\n{}\n",
            item.body.trim()
        ));
    }
    if let Some(notes) = &item.resolution_notes {
        let trimmed = notes.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "- **Solution Strategy (from discussion)**:\n{}\n",
                trimmed
            ));
        }
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on resolving review finding `#{}`.\n",
        item.id
    ));
    out.push_str("2. Do NOT touch unrelated code or address other findings outside of this scope.\n");
    out.push_str("3. Keep changes minimal, coherent, and verified with toolchain checks.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "5. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "6. If you encounter an insurmountable technical blocker, contradictory requirements, or need developer decisions, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
        marker_suffix
    ));

    Ok(out)
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

    #[test]
    fn test_format_review_step_execution_prompt() {
        use tauqe_protocol::review::{ReviewItem, ReviewItemStatus, ReviewSeverity};

        let session = ReviewSession {
            id: "rev-7".to_string(),
            title: "Performance Review".to_string(),
            created_at: 100,
            model: "model-x".to_string(),
            description: Some("Optimize string copies".to_string()),
            user_prompt: None,
            target_files: vec!["crates/tui/src/view.rs".to_string()],
            items: vec![ReviewItem {
                id: 3,
                title: "Excessive clone in hot path".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                file_path: Some("crates/tui/src/view.rs".to_string()),
                line_range: Some((100, 110)),
                resolution_notes: None,
                body: "Replace clone with reference borrowing".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_step_execution_prompt(&session, 3, "_TAG").unwrap();
        assert!(is_review_step_execution_prompt(&prompt));
        assert!(prompt.contains(
            "# Target Review Step Execution: [rev-7:3] Excessive clone in hot path"
        ));
        assert!(prompt.contains("<success_TAG summary=\"...\" />"));
        assert!(prompt.contains("<error_TAG message=\"...\" />"));

        let target = extract_review_step_execution_target(&prompt).unwrap();
        assert_eq!(target.review_id.as_deref(), Some("rev-7"));
        assert_eq!(target.item_id, 3);
        assert_eq!(extract_review_step_execution_id(&prompt), Some(3));
    }

    #[test]
    fn test_build_review_system_prompt_requires_structured_output() {
        let prompt = build_review_system_prompt();
        assert!(prompt.contains("structured output schema"));
        assert!(prompt.contains("`summary`"));
        assert!(prompt.contains("`findings`"));
        assert!(prompt.contains("`severity`"));
        assert!(!prompt.contains("## [SEVERITY]"));

        let summary_idx = prompt.find("`summary`").expect("summary documented");
        let findings_idx = prompt.find("`findings`").expect("findings documented");
        assert!(
            summary_idx < findings_idx,
            "summary must be specified before findings"
        );
    }
}
