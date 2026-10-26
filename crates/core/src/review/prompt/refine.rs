use crate::context::ContextFileContent;
use crate::escape::single_line;
use crate::prompt::format_repository_outline_section;
use tauqe_protocol::review::{ReviewIssue, ReviewItem, ReviewSession};
use tauqe_protocol::PlanItem;

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
    out.push_str("   - Return ONLY the unfinished steps: already completed (DONE) sub-steps are preserved verbatim by the harness, so restating them is a contract violation.\n");
    out.push_str("   - When failure diagnostics of an already executed sub-step are provided, reproject the remaining horizon around that failure instead of repeating the failed approach.\n");
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

    out.push_str("6. Architect Discovery Protocol (Mandatory Contract Verification):\n");
    out.push_str("   - You have access to symbol contours in `<repo_map>` and already loaded files in `Active Context Files`.\n");
    out.push_str("   - Eager contour generation may be disabled, so `<repo_map>` can cover only part of the codebase. A module absent from the contours is never evidence that it is empty: request its contours via `repo_maps` before designing a step that touches it.\n");
    out.push_str("   - RepoMap contours are blinkers, not contracts: they reveal only the shape of the codebase (names, kinds, and rough signatures), never exact type definitions, field layouts, trait bounds, error semantics, lifetimes, or crate boundaries.\n");
    out.push_str("   - Whenever a leaf step you synthesize must interact with an existing interface, struct, enum, function, trait, module, or external crate, you MUST NOT infer its contract from contours alone. You MUST set `status: \"needs_context\"`, request the concrete source files in `context_requests: [...]`, execute searches via `repo_searches: [...]`, or request Tree-sitter symbol contours of a specific subsystem via `repo_maps: [\"crates/core/src/review\"]`, and leave `items: []`.\n");
    out.push_str("   - Contour requests (`repo_maps`) accept a repository-relative directory or file prefix; an empty string or `.` names the whole repository. They reveal module shape only, so an exact contract still has to be verified through `context_requests` or `repo_searches`.\n");
    out.push_str("   - Repository Search Syntax: searches use Rust / ripgrep regular expressions (lookaround assertions and backreferences are not supported).\n");
    out.push_str("   - Never emit speculative field names, argument types, return types, generic parameters, or method signatures derived from a contour line. A guessed contract is a hard contract violation.\n");
    out.push_str("   - Do not re-request files already present in `Active Context Files`; treat them as verified context.\n\n");

    out.push_str("7. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema.\n");
    out.push_str("   - Final Content Invariant: every text field (explanation, titles, details, reason) carries only the final distilled result. The reasoning process itself is never plan content: do not record deliberation, self-questioning, exploratory analysis, or intermediate reasoning steps in any field.\n");
    out
}

/// Alias for `build_review_finding_refine_system_prompt`.
pub fn build_review_refine_system_prompt() -> String {
    build_review_finding_refine_system_prompt()
}

/// Diagnostics describing a failed sub-step that triggered horizon replanning.
///
/// Replanning a finding is the same Architect operation as its initial
/// decomposition: both reproject the unfinished horizon from the immutable `Done`
/// foundation. The failure diagnostics are therefore carried into the same
/// prompt instead of a parallel template, so a single contract governs both.
#[derive(Debug, Clone, Copy)]
pub struct ReviewRefineDiagnostics<'a> {
    /// Identifier of the sub-step whose execution failed.
    pub failed_step_id: &'a str,
    /// Diagnostic reported by the failed execution, when one is available.
    pub failure_reason: Option<&'a str>,
}

/// Resolves a sub-step by id inside the finding's embedded subplan.
fn find_subplan_item<'a>(items: &'a [PlanItem], step_id: &str) -> Option<&'a PlanItem> {
    for item in items {
        if item.id == step_id {
            return Some(item);
        }
        if let Some(found) = find_subplan_item(&item.children, step_id) {
            return Some(found);
        }
    }
    None
}

