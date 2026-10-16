use crate::context::ContextFileContent;
use crate::escape::single_line;
use tauqe_protocol::review::{ReviewItem, ReviewSession};

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
