//! Prompts and schema of the two-phase plan synthesis contour.
//!
//! A plan is issued in two phases: the Planner synthesizes a draft horizon, an
//! independent Architect entry audits that draft, and the Planner reconciles
//! the findings into the plan it issues. The critique is a read-only
//! analytical round: [`build_plan_critique_prompt`] renders the requirement and
//! the draft, and the critic answers with [`PlanCritiqueOutput`], never with a
//! plan of its own. The reconciliation is the horizon synthesis itself, so its
//! round reuses
//! [`build_plan_horizon_system_prompt`](super::refine::build_plan_horizon_system_prompt)
//! and the `plan_refinement` schema; only the user prompt differs, because it
//! carries the audit ([`build_plan_reconciliation_prompt`]).

use crate::edits::stream::json::decode::{first_json_object_str, strip_code_fence};
use crate::escape::{escape_xml_text, single_line};
use serde::{Deserialize, Serialize};
use tauqe_protocol::{ConceptItem, Plan, PlanItem, PlanItemStatus};

use super::refine::PlanRefinementOutput;

/// Kind of defect an independent critique reports.
///
/// The categories partition the audit, so the reconciliation answers each
/// defect with the repair its own kind requires. They travel on the wire
/// exactly as the response schema enumerates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCritiqueCategory {
    /// A concept invariant no step implements, or a step that implements nothing.
    InvariantGap,
    /// A dependency that cannot hold: a cycle, a self-dependency, or an unknown prerequisite.
    DependencyCycle,
    /// A step that is not a cohesive unit of work or names no target context.
    DecompositionDefect,
    /// A step that decides what the concept fixed, or claims work the harness owns.
    ScopeViolation,
    /// A draft that cannot be executed as written: no horizon, a non-success
    /// status, or a repeated step identifier.
    ContractViolation,
}

impl PlanCritiqueCategory {
    /// Wire spelling of the category, identical to the schema enumeration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvariantGap => "invariant_gap",
            Self::DependencyCycle => "dependency_cycle",
            Self::DecompositionDefect => "decomposition_defect",
            Self::ScopeViolation => "scope_violation",
            Self::ContractViolation => "contract_violation",
        }
    }
}

/// Verdict of an independent critique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCritiqueVerdict {
    /// The draft violates nothing, so the reconciliation round is skipped.
    Approved,
    /// At least one finding must be reconciled before the plan is issued.
    ChangesRequired,
}

impl PlanCritiqueVerdict {
    /// Wire spelling of the verdict, identical to the schema enumeration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequired => "changes_required",
        }
    }
}

/// One actionable defect an independent critique reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCritiqueFinding {
    pub category: PlanCritiqueCategory,
    /// Identifier of the concept node or plan step the finding addresses.
    ///
    /// An absent target names a defect of the draft as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The violation together with the concrete repair the Planner must apply.
    pub detail: String,
}

/// Output of the independent critique phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCritiqueOutput {
    pub verdict: PlanCritiqueVerdict,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<PlanCritiqueFinding>,
}

impl PlanCritiqueOutput {
    /// Reports whether the critique approved the draft without findings.
    ///
    /// The reconciliation round runs only when the draft must change, so the
    /// pipeline reads that decision from the verdict instead of matching a
    /// finding count.
    pub fn is_approved(&self) -> bool {
        self.verdict == PlanCritiqueVerdict::Approved
    }
}

/// Parses the raw JSON string returned by the critic into a [`PlanCritiqueOutput`].
///
/// Decoding goes through the shared structured-output helpers
/// (`strip_code_fence`, `first_json_object_str`), exactly like every other
/// structured-output parser in the engine: a markdown code fence is unwrapped
/// first, and a payload surrounded by wrapper prose yields its outermost
/// balanced JSON object.
pub fn parse_plan_critique_output(raw_json: &str) -> anyhow::Result<PlanCritiqueOutput> {
    let unfenced = strip_code_fence(raw_json);
    let error = match serde_json::from_str::<PlanCritiqueOutput>(unfenced) {
        Ok(output) => return Ok(output),
        Err(error) => error,
    };
    if let Some(object) = first_json_object_str(raw_json) {
        if let Ok(output) = serde_json::from_str::<PlanCritiqueOutput>(object) {
            return Ok(output);
        }
    }
    Err(anyhow::anyhow!(
        "Failed to parse plan critique JSON output: {error}"
    ))
}