/// Builds the user prompt for refining a single review finding into a subplan.
pub fn build_review_finding_refine_user_prompt(
    session: &ReviewSession,
    item: &ReviewItem,
    context_files: &[ContextFileContent],
    repo_map: Option<&str>,
    repo_dirs: Option<&str>,
    diagnostics: Option<ReviewRefineDiagnostics<'_>>,
    instructions: Option<&str>,
) -> String {
    // The refinement workspace renders the enriched finding view: the
    // description states the defect, the recommendations carry the remediation
    // strategy, and the protected contracts define what the fix must preserve.
    let issue = ReviewIssue::from(item.clone());

    let mut out = String::new();
    out.push_str(&format!(
        "# Review Finding Refinement: [{}#{}] {}\n\n",
        session.id, issue.id, issue.title
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
    out.push_str(&format!("- **ID**: `#{}`\n", issue.id));
    out.push_str(&format!("- **Type**: {}\n", issue.issue_type));
    out.push_str(&format!("- **Title**: {}\n", issue.title));
    if !issue.files.is_empty() {
        out.push_str(&format!("- **Target Files**: {}\n", issue.files.join(", ")));
    }
    if !issue.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Protected Contracts (readonly_files)**: {}\n",
            issue.readonly_files.join(", ")
        ));
    }
    if !issue.description.trim().is_empty() {
        out.push_str(&format!(
            "- **Description**:\n{}\n",
            issue.description.trim()
        ));
    }
    if !issue.recommendations.is_empty() {
        out.push_str("- **Recommendations**:\n");
        for (index, rec) in issue.recommendations.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, rec));
        }
    }
    if let Some(note) = &issue.note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Resolution Note**:\n{}\n", trimmed));
        }
    }
    if !issue.plan.is_empty() {
        out.push_str("- **Existing Subplan Steps**:\n");
        for sub in &issue.plan {
            out.push_str(&format!("  - [{}] #{} {}\n", sub.status, sub.id, sub.title));
        }
    }
    out.push('\n');

    if let Some(diag) = diagnostics {
        out.push_str("## Failed Sub-step Diagnostics:\n");
        let failed = find_subplan_item(&issue.plan, diag.failed_step_id);
        match failed {
            Some(step) => out.push_str(&format!(
                "- **Failed Sub-step**: #{} {}\n",
                step.id, step.title
            )),
            None => out.push_str(&format!(
                "- **Failed Sub-step**: #{}\n",
                diag.failed_step_id
            )),
        }
        if let Some(step) = failed {
            if !step.files.is_empty() {
                out.push_str(&format!(
                    "- **Declared Target Files**: {}\n",
                    step.files.join(", ")
                ));
            }
            if let Some(details) = &step.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!("- **Declared Contract**:\n{}\n", trimmed));
                }
            }
        }
        match diag.failure_reason.map(str::trim).filter(|s| !s.is_empty()) {
            Some(reason) => out.push_str(&format!("- **Execution Failure Report**: {}\n", reason)),
            None => out.push_str("- **Execution Failure Report**: (no diagnostics provided)\n"),
        }
        out.push_str("- **Horizon Contract**: return ONLY the new, unfinished steps that remain. Completed (DONE) sub-steps are preserved verbatim by the harness and MUST NOT be restated.\n");
        out.push('\n');
    }

    // The eager symbol map and the compact directory tree are mutually
    // exclusive: the Architect reads either symbol contours or the folder
    // layout, never both.
    if let Some(outline) = format_repository_outline_section(repo_map, repo_dirs) {
        out.push_str(&outline);
    }

    if !context_files.is_empty() {
        out.push_str("## Active Context Files:\n");
        for file in context_files {
            out.push_str(&format!(
                "### File: `{}`\n```\n{}\n```\n\n",
                file.path, file.content
            ));
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
    out.push_str("1. Determine the refactorings and code changes the finding requires.\n");
    out.push_str("2. Synthesize a decomposed, dependency-ordered subplan of leaf steps with `status: \"todo\"`.\n");
    out.push_str("3. For each leaf step, declare `files`, `readonly_files`, and `depends_on`. Ensure each step leaves the codebase in a compiling state.\n");
    out.push_str("4. If the finding is blocked or cannot be implemented, report `status: \"blocked\"` with `reason`.\n");
    out.push_str("5. Follow the Architect Discovery Protocol: before designing any step that interacts with an existing interface, type, trait, module, or crate, verify its real contract by reporting `status: \"needs_context\"` with the required `context_requests`, `repo_searches`, or `repo_maps`. Never infer contracts from RepoMap contours alone.\n");
    out.push_str("6. Language Directive: Formulate all titles, details, explanations, and reasons strictly in the same natural language as the finding.\n");

    out
}

/// Alias for `build_review_finding_refine_user_prompt` without diagnostics.
pub fn build_review_refine_user_prompt(
    session: &ReviewSession,
    item: &ReviewItem,
    context_files: &[ContextFileContent],
    repo_map: Option<&str>,
    repo_dirs: Option<&str>,
    instructions: Option<&str>,
) -> String {
    build_review_finding_refine_user_prompt(
        session,
        item,
        context_files,
        repo_map,
        repo_dirs,
        None,
        instructions,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueType, ReviewItemStatus, ReviewSeverity};
    use tauqe_protocol::ModelRef;

    fn sample_session() -> ReviewSession {
        ReviewSession {
            id: "rev-ref".to_string(),
            title: "Refinement Session".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            description: Some("Audit findings to refine".to_string()),
            user_prompt: None,
            target_files: vec!["crates/core/src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 42,
                title: "Decompose this issue".to_string(),
                subplan_last_replanned_step: None,
                model: None,
                issue_type: IssueType::Bug,
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                recommendations: vec![],
                files: vec!["crates/core/src/lib.rs".to_string()],
                readonly_files: vec![],
                subplan: vec![],
                subplan_architect: None,
                resolution_notes: None,
                body: "Needs structural division".to_string(),
            }],
            raw_markdown: String::new(),
        }
    }

    #[test]
    fn test_review_refine_outline_is_map_or_directories_never_both() {
        let session = sample_session();
        let item = &session.items[0];

        // Symbol contours win whenever the eager map is available, so the
        // directory tree never dilutes an existing outline.
        let with_map = build_review_finding_refine_user_prompt(
            &session,
            item,
            &[],
            Some("crate\n  pub fn main"),
            Some("crate\n  src"),
            None,
            None,
        );
        assert!(with_map.contains("## Codebase Symbol Contours (<repo_map>):"));
        assert!(with_map.contains("crate\n  pub fn main"));
        assert!(!with_map.contains("## Repository Directories"));

        // Without a map the compact tree still tells the Architect where
        // subsystems live instead of leaving it blind.
        let with_dirs = build_review_finding_refine_user_prompt(
            &session,
            item,
            &[],
            None,
            Some("crate\n  src"),
            None,
            None,
        );
        assert!(with_dirs.contains("## Repository Directories (<repo_dirs>):"));
        assert!(with_dirs.contains("crate\n  src"));
        assert!(!with_dirs.contains("## Codebase Symbol Contours"));
    }

    #[test]
    fn test_review_refine_contract_demands_distilled_content() {
        let sys = build_review_finding_refine_system_prompt();
        assert!(sys.contains("Final Content Invariant"));
        assert!(sys.contains("reasoning process itself is never plan content"));

        // The user prompt orders the outcome, not an analytical narrative.
        let session = sample_session();
        let item = &session.items[0];
        let user =
            build_review_finding_refine_user_prompt(&session, item, &[], None, None, None, None);
        assert!(!user.contains("Analyze the review finding"));
        assert!(user.contains("Determine the refactorings"));
    }
}
