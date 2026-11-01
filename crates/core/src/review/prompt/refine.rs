use crate::escape::{escape_xml_text, single_line};
use tauqe_protocol::review::{ReviewGroup, ReviewItem, ReviewSession};
use tauqe_protocol::PlanItem;

/// Target size range (in changed lines) for one cohesive leaf implementation step.
const LEAF_STEP_TARGET_LINES: (usize, usize) = (150, 400);

/// Maximum supported hierarchy nesting depth for structured plan refinement output.
pub const MAX_REVIEW_IMPLEMENTATION_DEPTH: usize = 5;

/// A question the engineer already answered on one finding.
///
/// The answer is the resolution the engineer recorded in review discussion, so a
/// finding that carries one states a settled requirement ("What") rather than a
/// doubt the Architect may raise again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedDecision<'a> {
    /// Identifier of the finding the decision resolves.
    pub finding_id: u32,
    /// Title of the finding, as the review stated it.
    pub title: &'a str,
    /// The decision itself, as the engineer recorded it.
    pub decision: &'a str,
}

/// Collects the decisions the engineer already recorded on the findings.
///
/// Every collected decision is the answer the engineer gave in review
/// discussion, so the questions it settles are no longer open ones.
pub fn recorded_decisions(findings: &[ReviewItem]) -> Vec<RecordedDecision<'_>> {
    findings
        .iter()
        .filter_map(|finding| {
            let decision = recorded_decision_of(finding)?;
            Some(RecordedDecision {
                finding_id: finding.id,
                title: finding.title.trim(),
                decision,
            })
        })
        .collect()
}

/// Whether every finding of the group carries a recorded decision.
///
/// A fully decided group has no open question left: every question it raised was
/// answered by the engineer, so the Architect has nothing to escalate.
pub fn all_decisions_recorded(findings: &[ReviewItem]) -> bool {
    if findings.is_empty() {
        return false;
    }
    findings
        .iter()
        .all(|finding| recorded_decision_of(finding).is_some())
}

/// Returns the trimmed decision recorded on one finding, when it carries one.
fn recorded_decision_of(finding: &ReviewItem) -> Option<&str> {
    finding
        .resolution_notes
        .as_deref()
        .map(str::trim)
        .filter(|decision| !decision.is_empty())
}
/// Whether the finding carries a recorded engineering decision.
///
/// The decision is the answer the engineer already gave in review discussion,
/// so a finding that carries one states a settled requirement rather than an
/// open question. The rule lives here, beside `recorded_decisions` and
/// `all_decisions_recorded`, so the readiness gate consumes it instead of
/// restating it.
pub fn has_recorded_decision(finding: &ReviewItem) -> bool {
    recorded_decision_of(finding).is_some()
}