/// Strict JSON Schema for the independent critique output.
pub fn plan_critique_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["approved", "changes_required"],
                "description": "Critique verdict: 'approved' only when the draft violates nothing and 'findings' is empty, otherwise 'changes_required'."
            },
            "summary": {
                "type": "string",
                "description": "Concise audit summary in the same natural language as the plan: what the draft gets right and which invariants it breaks. Final distilled content only: no chain-of-thought, deliberation, or intermediate reasoning steps."
            },
            "findings": {
                "type": "array",
                "description": "Every defect the Planner must reconcile, reported exhaustively in one round. Empty only for an approved draft.",
                "items": {
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "enum": ["invariant_gap", "dependency_cycle", "decomposition_defect", "scope_violation", "contract_violation"],
                            "description": "'invariant_gap' for a concept requirement no step implements or a step implementing nothing; 'dependency_cycle' for a dependency that cannot hold; 'decomposition_defect' for a non-cohesive step or a missing target context; 'scope_violation' for a step that decides what the concept fixed or claims work the harness owns; 'contract_violation' for a draft that cannot be executed as written."
                        },
                        "target": {
                            "anyOf": [
                                { "type": "string" },
                                { "type": "null" }
                            ],
                            "description": "Identifier of the concept node or plan step the finding addresses, or null when the defect belongs to the draft as a whole."
                        },
                        "detail": {
                            "type": "string",
                            "description": "The violation together with the concrete repair the Planner must apply, in the same natural language as the plan."
                        }
                    },
                    "required": ["category", "target", "detail"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["verdict", "summary", "findings"],
        "additionalProperties": false
    })
}

/// JSON Schema definition wrapper for provider configuration.
pub fn plan_critique_response_schema_definition() -> serde_json::Value {
    serde_json::json!({
        "name": "plan_critique",
        "description": "Independent critique of a plan draft: the verdict and every finding the Planner must reconcile",
        "strict": true,
        "schema": plan_critique_response_schema()
    })
}

