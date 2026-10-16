use crate::context::ContextFileContent;
use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use tauqe_protocol::review::{ReviewItem, ReviewSession};
use tauqe_protocol::PlanItem;

pub const REVIEW_DISCUSSION_PREFIX: &str = "# Review Discussion:";

/// Returns true if the prompt represents an explicit review discussion turn.
pub fn is_review_discussion_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(REVIEW_DISCUSSION_PREFIX)
}

/// Formats a targeted prompt for discussing and analyzing review findings in Structured Output mode.
pub fn format_review_discussion_prompt(
    session: &ReviewSession,
    focused_item_id: Option<u32>,
    user_comment: &str,
) -> String {
    let ids: Vec<u32> = focused_item_id.into_iter().collect();
    format_review_discussion_prompt_items(session, &ids, user_comment)
}

/// Formats a targeted prompt for discussing multiple focused review findings in Structured Output mode.
pub fn format_review_discussion_prompt_items(
    session: &ReviewSession,
    focused_item_ids: &[u32],
    user_comment: &str,
) -> String {
    let mut out = String::new();
    let clean_title = single_line(&session.title);
    let title = if clean_title.is_empty() {
        session.id.as_str()
    } else {
        clean_title.as_str()
    };
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        REVIEW_DISCUSSION_PREFIX,
        single_line(&session.id),
        title
    ));

    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Review Scope & Description:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
        }
    }

    out.push_str("## Target Review Session Context:\n");
    let (total, fixed) = session.stats();
    out.push_str(&format!(
        "<active_review id=\"{}\" title=\"{}\" progress=\"{}/{} fixed\">\n",
        escape_xml_attr(&session.id),
        escape_xml_attr(title),
        fixed,
        total
    ));

    for item in &session.items {
        let is_focused = focused_item_ids.contains(&item.id);
        let focus_mark = if is_focused { " [FOCUSED]" } else { "" };
        let loc = if item.files.is_empty() {
            String::new()
        } else {
            let joined = item
                .files
                .iter()
                .map(|f| escape_xml_text(f))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" ({})", joined)
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}{}\n",
            item.id,
            item.severity,
            escape_xml_text(&item.title),
            loc,
            focus_mark
        ));
    }
    out.push_str("</active_review>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = session.find_item(*id) {
                out.push_str(&format!(
                    "- **#{} [{}] {}** (status: {})\n",
                    item.id,
                    item.severity,
                    escape_xml_text(&item.title),
                    item.status
                ));
                if !item.files.is_empty() {
                    let joined = item
                        .files
                        .iter()
                        .map(|f| format!("`{}`", escape_xml_text(f)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  Files: {}\n", joined));
                }
                if let Some(notes) = &item.resolution_notes {
                    if !notes.trim().is_empty() {
                        out.push_str(&format!(
                            "  Resolution Strategy: {}\n",
                            escape_xml_text(notes.trim())
                        ));
                    }
                }
                if !item.body.trim().is_empty() {
                    out.push_str(&format!("  Details: {}\n", escape_xml_text(item.body.trim())));
                }
                if item.has_subplan() {
                    out.push_str("  Subplan:\n");
                    fn render_subplan_discussion(
                        out: &mut String,
                        items: &[PlanItem],
                        depth: usize,
                    ) {
                        let indent = "  ".repeat(depth + 2);
                        for sub in items {
                            out.push_str(&format!(
                                "{}- #{}: {} ({})\n",
                                indent,
                                sub.id,
                                escape_xml_text(&sub.title),
                                sub.status
                            ));
                            if !sub.children.is_empty() {
                                render_subplan_discussion(out, &sub.children, depth + 1);
                            }
                        }
                    }
                    render_subplan_discussion(&mut out, &item.subplan, 0);
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the code review findings.\n");
    out.push_str("2. Discuss the problem, edge cases, and potential solutions conceptually. Do NOT emit file edits or code patches in this discussion turn.\n");
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications clearly in conversational text in `message`; the developer applies status changes through the review interface.\n");
    out.push_str("4. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
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
    prompt.push_str("- `files`: array of repository-relative file paths affected by this finding (use `[]` when not file-specific).\n");
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

pub const REVIEW_STEP_RETRY_PREFIX: &str = "# Correction Required: Review Finding Turn Incomplete";

/// Builds a corrective reprompt for a review finding turn that ended without
/// valid code edits and without a terminal success tag.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback (such as a missing edit block or toolchain verification failure),
/// then explicitly demands strict adherence to the active edit protocol on the
/// next turn.
pub fn format_review_step_retry_prompt(assistant_text: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(REVIEW_STEP_RETRY_PREFIX);
    out.push_str("\n\n");
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
    let trimmed = prompt.trim_start();
    trimmed.starts_with(REVIEW_STEP_EXECUTION_PREFIX)
        || trimmed.starts_with(REVIEW_STEP_RETRY_PREFIX)
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
    if !item.files.is_empty() {
        let joined = item
            .files
            .iter()
            .map(|f| format!("`{}`", f))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("- **Files**: {}\n", joined));
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
    if item.has_subplan() {
        out.push_str("### Decomposed Subplan Steps:\n");
        fn render_subplan_exec(out: &mut String, items: &[PlanItem], depth: usize) {
            let indent = "  ".repeat(depth);
            for sub in items {
                out.push_str(&format!(
                    "{}- [{}] #{} {}",
                    indent,
                    sub.status,
                    sub.id,
                    sub.title
                ));
                if !sub.files.is_empty() {
                    out.push_str(&format!(" (files: {})", sub.files.join(", ")));
                }
                if !sub.readonly_files.is_empty() {
                    out.push_str(&format!(" (readonly: {})", sub.readonly_files.join(", ")));
                }
                if let Some(details) = &sub.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                if !sub.children.is_empty() {
                    render_subplan_exec(out, &sub.children, depth + 1);
                }
            }
        }
        render_subplan_exec(&mut out, &item.subplan, 0);
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on resolving review finding `#{}`.\n",
        item.id
    ));
    out.push_str("2. Do NOT touch unrelated code or address other findings outside of this scope.\n");
    out.push_str("3. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "5. If the finding is already resolved by earlier changes, is a false positive, or requires no code modifications, do NOT emit unnecessary edits; instead emit <skip{0} reason=\"...\">Detailed explanation</skip{0}>.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "6. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));
    out.push_str(&format!(
        "7. If you encounter an insurmountable technical blocker, contradictory requirements, or need developer decisions, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
        marker_suffix
    ));

    Ok(out)
}

/// Extracts a `<skip>` reason and optional body from an assistant response.
///
/// Recognizes both turn-marker-bound tags (`<skip_MARKER ...>`) and unmarked
/// `<skip ...>` tags, combining the `reason` attribute and inner text when both
/// are present.
pub fn extract_review_skip_reason(text: &str, marker: Option<&str>) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(m) = marker {
        let trimmed = m.trim();
        if !trimmed.is_empty() {
            candidates.push(format!("skip_{}", trimmed));
        }
    }
    candidates.push("skip".to_string());

    for tag_name in &candidates {
        let open_prefix = format!("<{}", tag_name);
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(&open_prefix) {
            let start = search_from + rel;
            let after_name = start + open_prefix.len();
            let next_ch = text[after_name..].chars().next();
            if !matches!(next_ch, Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('>') | Some('/')) {
                search_from = after_name;
                continue;
            }
            let Some(gt_rel) = text[after_name..].find('>') else {
                break;
            };
            let gt_pos = after_name + gt_rel;
            let header = &text[after_name..gt_pos];
            let self_closing = header.trim_end().ends_with('/');
            let reason_attr = extract_reason_attr(header);

            let body = if self_closing {
                String::new()
            } else {
                let close_tag = format!("</{}>", tag_name);
                if let Some(close_rel) = text[gt_pos + 1..].find(&close_tag) {
                    text[gt_pos + 1..gt_pos + 1 + close_rel].trim().to_string()
                } else {
                    String::new()
                }
            };

            let combined = match (reason_attr, body.is_empty()) {
                (Some(r), true) => r,
                (Some(r), false) if r == body => r,
                (Some(r), false) => format!("{}: {}", r, body),
                (None, false) => body,
                (None, true) => "Skipped without additional details".to_string(),
            };
            return Some(combined);
        }
    }
    None
}

fn extract_reason_attr(header: &str) -> Option<String> {
    let idx = header.find("reason")?;
    let rest = header[idx + "reason".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after_quote = &rest[quote.len_utf8()..];
    let end = after_quote.find(quote)?;
    let val = after_quote[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Target size range (in changed lines) for one cohesive leaf implementation step.
const LEAF_STEP_TARGET_LINES: (usize, usize) = (150, 400);

/// Maximum supported hierarchy nesting depth for structured plan refinement output.
pub const MAX_REVIEW_SUBPLAN_DEPTH: usize = 5;

/// Builds the system prompt for refining a review finding into an actionable subplan.
///
/// Dispatched to the Architect role to decompose an audit finding into concrete,
/// compilable leaf steps conforming to the plan engine's execution invariants.
pub fn build_review_finding_refine_system_prompt() -> String {
    let mut out = String::new();
    out.push_str("You are a Principal Software Architect in TAUQE (The Answer to the Ultimate Question of Engineering).\n\n");
    out.push_str("Your mission is Review Finding Decomposition: analyzing an analytical code review finding and synthesizing an actionable, verifiable step plan (subplan) that completely resolves the identified architectural, safety, or correctness issue.\n\n");

    out.push_str("## Core Invariants & Rules:\n\n");
    out.push_str("1. Finding Remediation Mandate (\"What\" vs \"How\"):\n");
    out.push_str("   - The review finding describes the deficiency, safety hazard, or architectural problem to fix.\n");
    out.push_str("   - Your exclusive responsibility is engineering decomposition (\"How\"): the exact modules, interfaces, contracts, types, and sequence of implementation required to resolve it.\n\n");

    out.push_str("2. Subplan Synthesis Contract:\n");
    out.push_str("   - Return the hierarchical list of executable implementation steps with `status: \"todo\"`.\n");
    out.push_str("   - Synthesize a coherent roadmap that completely resolves the finding without breaking existing subsystem invariants.\n\n");

    out.push_str("3. Strict Blocker Protocol (Refusal to Plan):\n");
    out.push_str("   - If you discover that the finding is a false positive, contradicts architectural invariants, or cannot be resolved without developer guidance:\n");
    out.push_str("   - You MUST set `status: \"blocked\"`, provide empty `items: []`, and document a comprehensive technical explanation in `reason`.\n\n");

    out.push_str("4. Step Granularity and Compilability Invariants:\n");
    out.push_str("   - Every leaf step is executed in an isolated turn with its own toolchain verification gates.\n");
    out.push_str("   - Leaf-Only Files Invariant: For every executable leaf step, you MUST populate `files: [\"path/to/file\"]` with exact repository-relative file paths. Conceptual container items MUST have empty `files: []`.\n");
    out.push_str("   - Reference Files Contract: Populate `readonly_files: [\"path/to/reference\"]` with contracts, conventions, or types needed as read-only context during step execution.\n");
    out.push_str("   - Avoid Micro-Fragmentation: Group cohesive changes across 1-4 related files under a single leaf step if they form a logical unit of work.\n");
    out.push_str(&format!(
        "   - Target leaf step size: calibrated to {}-{} lines of change.\n",
        LEAF_STEP_TARGET_LINES.0, LEAF_STEP_TARGET_LINES.1
    ));
    out.push_str("   - Compilability Invariant: The codebase MUST remain in a compiling, fully verified state at the end of EVERY leaf step.\n");
    out.push_str("   - Order steps strictly by prerequisite dependencies (`depends_on`).\n\n");

    out.push_str("5. Strict Language Consistency Invariant:\n");
    out.push_str("   - Detect the primary natural language used in the review finding's title and description.\n");
    out.push_str("   - You MUST formulate all titles, step details, explanations, and reasons strictly in that SAME natural language.\n\n");

    out.push_str("6. Codebase Exploration & Discovery Contract:\n");
    out.push_str("   - You have access to symbol contours in `<repo_map>` and already loaded files in `Active Context Files`.\n");
    out.push_str("   - If you need to inspect exact implementations before synthesizing the subplan, set `status: \"needs_context\"`, specify repository-relative file paths in `context_requests: [...]`, and leave `items: []`.\n\n");

    out.push_str("7. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema.\n");
    out
}

/// Alias for `build_review_finding_refine_system_prompt`.
pub fn build_review_refine_system_prompt() -> String {
    build_review_finding_refine_system_prompt()
}

/// Builds the user prompt for refining a single review finding into a subplan.
pub fn build_review_finding_refine_user_prompt(
    session: &ReviewSession,
    item: &ReviewItem,
    context_files: &[ContextFileContent],
    repo_map: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Review Finding Refinement: [{}#{}] {}\n\n",
        session.id, item.id, item.title
    ));

    out.push_str("## Target Review Session:\n");
    let clean_title = single_line(&session.title);
    let title = if clean_title.is_empty() {
        &session.id
    } else {
        &clean_title
    };
    out.push_str(&format!("- **Session**: {} ({})\n", title, session.id));
    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Scope / Summary**: {}\n", trimmed));
        }
    }
    out.push('\n');

    out.push_str("## Target Finding to Decompose:\n");
    out.push_str(&format!("- **ID**: `#{}`\n", item.id));
    out.push_str(&format!("- **Severity**: {}\n", item.severity));
    out.push_str(&format!("- **Title**: {}\n", item.title));
    if !item.files.is_empty() {
        out.push_str(&format!(
            "- **Target Files**: {}\n",
            item.files.join(", ")
        ));
    }
    if !item.body.trim().is_empty() {
        out.push_str(&format!("- **Details / Recommendation**:\n{}\n", item.body.trim()));
    }
    if let Some(notes) = &item.resolution_notes {
        let trimmed = notes.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Resolution Notes**:\n{}\n", trimmed));
        }
    }
    if item.has_subplan() {
        out.push_str("- **Existing Subplan Steps**:\n");
        for sub in &item.subplan {
            out.push_str(&format!("  - [{}] #{} {}\n", sub.status, sub.id, sub.title));
        }
    }
    out.push('\n');

    if let Some(rm) = repo_map {
        let trimmed = rm.trim();
        if !trimmed.is_empty() {
            out.push_str("## Codebase Symbol Contours (<repo_map>):\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    if !context_files.is_empty() {
        out.push_str("## Active Context Files:\n");
        for file in context_files {
            out.push_str(&format!("### File: `{}`\n```\n{}\n```\n\n", file.path, file.content));
        }
    }

    if let Some(ins) = instructions {
        let trimmed = ins.trim();
        if !trimmed.is_empty() {
            out.push_str("## Developer Directives:\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    out.push_str("## Architect Instructions:\n");
    out.push_str("1. Analyze the review finding and determine the necessary refactorings and code changes.\n");
    out.push_str("2. Synthesize a decomposed, dependency-ordered subplan of leaf steps with `status: \"todo\"`.\n");
    out.push_str("3. For each leaf step, declare `files`, `readonly_files`, and `depends_on`. Ensure each step leaves the codebase in a compiling state.\n");
    out.push_str("4. If the finding is blocked or cannot be implemented, report `status: \"blocked\"` with `reason`.\n");
    out.push_str("5. If you need more source files to verify interfaces, report `status: \"needs_context\"` with `context_requests`.\n");
    out.push_str("6. Language Directive: Formulate all titles, details, explanations, and reasons strictly in the same natural language as the finding.\n");

    out
}

/// Alias for `build_review_finding_refine_user_prompt`.
pub fn build_review_refine_user_prompt(
    session: &ReviewSession,
    item: &ReviewItem,
    context_files: &[ContextFileContent],
    repo_map: Option<&str>,
    instructions: Option<&str>,
) -> String {
    build_review_finding_refine_user_prompt(session, item, context_files, repo_map, instructions)
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
        assert!(is_review_step_execution_prompt(&prompt));
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
    fn test_format_review_step_execution_prompt_with_subplan() {
        use tauqe_protocol::review::{ReviewItem, ReviewItemStatus, ReviewSeverity};
        use tauqe_protocol::PlanItemStatus;

        let session = ReviewSession {
            id: "rev-sub".to_string(),
            title: "Subplan Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: None,
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Refactor architecture".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: vec!["crates/core/src/lib.rs".to_string()],
                subplan: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Split module".to_string(),
                    details: Some("Create submodules".to_string()),
                    status: PlanItemStatus::Done,
                    concept_refs: vec![],
                    files: vec!["crates/core/src/sub.rs".to_string()],
                    readonly_files: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
                resolution_notes: None,
                body: "Break down large file".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_step_execution_prompt(&session, 1, "_TAG").unwrap();
        assert!(prompt.contains("Decomposed Subplan Steps:"));
        assert!(prompt.contains("- [DONE] #1.1 Split module (files: crates/core/src/sub.rs): Create submodules"));
    }

    #[test]
    fn test_build_review_finding_refine_prompts() {
        use tauqe_protocol::review::{ReviewItem, ReviewItemStatus, ReviewSeverity};

        let session = ReviewSession {
            id: "rev-ref".to_string(),
            title: "Refinement Session".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: Some("Audit findings to refine".to_string()),
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 42,
                title: "Decompose this issue".to_string(),
                model: None,
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                files: vec!["crates/core/src/lib.rs".to_string()],
                subplan: vec![],
                resolution_notes: Some("Break into sub-steps".to_string()),
                body: "Needs structural division".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let sys = build_review_finding_refine_system_prompt();
        assert!(sys.contains("Review Finding Decomposition"));
        assert!(sys.contains("Compilability Invariants"));

        let user = build_review_finding_refine_user_prompt(
            &session,
            &session.items[0],
            &[],
            Some("fn outline()"),
            Some("Focus on safety"),
        );
        assert!(user.contains("# Review Finding Refinement: [rev-ref#42] Decompose this issue"));
        assert!(user.contains("Focus on safety"));
        assert!(user.contains("fn outline()"));
        assert!(user.contains("Break into sub-steps"));
    }

    #[test]
    fn test_format_review_step_execution_prompt() {
        use tauqe_protocol::review::{ReviewItem, ReviewItemStatus, ReviewSeverity};

        let session = ReviewSession {
            id: "rev-7".to_string(),
            title: "Performance Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("model-x"),
            description: Some("Optimize string copies".to_string()),
            user_prompt: None,
            target_files: vec!["crates/tui/src/view.rs".to_string()],
            items: vec![ReviewItem {
                id: 3,
                title: "Excessive clone in hot path".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: vec!["crates/tui/src/view.rs".to_string()],
                subplan: Vec::new(),
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
    fn test_review_discussion_prompt_formatting() {
        use tauqe_protocol::review::{ReviewItem, ReviewItemStatus, ReviewSeverity};
        use tauqe_protocol::ModelRef;

        let session = ReviewSession {
            id: "rev-42".to_string(),
            title: "Security Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("test-model"),
            description: Some("Security review for core crate".to_string()),
            user_prompt: None,
            target_files: vec!["src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Race condition in cache".to_string(),
                model: None,
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                files: vec!["src/lib.rs".to_string()],
                subplan: Vec::new(),
                resolution_notes: Some("Extend scope of mutex guard".to_string()),
                body: "Lock is dropped prematurely.".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt =
            format_review_discussion_prompt(&session, Some(1), "Is this really possible?");
        assert!(prompt.contains("# Review Discussion: [rev-42] Security Audit"));
        assert!(prompt.contains("Race condition in cache"));
        assert!(prompt.contains("Resolution Strategy: Extend scope of mutex guard"));
        assert!(prompt.contains("Is this really possible?"));
        assert!(prompt.contains("explain the proposed modifications clearly in conversational text"));
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