/// Builds the system prompt for decomposing an architectural review group.
///
/// Dispatched to the Architect role when the findings of one group share a root
/// cause: the group is the unit of remediation, so the Architect answers with a
/// single implementation horizon that removes the cause instead of one plan per symptom.
pub fn build_review_group_refine_system_prompt() -> String {
    let mut out = String::new();
    out.push_str("You are a Principal Software Architect in TAUQE (The Answer to the Ultimate Question of Engineering).\n\n");
    out.push_str("Your mission is Architectural Root-Cause Group Decomposition: analyzing a group of code review findings that share one architectural root cause and synthesizing a single actionable, verifiable implementation plan that eliminates that cause across the whole codebase.\n\n");

    out.push_str("## Core Invariants & Rules:\n\n");
    out.push_str("1. Root-Cause Remediation Mandate (\"What\" vs \"How\"):\n");
    out.push_str("   - The group states one shared root cause and collects every finding that manifests it.\n");
    out.push_str("   - Your exclusive responsibility is engineering decomposition (\"How\"): the exact modules, interfaces, contracts, types, and sequence of implementation required to eliminate that cause.\n");
    out.push_str("   - Fix the cause instead of the symptoms one by one: a step that silences a single manifestation without removing the cause is a contract violation.\n\n");

    out.push_str("2. Implementation Plan Synthesis Contract:\n");
    out.push_str("   - Return one hierarchical list of executable implementation steps with `status: \"todo\"` that resolves EVERY finding of the group.\n");
    out.push_str("   - Return ONLY the unfinished steps: already completed (DONE) sub-steps are preserved verbatim by the harness, so restating them is a contract violation.\n");
    out.push_str("   - When failure diagnostics of an already executed sub-step are provided, reproject the remaining horizon around that failure instead of repeating the failed approach.\n");
    out.push_str("   - Synthesize a coherent roadmap that resolves the whole group without breaking existing subsystem invariants.\n\n");

    out.push_str("3. Strict Blocker Protocol (Refusal to Plan):\n");
    out.push_str("   - If you discover that the findings do not share the stated root cause, that the group is a false positive, or that it cannot be resolved without developer guidance:\n");
    out.push_str("   - You MUST set `status: \"blocked\"`, provide empty `items: []`, and document a comprehensive technical explanation in `reason`.\n");
    out.push_str("   - A finding that carries a recorded decision is never a reason to report a blocker: the engineer already answered it, so only a question left open may be escalated.\n\n");

    out.push_str("4. Recorded Engineering Decisions (Binding Developer Directives):\n");
    out.push_str("   - A finding may carry a recorded decision: the engineer already answered the question it raised in review discussion.\n");
    out.push_str("   - A recorded decision is a requirement (\"What\"), not a suggestion: design the remediation strictly within it, and never restate it as a doubt, an open question, or a blocker.\n");
    out.push_str("   - A group in which every finding carries a recorded decision is settled: escalate nothing and return the decomposition that implements those decisions.\n\n");

    push_step_granularity_contract(&mut out);

    out.push_str("6. Strict Language Consistency Invariant:\n");
    out.push_str("   - Detect the primary natural language used in the group title, in its root-cause description, and in the findings.\n");
    out.push_str("   - You MUST formulate all titles, step details, explanations, and reasons strictly in that SAME natural language.\n\n");

    push_architect_discovery_contract(&mut out);
    push_output_format_contract(&mut out);
    out
}

/// Pushes the granularity contract every decomposition prompt states as rule 5.
fn push_step_granularity_contract(out: &mut String) {
    out.push_str("5. Step Granularity and Compilability Invariants:\n");
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
}

/// Pushes the discovery contract every decomposition prompt states as rule 7.
fn push_architect_discovery_contract(out: &mut String) {
    out.push_str("7. Architect Discovery Protocol (Mandatory Contract Verification):\n");
    out.push_str("   - You have access to symbol contours in `<repo_map>` and already loaded files in `Active Context Files`.\n");
    out.push_str("   - Eager contour generation may be disabled, so `<repo_map>` can cover only part of the codebase. A module absent from the contours is never evidence that it is empty: request its contours via `repo_maps` before designing a step that touches it.\n");
    out.push_str("   - RepoMap contours are blinkers, not contracts: they reveal only the shape of the codebase (names, kinds, and rough signatures), never exact type definitions, field layouts, trait bounds, error semantics, lifetimes, or crate boundaries.\n");
    out.push_str("   - Whenever a leaf step you synthesize must interact with an existing interface, struct, enum, function, trait, module, or external crate, you MUST NOT infer its contract from contours alone. You MUST set `status: \"needs_context\"`, request the concrete source files in `context_requests: [...]`, execute searches via `repo_searches: [...]`, or request Tree-sitter symbol contours of a specific subsystem via `repo_maps: [\"crates/core/src/review\"]`, and leave `items: []`.\n");
    out.push_str("   - Contour requests (`repo_maps`) accept a repository-relative directory or file prefix; an empty string or `.` names the whole repository. They reveal module shape only, so an exact contract still has to be verified through `context_requests` or `repo_searches`.\n");
    out.push_str("   - Repository Search Syntax: searches use Rust / ripgrep regular expressions (lookaround assertions and backreferences are not supported).\n");
    out.push_str("   - Never emit speculative field names, argument types, return types, generic parameters, or method signatures derived from a contour line. A guessed contract is a hard contract violation.\n");
    out.push_str("   - Do not re-request files already present in `Active Context Files`; treat them as verified context.\n\n");
}