/// Builds the system prompt of the independent critique phase.
///
/// The critic owns judgement, never authorship: the prompt forbids it to rewrite
/// the draft, to issue a plan of its own, or to request context, and it names
/// the category of every defect it may report so the verdict stays decidable.
pub fn build_plan_critique_system_prompt() -> String {
    let mut out = String::new();
    out.push_str("You are a Principal Software Architect in TAUQE (The Answer to the Ultimate Question of Engineering).\n\n");
    out.push_str("Your mission is an Independent Plan Critique: auditing a plan draft synthesized by another Architect entry against the immutable architectural concept, the developer directives, and the harness decomposition invariants, and reporting every defect you find together with the concrete repair it requires.\n\n");
    out.push_str("## Core Invariants & Rules:\n\n");
    out.push_str("1. Independent Judgement:\n");
    out.push_str(
        "   - You audit a draft you did not write, and you inherit none of its assumptions.\n",
    );
    out.push_str("   - Judge the draft strictly by the concept, the developer directives, and the invariants below; the Planner's own explanation is context, never evidence.\n");
    out.push_str("   - Do not report a step you would merely have written differently: a different but valid decomposition is not a defect.\n\n");
    out.push_str("2. Read-Only Audit Contract:\n");
    out.push_str("   - You never rewrite, reorder, or re-emit the draft, and you never issue a plan of your own: the Planner reconciles your findings in a later round.\n");
    out.push_str("   - A finding is actionable only when `detail` names the violation and the concrete repair. A finding without a stated repair is noise.\n");
    out.push_str("   - You request no context and run no research: the plan and its draft reach you in full, so a defect you cannot decide from them is reported as a finding of its category instead of a question.\n\n");
    out.push_str("3. Concept Immutability (\"What\" vs \"How\"):\n");
    out.push_str("   - When the plan carries a concept tree, it is the developer's immutable decision: a draft that alters, dilutes, or contradicts it is a defect, never an accepted improvement.\n");
    out.push_str("   - When the plan is being created from directives and carries no concept yet, the developer directives are the requirement, and a draft that contradicts them is a defect.\n");
    out.push_str("   - A draft that leaves an open disjunctive alternative (\"either X or Y\") undecided is a defect: the decision belongs to the concept, and a step owns only the implementation.\n\n");
    out.push_str("4. Invariant Coverage Audit (category `invariant_gap`):\n");
    out.push_str("   - Every concept node must be implemented by at least one step, and every step must implement a declared concept node or a developer directive.\n");
    out.push_str("   - Report a concept invariant that no step enforces, and a step that enforces nothing.\n\n");
    out.push_str("5. Dependency Audit (category `dependency_cycle`):\n");
    out.push_str("   - The steps form a DAG: report a self-dependency, a prerequisite that neither a step of the draft nor a completed foundation declares, a prerequisite declared after its dependent, and every cycle, direct or transitive.\n");
    out.push_str("   - Report a leaf that depends on its own ancestor: the ancestor finishes only after that leaf, so the pair can never start.\n\n");
    out.push_str("6. Decomposition Audit (category `decomposition_defect`):\n");
    out.push_str("   - Every executable leaf must name the context it targets in `files` or `readonly_files`, and a parent container item must leave both lists empty.\n");
    out.push_str("   - A leaf stays a cohesive unit of work: report micro-fragments that split one logical change and monoliths that bundle unrelated files.\n");
    out.push_str("   - Report a step whose details leave the implementation undecidable, because the executor of a later turn cannot ask for clarification.\n\n");
    out.push_str("7. Scope Audit (category `scope_violation`):\n");
    out.push_str("   - Report a step that decides an architectural question the concept already fixed, or that substitutes a selected component, library, or interface.\n");
    out.push_str("   - Report a step whose only work is verification: the harness runs the toolchain deterministically after every leaf, and the executor has no shell.\n\n");
    out.push_str("8. Execution Contract Audit (category `contract_violation`):\n");
    out.push_str("   - Report a draft that carries no executable step, or whose status is not `success`, as a missing horizon instead of approving it.\n");
    out.push_str("   - Report a step identifier that repeats: the harness rejects a horizon whose identifiers are not unique, so such a draft can never be executed.\n\n");
    out.push_str("9. Verdict Discipline:\n");
    out.push_str("   - Set `verdict` to `approved` only when the draft violates nothing; an approval carries an empty `findings` list.\n");
    out.push_str("   - Set `verdict` to `changes_required` as soon as one defect exists, and report every defect of the draft in a single round: the reconciliation answers the whole audit at once, so a defect reported later costs an extra round.\n");
    out.push_str("   - `summary` states the audit result in one paragraph: what the draft gets right and which invariants it breaks.\n\n");
    out.push_str("10. Strict Language Consistency Invariant:\n");
    out.push_str("   - Detect the primary natural language used in the plan title, description, and developer directives.\n");
    out.push_str("   - You MUST formulate `summary` and every `detail` strictly in that SAME natural language.\n");
    out.push_str(
        "   - Do NOT switch languages unless the plan itself is written in that language.\n\n",
    );
    out.push_str("11. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema.\n");
    out.push_str("   - Final Content Invariant: every text field carries only the final distilled result. The reasoning process itself is never critique content: do not record deliberation, self-questioning, exploratory analysis, or intermediate reasoning steps in any field.\n");
    out
}

/// Builds the user prompt of the independent critique phase.
///
/// The prompt carries the whole coordinate the critic needs: the requirement
/// (the concept tree, or the developer directives of a plan being created), the
/// completed foundation a draft step may depend on, and the draft horizon
/// itself. The critic receives it in full, because the phase runs no research.
pub fn build_plan_critique_prompt(
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Independent Plan Critique: [{}] {}\n\n",
        single_line(&plan.id),
        single_line(&plan.title)
    ));
    render_plan_intent(&mut out, plan);
    render_directives(&mut out, directives);
    render_concept(&mut out, &plan.concept);
    render_completed_foundation(&mut out, plan);
    render_draft(
        &mut out,
        "## Draft Horizon Proposed by the Planner (the audit target):\n",
        draft,
    );

    out.push_str("## Critic Instructions:\n");
    out.push_str("1. Audit the draft against the immutable concept, or against the developer directives when the plan carries none yet, and report every defect of the categories stated by the system contract.\n");
    out.push_str("2. Verify the dependency graph of the draft: every prerequisite resolves to a declared step or an already completed one, the declared order respects them, and no cycle exists.\n");
    out.push_str("3. Verify the coverage of the requirement: every concept node or developer directive is implemented by at least one step, and no step implements nothing.\n");
    out.push_str("4. Verify the decomposition contract: leaf-only target files, cohesive leaf granularity, decidable step details, and no verification-only step.\n");
    out.push_str("5. Report every defect you find in this round, each with the concrete repair the Planner must apply in `detail`.\n");
    out.push_str("6. Decide the verdict: `approved` only for a draft that violates nothing, `changes_required` otherwise.\n");
    out.push_str("7. Language Directive: detect the natural language of the plan title, description, and developer directives, and formulate the summary and every finding strictly in that same language.\n");
    out
}