/// Pushes the output contract every decomposition prompt states as rule 8.
fn push_output_format_contract(out: &mut String) {
    out.push_str("8. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema.\n");
    out.push_str("   - Final Content Invariant: every text field (explanation, titles, details, reason) carries only the final distilled result. The reasoning process itself is never plan content: do not record deliberation, self-questioning, exploratory analysis, or intermediate reasoning steps in any field.\n");
}
/// Diagnostics describing a failed sub-step that triggered group replanning.
///
/// A group replan is the same Architect operation as its initial decomposition:
/// the failed approach is named in the prompt, so the remaining horizon is
/// reprojected around it instead of being repeated step by step.
#[derive(Debug, Clone, Copy)]
pub struct ReviewGroupRefineDiagnostics<'a> {
    /// Identifier of the sub-step whose execution failed.
    pub failed_step_id: &'a str,
    /// Diagnostic reported by the failed execution, when one is available.
    pub failure_reason: Option<&'a str>,
}

/// Renders repository paths as escaped, backticked breadcrumbs.
pub(super) fn render_path_list(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| format!("`{}`", escape_xml_text(path)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders the session preamble every refinement prompt opens with.
fn push_session_preamble(out: &mut String, session: &ReviewSession) {
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
}

/// Renders the replanning diagnostics of a failed sub-step.
fn push_replan_diagnostics(
    out: &mut String,
    failed_step: Option<&PlanItem>,
    failed_step_id: &str,
    failure_reason: Option<&str>,
) {
    out.push_str("## Failed Sub-step Diagnostics:\n");
    match failed_step {
        Some(step) => out.push_str(&format!(
            "- **Failed Sub-step**: #{} {}\n",
            step.id, step.title
        )),
        None => out.push_str(&format!("- **Failed Sub-step**: #{}\n", failed_step_id)),
    }
    if let Some(step) = failed_step {
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
    match failure_reason.map(str::trim).filter(|s| !s.is_empty()) {
        Some(reason) => out.push_str(&format!("- **Execution Failure Report**: {}\n", reason)),
        None => out.push_str("- **Execution Failure Report**: (no diagnostics provided)\n"),
    }
    out.push_str("- **Horizon Contract**: return ONLY the new, unfinished steps that remain. Completed (DONE) sub-steps are preserved verbatim by the harness and MUST NOT be restated.\n");
    out.push('\n');
}

/// Renders the developer directives attached to a decomposition run.
///
/// The repository material of a synthesis — the rendered outline, the loaded
/// files, and the findings discovery accumulated — is composed by the turn kernel
/// of the structured contour, so this builder renders the directives alone and
/// never restates material the kernel already provides.
fn push_developer_directives(out: &mut String, instructions: Option<&str>) {
    if let Some(ins) = instructions {
        let trimmed = ins.trim();
        if !trimmed.is_empty() {
            out.push_str("## Developer Directives:\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }
}

/// Builds the user prompt for decomposing an architectural review group.
///
/// The group is the operational unit of remediation: its findings are symptoms
/// of one shared root cause, so the Architect receives the cause together with
/// every manifestation and answers with a single coherent implementation horizon.
///
/// The repository material of the round — the rendered outline and the loaded
/// files — is composed by the turn kernel of the structured contour, so the
/// builder renders only the group, its findings, the replanning diagnostics, and
/// the developer directives.
pub fn build_review_group_refine_user_prompt(
    session: &ReviewSession,
    group: &ReviewGroup,
    findings: &[ReviewItem],
    diagnostics: Option<ReviewGroupRefineDiagnostics<'_>>,
    instructions: Option<&str>,
) -> String {
    let clean_title = single_line(&group.title);
    let title = if clean_title.is_empty() {
        single_line(&group.id)
    } else {
        clean_title
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# Review Group Refinement: [{}] {}\n\n",
        single_line(&session.id),
        escape_xml_text(&title)
    ));

    push_session_preamble(&mut out, session);

    out.push_str("## Target Group to Decompose:\n");
    out.push_str(&format!("- **ID**: `{}`\n", single_line(&group.id)));
    out.push_str(&format!("- **Title**: {}\n", escape_xml_text(&title)));
    if let Some(desc) = &group.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "- **Root Cause**:\n{}\n",
                escape_xml_text(trimmed)
            ));
        }
    }
    out.push('\n');

    out.push_str("## Group Findings to Decompose:\n");
    for finding in findings {
        push_group_finding(&mut out, finding);
    }
    push_recorded_decisions(&mut out, &recorded_decisions(findings));

    if let Some(diag) = diagnostics {
        // The failed sub-step is looked up in the horizon the group records, so
        // the diagnostics describe the plan the group actually owns.
        let failed = group.implementation.find_item(diag.failed_step_id);
        push_replan_diagnostics(&mut out, failed, diag.failed_step_id, diag.failure_reason);
    }

    push_developer_directives(&mut out, instructions);

    out.push_str("## Architect Instructions:\n");
    out.push_str("1. Identify the single architectural root cause the findings of this group share, then determine the code changes that remove it.\n");
    out.push_str("2. Synthesize one decomposed, dependency-ordered subplan of leaf steps with `status: \"todo\"` that resolves every finding of the group.\n");
    out.push_str("2. Synthesize one decomposed, dependency-ordered implementation horizon of leaf steps with `status: \"todo\"` that resolves every finding of the group.\n");
    out.push_str("4. Design strictly within every recorded engineering decision: the question is already answered, so it is never a reason to report `status: \"blocked\"`.\n");
    out.push_str("5. If the group cannot be implemented within the recorded decisions, report `status: \"blocked\"` with `reason`.\n");
    out.push_str("6. Follow the Architect Discovery Protocol: before designing any step that interacts with an existing interface, type, trait, module, or crate, verify its real contract by reporting `status: \"needs_context\"` with the required `context_requests`, `repo_searches`, or `repo_maps`. Never infer contracts from RepoMap contours alone.\n");
    out.push_str("7. Language Directive: Formulate all titles, details, explanations, and reasons strictly in the same natural language as the group and its findings.\n");

    out
}

/// Renders one finding of a group as decomposition input.
fn push_group_finding(out: &mut String, finding: &ReviewItem) {
    out.push_str(&format!(
        "### Finding `#{}` [{}] {}\n",
        finding.id,
        finding.issue_type,
        escape_xml_text(&single_line(&finding.title))
    ));
    if !finding.files.is_empty() {
        out.push_str(&format!(
            "- **Files**: {}\n",
            render_path_list(&finding.files)
        ));
    }
    if !finding.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Protected Contracts (readonly_files)**: {}\n",
            render_path_list(&finding.readonly_files)
        ));
    }
    if !finding.body.trim().is_empty() {
        out.push_str(&format!(
            "- **Description**:\n{}\n",
            escape_xml_text(finding.body.trim())
        ));
    }
    if !finding.recommendations.is_empty() {
        out.push_str("- **Recommendations**:\n");
        for (index, rec) in finding.recommendations.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, escape_xml_text(rec)));
        }
    }
    out.push('\n');
}