/// Builds the user prompt of the reconciliation round.
///
/// The round is the horizon synthesis itself, so it reuses
/// [`build_plan_horizon_system_prompt`](super::refine::build_plan_horizon_system_prompt)
/// and the `plan_refinement` schema; this prompt adds what only the
/// reconciliation knows: the rejected draft and the independent audit. The
/// Planner adjudicates the audit, because the architectural decision stays with
/// the concept and the developer, never with the critic.
pub fn build_plan_reconciliation_prompt(
    plan: &Plan,
    draft: &PlanRefinementOutput,
    critique: &PlanCritiqueOutput,
    directives: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Plan Reconciliation: [{}] {}\n\n",
        single_line(&plan.id),
        single_line(&plan.title)
    ));
    render_plan_intent(&mut out, plan);
    render_directives(&mut out, directives);
    render_concept(&mut out, &plan.concept);
    render_completed_foundation(&mut out, plan);
    render_draft(
        &mut out,
        "## Previous Draft Horizon (the round the critique audited):\n",
        draft,
    );
    render_critique(&mut out, critique);

    out.push_str("## Reconciliation Instructions:\n");
    out.push_str("1. Adjudicate every finding: accept it and repair the horizon, or reject it with the reason stated in `explanation`. A finding that contradicts the concept, the developer directives, or the harness invariants is rejected, never applied.\n");
    out.push_str("2. Re-emit the WHOLE unfinished horizon: the harness replaces the unfinished horizon with your answer and preserves the completed (DONE) foundation verbatim. Do NOT restate a completed step and do NOT resend the previous draft unchanged.\n");
    out.push_str("3. Keep every element the critique did not challenge: reconciliation repairs defects, it does not rewrite a valid decomposition.\n");
    out.push_str("4. Answer with the `plan_refinement` contract of the horizon synthesis: `status`, `explanation`, `reason`, `abort`, `context_requests`, `repo_searches`, `repo_maps`, and `items`.\n");
    out.push_str("5. The reconciliation round is not a research round: carry `context_requests: []`, `repo_searches: []`, and `repo_maps: []` unless a finding genuinely cannot be adjudicated without an exact contract. A contract you must verify answers with `status: \"needs_context\"`, the request that resolves it, and `items: []`.\n");
    out.push_str("6. When a finding reveals an architectural contradiction only the developer can resolve, set `status` to `blocked`, leave `items: []`, and state the exact decision required in `reason`. Never resolve it by offering alternatives inside a step.\n");
    out.push_str("7. A host-environment condition only the developer can repair is never a blocker: report it through `abort` with `is_aborted: true`, name the concrete missing capability in `reason`, carry the exact command the developer must run in `recommendation`, and leave `items: []`.\n");
    out.push_str("8. Preserve the decomposition invariants of the synthesis: leaf-only target files, cohesive leaf granularity, decidable details, no verification-only step, and concept traceability.\n");
    out.push_str("9. Language Directive: detect the natural language of the plan title, description, and developer directives, and formulate `explanation`, titles, and details strictly in that same language.\n");
    out.push_str("10. Final Content Invariant: every text field carries only the final distilled result, never deliberation, self-questioning, or intermediate reasoning steps.\n");
    out
}