/// Renders the decisions the engineer already recorded as binding directives.
///
/// A recorded decision is one answer the group can no longer ask for, so it is
/// stated as a requirement ("What") beside the findings it settles.
fn push_recorded_decisions(out: &mut String, decisions: &[RecordedDecision<'_>]) {
    if decisions.is_empty() {
        return;
    }
    out.push_str("## Recorded Engineering Decisions (Binding Developer Directives):\n");
    out.push_str("- The engineer already answered these questions in review discussion, so each one is a requirement (\"What\") and never an open question: design the remediation strictly within it, and never report it as a blocker or ask it again.\n");
    for decision in decisions {
        let id = decision.finding_id;
        let title = escape_xml_text(decision.title);
        let answer = escape_xml_text(decision.decision);
        out.push_str(&format!("- **Finding `#{id}` — {title}**: {answer}\n"));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueStatus, IssueType, ReviewIssue};

    /// Builds a finding that carries the given recorded decision.
    fn finding(decision: Option<&str>) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id: 7,
            title: "Guard the parser".to_string(),
            uuid: None,
            group: Some("arch-1".to_string()),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: "The parser returns bare strings.".to_string(),
            recommendations: vec!["Return a structured error".to_string()],
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: decision.map(str::to_string),
        })
    }

    /// Builds the session the group prompt is rendered for.
    fn session() -> ReviewSession {
        ReviewSession {
            id: "rev-grp".to_string(),
            title: "Grouped session".to_string(),
            ..Default::default()
        }
    }

    /// Builds the group whose root cause the prompt decomposes.
    fn group() -> ReviewGroup {
        ReviewGroup {
            id: "arch-1".to_string(),
            title: "Unified error handling".to_string(),
            description: Some("Every module invents its own error type.".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn recorded_decisions_collect_only_answered_findings() {
        let mut open = finding(None);
        open.id = 9;

        let findings = [finding(Some("  Keep the public API stable  ")), open];
        let decisions = recorded_decisions(&findings);

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].finding_id, 7);
        assert_eq!(decisions[0].title, "Guard the parser");
        assert_eq!(decisions[0].decision, "Keep the public API stable");
    }

    #[test]
    fn all_decisions_recorded_requires_every_finding_to_carry_one() {
        let mut open = finding(None);
        open.id = 9;

        assert!(!all_decisions_recorded(&[]));
        assert!(all_decisions_recorded(&[finding(Some("Decided"))]));
        assert!(!all_decisions_recorded(&[finding(Some("Decided")), open]));
        // A note the engineer left blank is not a decision.
        assert!(!all_decisions_recorded(&[finding(Some("   "))]));
    }

    #[test]
    fn system_prompt_treats_recorded_decisions_as_binding_directives() {
        let system = build_review_group_refine_system_prompt();
        let rule = "Recorded Engineering Decisions (Binding Developer Directives)";

        assert!(system.contains(rule));
        assert!(system.contains("is never a reason to report a blocker"));
        // The new rule takes the fourth position, so the rules that follow keep
        // a gap-free numbering.
        assert!(system.contains("5. Step Granularity and Compilability Invariants:"));
        assert!(system.contains("6. Strict Language Consistency Invariant:"));
        assert!(system.contains("7. Architect Discovery Protocol"));
        assert!(system.contains("8. Output Format:"));
    }

    #[test]
    fn user_prompt_renders_recorded_decisions_as_binding_directives() {
        let user = build_review_group_refine_user_prompt(
            &session(),
            &group(),
            &[finding(Some("Keep the public API stable"))],
            None,
            None,
        );
        let header = "## Recorded Engineering Decisions (Binding Developer Directives):";

        assert!(user.contains(header));
        assert!(user.contains("**Finding `#7` — Guard the parser**"));
        assert!(user.contains("Keep the public API stable"));
        assert!(user.contains("never report it as a blocker"));
        // The decisions are decomposition input, so they precede the architect
        // instructions.
        let decisions_at = user
            .find("Recorded Engineering Decisions")
            .expect("decisions");
        let instructions_at = user
            .find("## Architect Instructions:")
            .expect("instructions");
        assert!(decisions_at < instructions_at);
    }
    #[test]
    fn has_recorded_decision_accepts_only_a_non_empty_note() {
        let decision = "Keep the public API stable";

        assert!(!has_recorded_decision(&finding(None)));
        assert!(!has_recorded_decision(&finding(Some("   "))));
        assert!(has_recorded_decision(&finding(Some(decision))));
    }

    #[test]
    fn has_recorded_decision_matches_the_collected_decisions() {
        let mut decided = finding(Some("Decided"));
        decided.id = 9;
        let mut blank = finding(Some("   "));
        blank.id = 11;

        let findings = [finding(None), decided, blank];
        let mut by_predicate: Vec<u32> = findings
            .iter()
            .filter(|item| has_recorded_decision(item))
            .map(|item| item.id)
            .collect();
        let mut by_collector: Vec<u32> = recorded_decisions(&findings)
            .iter()
            .map(|decision| decision.finding_id)
            .collect();

        by_predicate.sort_unstable();
        by_collector.sort_unstable();

        assert_eq!(by_predicate, by_collector);
        // Только находка с непустой заметкой является зафиксированным
        // решением: пустая заметка и её отсутствие не квалифицируются.
        assert_eq!(by_predicate, vec![9]);
    }
}