/// Renders identifiers or paths into a single escaped, comma-separated line.
fn escape_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| escape_xml_text(&single_line(item)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders the high-level intent of the plan when it states one.
fn render_plan_intent(out: &mut String, plan: &Plan) {
    if let Some(description) = &plan.description {
        let trimmed = description.trim();
        if !trimmed.is_empty() {
            out.push_str("## High-Level Architectural Intent (\"What\"):\n");
            out.push_str(&escape_xml_text(trimmed));
            out.push_str("\n\n");
        }
    }
}

/// Renders the developer directives that established the plan.
fn render_directives(out: &mut String, directives: Option<&str>) {
    if let Some(directives) = directives.map(str::trim).filter(|value| !value.is_empty()) {
        out.push_str("## Developer Directives (\"What\" the plan must satisfy):\n");
        out.push_str(&escape_xml_text(directives));
        out.push_str("\n\n");
    }
}

/// Renders the concept tree, or states that the directives are the requirement.
fn render_concept(out: &mut String, concept: &[ConceptItem]) {
    out.push_str("## Immutable Architectural Concept (\"What\"):\n");
    if concept.is_empty() {
        out.push_str("*(The plan carries no concept yet: the developer directives above are the requirement.)*\n\n");
        return;
    }
    fn walk(out: &mut String, items: &[ConceptItem], depth: usize) {
        let indent = "  ".repeat(depth);
        for item in items {
            out.push_str(&format!(
                "{}- **#{}** {}",
                indent,
                single_line(&item.id),
                escape_xml_text(&single_line(&item.title))
            ));
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                }
            }
            out.push('\n');
            walk(out, &item.children, depth + 1);
        }
    }
    walk(out, concept, 0);
    out.push('\n');
}

/// Collects every completed step of the plan, at any nesting depth.
fn collect_done<'a>(items: &'a [PlanItem], acc: &mut Vec<&'a PlanItem>) {
    for item in items {
        if item.status == PlanItemStatus::Done {
            acc.push(item);
        }
        collect_done(&item.children, acc);
    }
}

/// Renders the completed foundation the reconciliation preserves verbatim.
fn render_completed_foundation(out: &mut String, plan: &Plan) {
    out.push_str(
        "## Completed Foundation (DONE steps - immutable; a draft step may depend on them):\n",
    );
    let mut done = Vec::new();
    collect_done(&plan.implementation.items, &mut done);
    if done.is_empty() {
        out.push_str("*(No completed steps yet)*\n\n");
        return;
    }
    for item in done {
        out.push_str(&format!(
            "- [x] #{} {}",
            single_line(&item.id),
            escape_xml_text(&single_line(&item.title))
        ));
        if !item.files.is_empty() {
            out.push_str(&format!(" (files: {})", escape_list(&item.files)));
        }
        out.push('\n');
    }
    out.push('\n');
}

/// Renders a horizon payload under the heading of the round that carries it.
fn render_draft(out: &mut String, heading: &str, draft: &PlanRefinementOutput) {
    out.push_str(heading);
    out.push_str(&format!(
        "- **Status**: {}\n",
        escape_xml_text(&single_line(&draft.status))
    ));
    let explanation = draft.explanation.trim();
    if !explanation.is_empty() {
        out.push_str(&format!(
            "- **Explanation**: {}\n",
            escape_xml_text(&single_line(explanation))
        ));
    }
    if let Some(reason) = draft
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.push_str(&format!(
            "- **Reported Reason**: {}\n",
            escape_xml_text(&single_line(reason))
        ));
    }
    if draft.items.is_empty() {
        out.push_str("\n*(No steps were synthesized)*\n\n");
        return;
    }
    out.push('\n');
    fn walk(out: &mut String, items: &[PlanItem], depth: usize) {
        let indent = "  ".repeat(depth);
        for item in items {
            out.push_str(&format!(
                "{}- #{} {} (status: {})",
                indent,
                escape_xml_text(&single_line(&item.id)),
                escape_xml_text(&single_line(&item.title)),
                item.status
            ));
            for (label, values) in [
                ("files", &item.files),
                ("readonly_files", &item.readonly_files),
                ("concept_refs", &item.concept_refs),
                ("depends_on", &item.depends_on),
            ] {
                if !values.is_empty() {
                    out.push_str(&format!(" [{}: {}]", label, escape_list(values)));
                }
            }
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                }
            }
            out.push('\n');
            walk(out, &item.children, depth + 1);
        }
    }
    walk(out, &draft.items, 0);
    out.push('\n');
}

/// Renders the independent audit the reconciliation must answer.
fn render_critique(out: &mut String, critique: &PlanCritiqueOutput) {
    out.push_str("## Independent Critique (the audit the reconciliation must answer):\n");
    out.push_str(&format!("- **Verdict**: {}\n", critique.verdict.as_str()));
    let summary = critique.summary.trim();
    if !summary.is_empty() {
        out.push_str(&format!(
            "- **Summary**: {}\n",
            escape_xml_text(&single_line(summary))
        ));
    }
    for finding in &critique.findings {
        let target = finding
            .target
            .as_deref()
            .map(single_line)
            .filter(|value| !value.is_empty())
            .map(|value| format!("#{value}"))
            .unwrap_or_else(|| "(whole draft)".to_string());
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            finding.category.as_str(),
            target,
            escape_xml_text(&single_line(&finding.detail))
        ));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        Plan::from_parts(
            "two-phase".to_string(),
            "Two-Phase Planning".to_string(),
            Some("Draft, critique, and reconcile.".to_string()),
            0,
            0,
            vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Independent Critique".to_string(),
                details: Some("The critic never rewrites the draft.".to_string()),
                children: vec![],
            }],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Shared DTOs".to_string(),
                status: PlanItemStatus::Done,
                files: vec!["crates/protocol/src/plan/dto.rs".to_string()],
                ..Default::default()
            }],
        )
    }

    fn draft() -> PlanRefinementOutput {
        PlanRefinementOutput {
            status: "success".to_string(),
            explanation: "Decomposed into the prompt surface and its schema.".to_string(),
            reason: None,
            abort: None,
            context_requests: Vec::new(),
            repo_searches: Vec::new(),
            repo_maps: Vec::new(),
            items: vec![PlanItem {
                id: "2".to_string(),
                title: "Two-phase prompt surface".to_string(),
                status: PlanItemStatus::Todo,
                children: vec![
                    PlanItem {
                        id: "2.1".to_string(),
                        title: "Critique prompts".to_string(),
                        details: Some("System and user prompts plus the schema.".to_string()),
                        status: PlanItemStatus::Todo,
                        files: vec!["crates/core/src/plan/prompt/critique.rs".to_string()],
                        readonly_files: vec!["crates/core/src/plan/prompt/refine.rs".to_string()],
                        concept_refs: vec!["c-1".to_string()],
                        depends_on: vec!["1".to_string()],
                        ..Default::default()
                    },
                    PlanItem {
                        id: "2.2".to_string(),
                        title: "Facade wiring".to_string(),
                        status: PlanItemStatus::Todo,
                        files: vec!["crates/core/src/plan/prompt.rs".to_string()],
                        concept_refs: vec!["c-1".to_string()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
        }
    }

    fn critique() -> PlanCritiqueOutput {
        PlanCritiqueOutput {
            verdict: PlanCritiqueVerdict::ChangesRequired,
            summary: "The horizon misses the schema test.".to_string(),
            findings: vec![PlanCritiqueFinding {
                category: PlanCritiqueCategory::InvariantGap,
                target: Some("2.1".to_string()),
                detail: "Add a test that validates the schema.".to_string(),
            }],
        }
    }

    #[test]
    fn the_critique_system_prompt_names_every_category_and_the_read_only_contract() {
        let system = build_plan_critique_system_prompt();
        assert!(system.contains("Independent Plan Critique"));
        assert!(system.contains("Read-Only Audit Contract"));
        assert!(system.contains("never issue a plan of your own"));
        assert!(system.contains("You request no context"));
        for category in [
            PlanCritiqueCategory::InvariantGap,
            PlanCritiqueCategory::DependencyCycle,
            PlanCritiqueCategory::DecompositionDefect,
            PlanCritiqueCategory::ScopeViolation,
            PlanCritiqueCategory::ContractViolation,
        ] {
            assert!(
                system.contains(category.as_str()),
                "the system prompt names no {} category",
                category.as_str()
            );
        }
        assert!(system.contains("`approved`"));
        assert!(system.contains("`changes_required`"));
        assert!(system.contains("Strict Language Consistency Invariant"));
        assert!(system.contains("Final Content Invariant"));
    }

    #[test]
    fn the_critique_prompt_renders_the_requirement_the_draft_and_the_instructions() {
        let prompt = build_plan_critique_prompt(
            &sample_plan(),
            &draft(),
            Some("Introduce the two-phase pipeline"),
        );

        assert!(prompt.starts_with("# Independent Plan Critique: [two-phase] Two-Phase Planning"));
        assert!(prompt.contains("Draft, critique, and reconcile."));
        assert!(prompt.contains("## Developer Directives"));
        assert!(prompt.contains("Introduce the two-phase pipeline"));
        assert!(prompt
            .contains("- **#c-1** Independent Critique: The critic never rewrites the draft."));
        assert!(prompt.contains("- [x] #1 Shared DTOs (files: crates/protocol/src/plan/dto.rs)"));
        assert!(prompt.contains("- #2 Two-phase prompt surface (status: TODO)"));
        assert!(prompt.contains("  - #2.1 Critique prompts (status: TODO)"));
        assert!(prompt.contains("[concept_refs: c-1]"));
        assert!(prompt.contains("[files: crates/core/src/plan/prompt/critique.rs]"));
        assert!(prompt.contains("[readonly_files: crates/core/src/plan/prompt/refine.rs]"));
        assert!(prompt.contains("[depends_on: 1]"));
        assert!(prompt.contains(": System and user prompts plus the schema."));
        assert!(prompt.contains("## Critic Instructions:"));
    }

    #[test]
    fn the_critique_prompt_states_the_creation_case_and_a_missing_horizon() {
        let creation = Plan::from_parts(
            "fresh".to_string(),
            "Fresh Plan".to_string(),
            Some("Introduce the audit log.".to_string()),
            0,
            0,
            Vec::new(),
            Vec::new(),
        );
        let blocked = PlanRefinementOutput {
            status: "blocked".to_string(),
            explanation: String::new(),
            reason: Some("The retention window is undecided.".to_string()),
            abort: None,
            context_requests: Vec::new(),
            repo_searches: Vec::new(),
            repo_maps: Vec::new(),
            items: Vec::new(),
        };

        let prompt = build_plan_critique_prompt(&creation, &blocked, None);

        assert!(prompt.contains(
            "*(The plan carries no concept yet: the developer directives above are the requirement.)*"
        ));
        assert!(prompt.contains("- **Status**: blocked"));
        assert!(prompt.contains("- **Reported Reason**: The retention window is undecided."));
        assert!(prompt.contains("*(No steps were synthesized)*"));
        // No directives were supplied, so the section is not advertised at all.
        assert!(!prompt.contains("## Developer Directives"));
    }

    #[test]
    fn the_reconciliation_prompt_carries_the_audit_and_keeps_the_planner_ownership() {
        let prompt = build_plan_reconciliation_prompt(
            &sample_plan(),
            &draft(),
            &critique(),
            Some("Introduce the two-phase pipeline"),
        );

        assert!(prompt.starts_with("# Plan Reconciliation: [two-phase] Two-Phase Planning"));
        assert!(prompt.contains("## Immutable Architectural Concept"));
        assert!(prompt.contains("## Completed Foundation"));
        assert!(prompt.contains("## Previous Draft Horizon"));
        assert!(prompt.contains("  - #2.1 Critique prompts (status: TODO)"));
        assert!(prompt.contains("## Independent Critique"));
        assert!(prompt.contains("- **Verdict**: changes_required"));
        assert!(prompt.contains("- **Summary**: The horizon misses the schema test."));
        assert!(prompt.contains("- [invariant_gap] #2.1: Add a test that validates the schema."));
        // The Planner adjudicates the audit instead of obeying it.
        assert!(prompt.contains("Adjudicate every finding"));
        assert!(prompt.contains("reject it with the reason"));
        assert!(prompt.contains("Re-emit the WHOLE unfinished horizon"));
        assert!(prompt.contains("`plan_refinement`"));
        assert!(prompt.contains("Language Directive"));
        assert!(prompt.contains("Final Content Invariant"));
    }

    #[test]
    fn the_prompts_keep_headers_on_one_line_and_escape_author_text() {
        let plan = Plan::from_parts(
            "escaped".to_string(),
            "Audit <plan> &\nhorizon".to_string(),
            Some("Draft <edits> & reconcile".to_string()),
            0,
            0,
            Vec::new(),
            Vec::new(),
        );

        let prompt =
            build_plan_critique_prompt(&plan, &draft(), Some("Keep <edits> & <context> intact"));

        assert!(prompt.contains("# Independent Plan Critique: [escaped] Audit <plan> & horizon"));
        assert!(prompt.contains("Draft &lt;edits&gt; &amp; reconcile"));
        assert!(prompt.contains("Keep &lt;edits&gt; &amp; &lt;context&gt; intact"));
    }

    #[test]
    fn the_critique_schema_is_strict_and_closed() {
        let schema = plan_critique_response_schema();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("the schema declares its required fields")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(required, ["verdict", "summary", "findings"]);

        let finding = &schema["properties"]["findings"]["items"];
        assert_eq!(finding["additionalProperties"], serde_json::json!(false));
        let finding_required: Vec<&str> = finding["required"]
            .as_array()
            .expect("a finding declares its required fields")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(finding_required, ["category", "target", "detail"]);
        assert_eq!(
            finding["properties"]["target"]["anyOf"][0]["type"],
            "string"
        );
        assert_eq!(finding["properties"]["target"]["anyOf"][1]["type"], "null");
        assert!(
            !schema.to_string().contains("$ref"),
            "the schema must not reference itself"
        );

        let definition = plan_critique_response_schema_definition();
        assert_eq!(definition["name"], "plan_critique");
        assert_eq!(definition["strict"], serde_json::json!(true));
        assert_eq!(definition["schema"], schema);
    }

    #[test]
    fn the_wire_spelling_of_the_categories_and_verdicts_matches_the_schema() {
        let schema = plan_critique_response_schema();
        let finding = &schema["properties"]["findings"]["items"];
        let categories: Vec<&str> = finding["properties"]["category"]["enum"]
            .as_array()
            .expect("the category enumeration")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        let declared = [
            PlanCritiqueCategory::InvariantGap,
            PlanCritiqueCategory::DependencyCycle,
            PlanCritiqueCategory::DecompositionDefect,
            PlanCritiqueCategory::ScopeViolation,
            PlanCritiqueCategory::ContractViolation,
        ];
        assert_eq!(categories.len(), declared.len());
        for category in declared {
            assert!(
                categories.contains(&category.as_str()),
                "the schema does not enumerate {}",
                category.as_str()
            );
        }

        let verdicts: Vec<&str> = schema["properties"]["verdict"]["enum"]
            .as_array()
            .expect("the verdict enumeration")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert_eq!(
            verdicts,
            [
                PlanCritiqueVerdict::Approved.as_str(),
                PlanCritiqueVerdict::ChangesRequired.as_str()
            ]
        );
    }

    #[test]
    fn parse_plan_critique_output_decodes_fences_wrapper_prose_and_absent_targets() {
        let json = r#"{
            "verdict": "changes_required",
            "summary": "The horizon breaks the dependency order",
            "findings": [
                {"category": "dependency_cycle", "target": "2.1", "detail": "Step 2.1 depends on its own ancestor"},
                {"category": "contract_violation", "target": null, "detail": "Step identifiers repeat"}
            ]
        }"#;

        let fenced = format!("Here is the audit:\n```json\n{json}\n```\nLet me know.");
        let parsed = parse_plan_critique_output(&fenced).unwrap();
        assert_eq!(parsed.verdict, PlanCritiqueVerdict::ChangesRequired);
        assert!(!parsed.is_approved());
        assert_eq!(parsed.findings.len(), 2);
        assert_eq!(
            parsed.findings[0].category,
            PlanCritiqueCategory::DependencyCycle
        );
        assert_eq!(parsed.findings[0].target.as_deref(), Some("2.1"));
        assert!(parsed.findings[1].target.is_none());

        let bare = format!("Preamble without a fence. {json} Trailing notes.");
        assert_eq!(parse_plan_critique_output(&bare).unwrap().findings.len(), 2);

        let approved = r#"{"verdict": "approved", "summary": "Clean", "findings": []}"#;
        let parsed = parse_plan_critique_output(approved).unwrap();
        assert!(parsed.is_approved());
        assert!(parsed.findings.is_empty());

        assert!(parse_plan_critique_output("not json at all").is_err());
        assert!(parse_plan_critique_output("").is_err());
    }

    #[test]
    fn parse_plan_critique_output_rejects_a_payload_outside_the_contract() {
        let unknown_category = r#"{
            "verdict": "changes_required",
            "summary": "x",
            "findings": [{"category": "mystery", "target": null, "detail": "y"}]
        }"#;
        assert!(parse_plan_critique_output(unknown_category).is_err());

        let unknown_verdict = r#"{"verdict": "maybe", "summary": "x", "findings": []}"#;
        assert!(parse_plan_critique_output(unknown_verdict).is_err());

        let missing_verdict = r#"{"summary": "x", "findings": []}"#;
        assert!(parse_plan_critique_output(missing_verdict).is_err());
    }
}
