use crate::context::ContextFileContent;
use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use serde::{Deserialize, Serialize};
use tauqe_protocol::{ConceptItem, Plan, PlanItem, PlanItemStatus};

pub const PLAN_STEP_EXECUTION_PREFIX: &str = "# Target Plan Step Execution:";

pub const PLAN_STEP_RETRY_PREFIX: &str = "# Correction Required: Plan Step Turn Incomplete";

pub const PLAN_DISCUSSION_PREFIX: &str = "# Plan Discussion:";

/// Returns true if the prompt represents an explicit plan discussion turn.
pub fn is_plan_discussion_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(PLAN_DISCUSSION_PREFIX)
}

/// Formats a targeted prompt for discussing and refining a plan or specific marked items.
///
/// Preserves session history and passes the targeted plan structure, focused items,
/// and developer question or instructions for architectural refinement.
pub fn format_plan_discussion_prompt(
    plan: &Plan,
    focused_item_ids: &[String],
    user_comment: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        PLAN_DISCUSSION_PREFIX,
        single_line(&plan.id),
        single_line(&plan.title)
    ));

    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Plan Architecture & Context:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
        }
    }

    out.push_str("## Target Plan Context:\n");
    let (total, completed) = plan.count_stats();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        escape_xml_attr(&plan.id),
        escape_xml_attr(&plan.title),
        completed,
        total
    ));
    if let Some(desc) = &plan.description {
        out.push_str(&format!(
            "  <description>{}</description>\n",
            escape_xml_text(desc.trim())
        ));
    }

    if !plan.concept.is_empty() {
        out.push_str("  <concept>\n");
        fn render_concept_xml(
            out: &mut String,
            items: &[ConceptItem],
            depth: usize,
            focused_item_ids: &[String],
        ) {
            let indent = "  ".repeat(depth + 2);
            for item in items {
                let is_focused =
                    focused_item_ids.iter().any(|id| id.eq_ignore_ascii_case(&item.id));
                let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
                out.push_str(&format!(
                    "{}- #{}{} {}",
                    indent,
                    item.id,
                    focus_marker,
                    escape_xml_text(&item.title)
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                    }
                }
                out.push('\n');
                if !item.children.is_empty() {
                    render_concept_xml(out, &item.children, depth + 1, focused_item_ids);
                }
            }
        }
        render_concept_xml(&mut out, &plan.concept, 0, focused_item_ids);
        out.push_str("  </concept>\n");
    }

    fn render_discussion_items(
        out: &mut String,
        items: &[PlanItem],
        depth: usize,
        focused_item_ids: &[String],
    ) {
        let indent = "  ".repeat(depth + 1);
        for item in items {
            let is_focused =
                focused_item_ids.iter().any(|id| id.eq_ignore_ascii_case(&item.id));
            let focus_marker = if is_focused { " [FOCUSED]" } else { "" };
            out.push_str(&format!(
                "{}- {}{} (id: \"{}\", status: {})",
                indent,
                escape_xml_text(&item.title),
                focus_marker,
                item.id,
                item.status
            ));
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(": {}", escape_xml_text(trimmed)));
                }
            }
            out.push('\n');
            if !item.children.is_empty() {
                render_discussion_items(out, &item.children, depth + 1, focused_item_ids);
            }
        }
    }
    render_discussion_items(&mut out, &plan.items, 0, focused_item_ids);
    out.push_str("</active_plan>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = plan.find_item(id) {
                out.push_str(&format!(
                    "- **#{} {}** (status: {})\n",
                    item.id,
                    escape_xml_text(&item.title),
                    item.status
                ));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", escape_xml_text(trimmed)));
                    }
                }
            } else if let Some(concept) = plan.find_concept_item(id) {
                out.push_str(&format!(
                    "- **[CONCEPT] #{} {}**\n",
                    concept.id,
                    escape_xml_text(&concept.title)
                ));
                if let Some(details) = &concept.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("  {}\n", escape_xml_text(trimmed)));
                    }
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the plan architecture and steps.\n");
    out.push_str("2. Reins Invariant: Never mutate the plan (keep `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`.\n");
    out.push_str("3. When explicitly instructed to update the plan, explain the proposed modifications in `message` and populate the structured `plan_update` field.\n");
    out.push_str("4. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite the plan (mandatory `concept` tree defining architectural intent, with optional implementation `items`). Use `action: \"update\"` to modify individual existing items or concept nodes by ID without altering unmentioned items. Use `action: \"delete\"` to remove the plan.\n");
    out.push_str("5. Keep explanations clear, rigorous, and directly aligned with codebase design.\n");
    out.push_str("6. Language Consistency Directive: Detect the natural language of the plan title and description. Formulate all plan titles, descriptions, step titles, step details, and conversational explanations strictly in that same language.\n");

    out
}

/// Formats the plan, its concept tree, and implementation items into a prompt block for LLM context.
pub fn format_active_plan_context(plan: &Plan) -> Option<String> {
    if plan.concept.is_empty() && plan.items.is_empty() {
        return None;
    }

    let (total, completed) = plan.count_stats();
    let mut out = String::new();
    out.push_str(&format!(
        "<active_plan id=\"{}\" title=\"{}\" progress=\"{}/{} completed\">\n",
        escape_xml_attr(&plan.id),
        escape_xml_attr(&plan.title),
        completed,
        total
    ));

    if let Some(desc) = &plan.description {
        out.push_str(&format!("  <description>{}</description>\n", desc.trim()));
    }

    if !plan.concept.is_empty() {
        out.push_str("  <concept>\n");
        fn render_concept(out: &mut String, items: &[tauqe_protocol::ConceptItem], depth: usize) {
            let indent = "  ".repeat(depth + 2);
            for item in items {
                out.push_str(&format!("{}- {} (id: \"{}\")", indent, item.title, item.id));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                if !item.children.is_empty() {
                    render_concept(out, &item.children, depth + 1);
                }
            }
        }
        render_concept(&mut out, &plan.concept, 0);
        out.push_str("  </concept>\n");
    }

    if !plan.items.is_empty() {
        fn render_items(out: &mut String, items: &[PlanItem], depth: usize) {
            let indent = "  ".repeat(depth + 1);
            for item in items {
                let status_mark = match item.status {
                    PlanItemStatus::Discussion => "DISCUSSION",
                    PlanItemStatus::Todo => "TODO",
                    PlanItemStatus::InProgress => "IN_PROGRESS",
                    PlanItemStatus::Done => "DONE",
                    PlanItemStatus::Blocked => "BLOCKED",
                    PlanItemStatus::Cancelled => "CANCELLED",
                };

                out.push_str(&format!(
                    "{}- {} (id: \"{}\", status: {})",
                    indent, item.title, item.id, status_mark
                ));

                if !item.concept_refs.is_empty() {
                    out.push_str(&format!(" [concept_refs: {}]", item.concept_refs.join(", ")));
                }
                if !item.files.is_empty() {
                    out.push_str(&format!(" [files: {}]", item.files.join(", ")));
                }
                if !item.depends_on.is_empty() {
                    out.push_str(&format!(" [depends_on: {}]", item.depends_on.join(", ")));
                }

                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');

                if !item.children.is_empty() {
                    render_items(out, &item.children, depth + 1);
                }
            }
        }

        render_items(&mut out, &plan.items, 0);
    }
    out.push_str("</active_plan>");

    Some(out)
}

/// Output of deep plan refinement generated by the architect model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRefinementOutput {
    pub status: String,
    pub explanation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_requests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PlanItem>,
}

/// Parses the raw JSON string returned by the model into a `PlanRefinementOutput`.
pub fn parse_plan_refinement_output(raw_json: &str) -> anyhow::Result<PlanRefinementOutput> {
    let trimmed = raw_json.trim();
    let cleaned = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let cleaned = cleaned
        .strip_suffix("```")
        .unwrap_or(cleaned)
        .trim();
    serde_json::from_str(cleaned)
        .map_err(|e| anyhow::anyhow!("Failed to parse plan refinement JSON output: {e}"))
}

/// Maximum supported hierarchy nesting depth for structured plan refinement output.
pub const MAX_PLAN_REFINEMENT_DEPTH: usize = 5;

/// Target size range (in changed lines) for one cohesive leaf implementation step.
const LEAF_STEP_TARGET_LINES: (usize, usize) = (150, 400);

/// Helper that builds an unrolled, non-recursive PlanItem JSON Schema up to `max_depth`.
fn plan_item_schema(depth: usize, max_depth: usize) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "id".to_string(),
        serde_json::json!({
            "type": "string",
            "description": "Step identifier (e.g. '1', '1.1', '2')"
        }),
    );
    properties.insert(
        "title".to_string(),
        serde_json::json!({
            "type": "string",
            "description": "Concise step title formulated strictly in the same natural language as the plan description"
        }),
    );
    properties.insert(
        "details".to_string(),
        serde_json::json!({
            "anyOf": [
                { "type": "string" },
                { "type": "null" }
            ],
            "description": "Concrete technical details in the same language as the plan description: target files, functions, contracts, verification criteria"
        }),
    );
    properties.insert(
        "status".to_string(),
        serde_json::json!({
            "type": "string",
            "enum": ["discussion", "todo", "in_progress", "done", "cancelled"],
            "description": "Step status"
        }),
    );
    properties.insert(
        "files".to_string(),
        serde_json::json!({
            "type": "array",
            "description": "Repository-relative file paths that this step creates, modifies, or inspects (pre-loaded into context when executing). Invariant: files must only be specified for executable leaf steps without children; parent container items must have an empty files list.",
            "items": { "type": "string" }
        }),
    );
    properties.insert(
        "readonly_files".to_string(),
        serde_json::json!({
            "type": "array",
            "description": "Repository-relative file paths for read-only reference context (subsystem conventions like Conventions.md, interface contracts, protocol types). Invariant: readonly_files must only be specified for executable leaf steps without children; parent container items must have an empty readonly_files list.",
            "items": { "type": "string" }
        }),
    );
    properties.insert(
        "concept_refs".to_string(),
        serde_json::json!({
            "type": "array",
            "description": "IDs of concept items from the architectural concept tree that this step addresses or implements",
            "items": { "type": "string" }
        }),
    );
    properties.insert(
        "depends_on".to_string(),
        serde_json::json!({
            "type": "array",
            "description": "IDs of prerequisite steps that must reach DONE status before this step can execute (DAG edges). Only reference earlier steps. Must be empty for steps without prerequisites. Invariant: depends_on must only be specified for executable leaf steps without children; parent container items must have an empty depends_on list.",
            "items": { "type": "string" }
        }),
    );

    let mut required = vec![
        serde_json::Value::String("id".to_string()),
        serde_json::Value::String("title".to_string()),
        serde_json::Value::String("details".to_string()),
        serde_json::Value::String("status".to_string()),
        serde_json::Value::String("files".to_string()),
        serde_json::Value::String("readonly_files".to_string()),
        serde_json::Value::String("concept_refs".to_string()),
        serde_json::Value::String("depends_on".to_string()),
    ];

    if depth < max_depth {
        properties.insert(
            "children".to_string(),
            serde_json::json!({
                "type": "array",
                "description": "Sub-steps or child items",
                "items": plan_item_schema(depth + 1, max_depth)
            }),
        );
        required.push(serde_json::Value::String("children".to_string()));
    }

    serde_json::json!({
        "type": "object",
        "description": if depth < max_depth {
            "A refined step or item in the plan"
        } else {
            "A leaf step at maximum hierarchy depth (cannot have children)"
        },
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Strict JSON Schema for deep plan refinement output.
///
/// Unrolls nested plan items up to `MAX_PLAN_REFINEMENT_DEPTH` to guarantee
/// compliance with LLM Structured Output engines that forbid recursive `$ref` schemas.
pub fn plan_refine_response_schema() -> serde_json::Value {
    let items_description = format!(
        "New unfinished horizon: the hierarchical list of NEW executable steps (up to {} levels of nesting). Do NOT include already-DONE steps; the harness preserves and prepends the completed foundation. If status is 'blocked' or 'needs_context', this must be empty.",
        MAX_PLAN_REFINEMENT_DEPTH
    );
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["success", "blocked", "needs_context"],
                "description": "Result status: 'success' if the unfinished horizon was synthesized into actionable steps, 'blocked' if architectural blockers exist, or 'needs_context' if specific source files from repo_map must be inspected before designing the roadmap."
            },
            "explanation": {
                "type": "string",
                "description": "Architectural breakdown, decomposition strategy, dependency analysis, and technical rationale strictly in the same language as the plan description."
            },
            "reason": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "null" }
                ],
                "description": "Required when status is 'blocked': detailed technical explanation in the same language as the plan description of the contradiction, missing prerequisite, or codebase blocker."
            },
            "context_requests": {
                "type": "array",
                "description": "Required when status is 'needs_context': repository-relative file paths from <repo_map> that need to be read to verify contracts and inspect implementation details.",
                "items": {
                    "type": "string",
                    "description": "Repository-relative file path"
                }
            },
            "items": {
                "type": "array",
                "description": items_description,
                "items": plan_item_schema(1, MAX_PLAN_REFINEMENT_DEPTH)
            }
        },
        "required": ["status", "explanation", "reason", "context_requests", "items"],
        "additionalProperties": false
    })
}

/// JSON Schema definition wrapper for provider configuration.
pub fn plan_refine_response_schema_definition() -> serde_json::Value {
    serde_json::json!({
        "name": "plan_refinement",
        "description": "Plan horizon synthesis output: the new unfinished steps or a blocker report",
        "strict": true,
        "schema": plan_refine_response_schema()
    })
}

/// Builds the unified system prompt for Architect horizon synthesis.
///
/// One prompt governs every Architect planning call — initial refinement, manual
/// re-projection, and automatic recovery after a blocked step — because all three
/// are the same operation: reprojecting the unfinished horizon from the immutable
/// concept and the completed (DONE) foundation. The prompt therefore never asks the
/// Architect to restate finished work; it demands only the new, unfinished steps.
pub fn build_plan_horizon_system_prompt() -> String {
    let mut out = String::new();
    out.push_str("You are a Principal Software Architect in TAUQE (The Answer to the Ultimate Question of Engineering).\n\n");
    out.push_str("Your mission is Wave Horizon Planning: synthesizing the entire unfinished horizon of a plan — the ordered, executable steps that still remain — from an immutable architectural concept, the completed (DONE) foundation, and, when present, the diagnostics of a blocked step.\n\n");

    out.push_str("## Core Invariants & Rules:\n\n");
    out.push_str("1. Concept Immutability (\"What\" vs \"How\"):\n");
    out.push_str("   - The architectural concept tree (`concept`) captures the developer's intent and is the immutable source of truth.\n");
    out.push_str("   - You MUST NOT alter, expand, dilute, or contradict it.\n");
    out.push_str("   - Your exclusive responsibility is engineering decomposition (\"How\"): the exact modules, interfaces, contracts, types, and sequence of implementation.\n\n");
    out.push_str("2. Horizon Synthesis Contract:\n");
    out.push_str("   - Return ONLY the new, unfinished steps of the horizon; every returned step carries `status: \"todo\"`.\n");
    out.push_str("   - Do NOT restate, re-emit, or reorder the completed (DONE) steps shown to you: the harness preserves them verbatim and concatenates them ahead of your output. Emitting a DONE step is a hard contract violation.\n");
    out.push_str("   - Reproject the WHOLE remaining horizon on every call: the horizon is a fresh, coherent plan for the remaining work, not a patch layered on an earlier draft.\n\n");
    out.push_str("3. Strict Blocker Protocol (Refusal to Plan):\n");
    out.push_str("   - If you discover an insurmountable contradiction, technical blocker, missing prerequisite, or flawed assumption in the codebase or requirements:\n");
    out.push_str("   - You MUST NOT invent a compromised, broken, or speculative plan.\n");
    out.push_str("   - You MUST set `status: \"blocked\"`, provide empty `items: []`, and document a comprehensive, rigorous technical explanation in `reason` detailing the exact contradiction, codebase evidence, and recommended developer action.\n\n");
    out.push_str("4. Step Granularity, Grouping and Compilability Invariants:\n");
    out.push_str("   - Execution Lifecycle Contract: EVERY leaf step is executed in an isolated turn with its own toolchain verification gates (check, clippy, test).\n");
    out.push_str("   - Target File Precision (Leaf-Only Files Invariant): For every executable leaf step, you MUST populate `files: [\"path/to/file\"]` with exact repository-relative file paths. Parent items and task groups are conceptual containers and MUST have empty `files: []`. Only leaf steps (`children: []`) pre-load files into context upon execution.\n");
    out.push_str("   - Reference Files Contract (Read-Only Context): For every executable leaf step, identify and populate `readonly_files: [\"path/to/reference\"]` with existing interfaces, protocol types, or subsystem conventions needed as context during implementation. These files are pre-loaded as read-only context during step execution. Invariant: parent items must have empty `readonly_files: []`.\n");
    out.push_str("   - Concept Traceability: For each step, link it to the relevant architectural concept IDs via `concept_refs: [\"concept_id\"]`.\n");
    out.push_str("   - DAG Dependency Contract: For every executable leaf step, declare prerequisite step IDs in `depends_on: [\"step_id\"]`. A step runs only after every dependency reaches DONE status. Reference only steps within this horizon; steps without prerequisites must use `depends_on: []`.\n");
    out.push_str("   - Avoid Micro-Fragmentation: Strictly avoid splitting adjacent, closely related edits into separate microscopic steps. Group cohesive changes across 1-4 related files under a single leaf step if they form a logical unit of work and carry no risk of patch conflicts.\n");
    out.push_str(&format!(
        "   - Avoid Unmanageable Monoliths: Do not bundle numerous unrelated files into a single step; keep each leaf step calibrated to a cohesive unit (targeting {}-{} lines of change).\n",
        LEAF_STEP_TARGET_LINES.0, LEAF_STEP_TARGET_LINES.1
    ));
    out.push_str("   - Compilability Invariant: The codebase MUST remain in a compiling, fully verified state at the end of EVERY leaf step. Never create broken intermediate steps.\n");
    out.push_str("   - Order steps strictly by prerequisite dependencies.\n");
    out.push_str(&format!(
        "   - The plan hierarchy supports up to {} levels of nesting. Leaf steps at the maximum depth must not contain sub-items.\n\n",
        MAX_PLAN_REFINEMENT_DEPTH
    ));
    out.push_str("5. Strict Language Consistency Invariant:\n");
    out.push_str("   - Detect the primary natural language used in the plan's title and description.\n");
    out.push_str("   - You MUST formulate all titles, step details, explanations, and reasons strictly in that SAME natural language.\n");
    out.push_str("   - Do NOT switch languages unless the plan itself is written in that language.\n\n");
    out.push_str("6. Codebase Exploration & Discovery Contract:\n");
    out.push_str("   - You have access to symbol contours in `<repo_map>` and already loaded files in `Active Context Files`.\n");
    out.push_str("   - If you need to inspect exact implementations, contracts, existing toolchains, or interfaces before synthesizing the horizon, set `status: \"needs_context\"`, specify the repository-relative file paths in `context_requests: [...]`, leave `items: []`, and explain in `explanation` what you are analyzing and why these files are required.\n");
    out.push_str("   - When all required context files are loaded, set `status: \"success\"` (or `status: \"blocked\"` if blockers found), set `context_requests: []`, and provide the synthesized `items`.\n\n");
    out.push_str("7. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema.\n");
    out
}

/// Diagnostics describing a failed or blocked plan step that triggered horizon recovery.
///
/// The unified horizon synthesis prompt carries these diagnostics so the Architect can
/// reproject the unfinished horizon around the failure: the blocked step identifier, the
/// coder's blocker report, and the last observed toolchain status.
#[derive(Debug, Clone)]
pub struct PlanHorizonDiagnostics<'a> {
    pub blocked_step_id: &'a str,
    pub blocked_reason: Option<&'a str>,
    pub toolchain_status: Option<&'a str>,
}

/// Builds the unified user prompt for Architect horizon synthesis.
///
/// Structures the input in a fixed order: the immutable architectural concept, the
/// completed (DONE) foundation, the optional diagnostics of a blocked step, the codebase
/// contours (`<repo_map>`) and active context files, and the developer's directives. The
/// Architect returns only the NEW unfinished horizon; the harness preserves the DONE
/// foundation and concatenates it ahead of the synthesized steps.
pub fn build_plan_horizon_user_prompt(
    plan: &Plan,
    context_files: &[ContextFileContent],
    repo_map: Option<&str>,
    diagnostics: Option<PlanHorizonDiagnostics<'_>>,
    instructions: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Plan Horizon Synthesis: [{}] {}\n\n",
        plan.id, plan.title
    ));

    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str("## High-Level Architectural Intent (\"What\"):\n");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    if !plan.concept.is_empty() {
        out.push_str("## Architectural Concept (\"What\" - Immutable Source of Truth):\n");
        fn render_concept(out: &mut String, items: &[ConceptItem], depth: usize) {
            let indent = "  ".repeat(depth);
            for item in items {
                out.push_str(&format!("{}- **#{}** {}", indent, item.id, item.title));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                render_concept(out, &item.children, depth + 1);
            }
        }
        render_concept(&mut out, &plan.concept, 0);
        out.push('\n');
    }

    out.push_str("## Completed Foundation (DONE steps - immutable; do NOT repeat them in `items`):\n");
    fn collect_done<'a>(items: &'a [PlanItem], acc: &mut Vec<&'a PlanItem>) {
        for item in items {
            if item.status == PlanItemStatus::Done {
                acc.push(item);
            }
            collect_done(&item.children, acc);
        }
    }
    let mut done = Vec::new();
    collect_done(&plan.items, &mut done);
    if done.is_empty() {
        out.push_str("*(No completed steps yet)*\n");
    } else {
        for item in &done {
            out.push_str(&format!("- [x] #{} {}", item.id, item.title));
            if !item.files.is_empty() {
                out.push_str(&format!(" (files: {})", item.files.join(", ")));
            }
            out.push('\n');
        }
    }
    out.push('\n');

    if let Some(diag) = diagnostics {
        out.push_str("## Blocked Step Diagnostics:\n");
        let blocked = plan.find_item(diag.blocked_step_id);
        match &blocked {
            Some(item) => out.push_str(&format!("- **Blocked Step**: #{} {}\n", item.id, item.title)),
            None => out.push_str(&format!("- **Blocked Step**: #{}\n", diag.blocked_step_id)),
        }
        if let Some(item) = &blocked {
            if !item.files.is_empty() {
                out.push_str(&format!(
                    "- **Declared Target Files**: {}\n",
                    item.files.join(", ")
                ));
            }
            if let Some(details) = &item.details {
                let trimmed = details.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!("- **Declared Contract**:\n{}\n", trimmed));
                }
            }
        }
        match diag.blocked_reason.map(str::trim).filter(|s| !s.is_empty()) {
            Some(reason) => out.push_str(&format!("- **Coder Blocker Report**: {}\n", reason)),
            None => out.push_str("- **Coder Blocker Report**: (no diagnostics provided)\n"),
        }
        if let Some(status) = diag.toolchain_status.map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("- **Toolchain Status**: {}\n", status));
        }
        out.push('\n');
    }

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
    out.push_str("1. Conduct deep technical dependency analysis using the codebase contours and context files.\n");
    out.push_str("2. Synthesize the COMPLETE unfinished horizon as dependency-ordered leaf steps with `status: \"todo\"`. Do NOT include any DONE step; the harness preserves the completed foundation verbatim and prepends it.\n");
    out.push_str("3. If you need to inspect specific file implementations before synthesizing, request them by setting `status: \"needs_context\"` with `context_requests: [\"path/to/file\"]` and empty `items: []`.\n");
    out.push_str("4. Check for any blockers, circular dependencies, or contradictions. If blocked, report `status: \"blocked\"` with an exact `reason`.\n");
    out.push_str("5. For every executable leaf step, specify target files in `files`, reference contracts/types/conventions in `readonly_files`, addressed concept IDs in `concept_refs`, and prerequisite step IDs in `depends_on`. Calibrate leaf step granularity to cohesive units: group closely related adjacent edits under one step to avoid micro-fragmentation, while avoiding monolithic multi-file dumps.\n");
    out.push_str("6. Language Directive: Detect the natural language of the plan title and description. Formulate all titles, details, explanations, and reasons strictly in that same language.\n");

    out
}

/// Target plan and step identifiers parsed from an isolated plan step execution prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStepTarget {
    pub plan_id: Option<String>,
    pub step_id: String,
}

/// Returns true if the prompt represents an isolated plan step execution turn.
pub fn is_plan_step_execution_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    trimmed.starts_with(PLAN_STEP_EXECUTION_PREFIX)
        || trimmed.starts_with(PLAN_STEP_RETRY_PREFIX)
}

/// Extracts the target plan ID and step ID from an isolated plan step execution prompt.
pub fn extract_plan_step_execution_target(prompt: &str) -> Option<PlanStepTarget> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with(PLAN_STEP_EXECUTION_PREFIX) {
        return None;
    }
    let rest = trimmed.strip_prefix(PLAN_STEP_EXECUTION_PREFIX)?.trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some((plan_id, step_id)) = spec.split_once(':') {
            let plan_id = plan_id.trim();
            let step_id = step_id.trim();
            if !step_id.is_empty() {
                return Some(PlanStepTarget {
                    plan_id: if plan_id.is_empty() { None } else { Some(plan_id.to_string()) },
                    step_id: step_id.to_string(),
                });
            }
        } else {
            return Some(PlanStepTarget {
                plan_id: None,
                step_id: spec.to_string(),
            });
        }
    }
    None
}

/// Extracts the target step ID from an isolated plan step execution prompt.
pub fn extract_plan_step_execution_id(prompt: &str) -> Option<String> {
    extract_plan_step_execution_target(prompt).map(|t| t.step_id)
}

/// Formats a targeted prompt for executing a specific leaf step of a plan in isolation.
///
/// Excludes unrelated chat history and focuses the model entirely on completing
/// the designated step, while providing architectural plan context and completed prerequisites.
pub fn format_plan_step_execution_prompt(
    plan: &Plan,
    step_id: &str,
    marker_suffix: &str,
) -> Result<String, String> {
    let step = plan
        .find_item(step_id)
        .ok_or_else(|| format!("Step '{}' not found in plan '{}'", step_id, plan.id))?;

    if !step.is_leaf() {
        return Err(format!(
            "Step '{}' ({}) is not a leaf task; execute individual sub-items instead",
            step.id, step.title
        ));
    }

    let mut out = String::new();
    let clean_title = step.title.replace(['\r', '\n'], " ").trim().to_string();
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        PLAN_STEP_EXECUTION_PREFIX, plan.id, step.id, clean_title
    ));

    out.push_str(&format!("## Plan: {}\n", plan.title));
    if let Some(desc) = &plan.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", trimmed));
        }
    }

    if !plan.concept.is_empty() {
        out.push_str("### Architectural Concept (System Invariants & Focus):\n");
        fn render_concept(out: &mut String, items: &[tauqe_protocol::ConceptItem], depth: usize) {
            let indent = "  ".repeat(depth);
            for item in items {
                out.push_str(&format!("{}- #{} {}", indent, item.id, item.title));
                if let Some(details) = &item.details {
                    let trimmed = details.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!(": {}", trimmed));
                    }
                }
                out.push('\n');
                render_concept(out, &item.children, depth + 1);
            }
        }
        render_concept(&mut out, &plan.concept, 0);
        out.push('\n');
    }

    let mut completed_items = Vec::new();
    fn collect_completed<'a>(items: &'a [PlanItem], acc: &mut Vec<(&'a str, &'a str)>) {
        for item in items {
            if item.status == PlanItemStatus::Done {
                acc.push((&item.id, &item.title));
            }
            collect_completed(&item.children, acc);
        }
    }
    collect_completed(&plan.items, &mut completed_items);

    if !completed_items.is_empty() {
        out.push_str("### Already Completed Prerequisites:\n");
        for (id, title) in completed_items {
            out.push_str(&format!("- [x] #{} {}\n", id, title));
        }
        out.push('\n');
    }

    out.push_str("### Target Step to Implement:\n");
    out.push_str(&format!("- **ID**: `{}`\n", step.id));
    out.push_str(&format!("- **Title**: {}\n", step.title));
    if !step.concept_refs.is_empty() {
        out.push_str(&format!("- **Addressed Concept Nodes**: {}\n", step.concept_refs.join(", ")));
    }
    if !step.files.is_empty() {
        out.push_str(&format!("- **Target Files (to modify)**: {}\n", step.files.join(", ")));
    }
    if !step.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Reference Files (read-only context)**: {}\n",
            step.readonly_files.join(", ")
        ));
    }
    if !step.depends_on.is_empty() {
        out.push_str(&format!(
            "- **Prerequisite Steps (must be DONE)**: {}\n",
            step.depends_on.join(", ")
        ));
    }
    if let Some(details) = &step.details {
        let trimmed = details.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Details**:\n{}\n", trimmed));
        }
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on implementing step `{}` while upholding the Architectural Concept invariants.\n",
        step.id
    ));
    out.push_str("2. Do NOT implement any upcoming or subsequent steps from the plan.\n");
    out.push_str("3. Keep changes minimal and coherent. Toolchain verification is handled automatically and deterministically by the harness; do NOT emit <verify> tags.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <success{0} summary=\"...\" />.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "5. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Request all necessary files together in a single turn to avoid exhausting discovery rounds. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));

    out.push_str(&format!(
        "6. If you encounter an insurmountable technical blocker, contradictory requirements, or need developer decisions, emit the terminal error tag: <error{0} message=\"...\" /> explaining the exact blocker.\n",
        marker_suffix
    ));

    Ok(out)
}

/// Builds a corrective reprompt for a plan step turn that ended without valid
/// code edits and without a terminal success tag.
///
/// The prompt echoes the model's previous response and optional diagnostic
/// feedback, then explicitly demands strict adherence to the active edit
/// protocol on the next turn.
pub fn format_plan_step_retry_prompt(assistant_text: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(PLAN_STEP_RETRY_PREFIX);
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
    out.push_str("1. If the step requires code changes, emit a valid edit block that strictly conforms to the active edit protocol.\n");
    out.push_str("2. If no code changes are required to complete the step, confirm completion explicitly by emitting the terminal success tag.\n");
    out.push_str("3. Do NOT replace protocol operations with conversational text, and do NOT end the turn without either a valid edit block or the terminal success tag.\n");

    out
}



#[cfg(test)]
mod tests {
    use super::*;
    fn sample_plan() -> Plan {
        Plan {
            id: "jwt-auth".to_string(),
            title: "JWT Authentication".to_string(),
            description: Some("Implement secure tokens".to_string()),
            created_at: 100,
            updated_at: 200,
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c-1".to_string(),
                title: "Stateless JWT".to_string(),
                details: Some("Use asymmetric ECDSA".to_string()),
                children: vec![],
            }],
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Models".to_string(),
                    details: Some("Claims struct".to_string()),
                    status: PlanItemStatus::Done,
                    concept_refs: vec!["c-1".to_string()],
                    files: vec!["crates/core/src/models.rs".to_string()],
                    readonly_files: vec![],
                    depends_on: vec![],
                    children: vec![],
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Endpoint".to_string(),
                    details: Some("Write handler".to_string()),
                    status: PlanItemStatus::InProgress,
                    concept_refs: vec!["c-1".to_string()],
                    files: vec!["crates/core/src/handler.rs".to_string()],
                    readonly_files: vec!["crates/core/src/contract.rs".to_string()],
                    depends_on: vec!["1".to_string()],
                    children: vec![],
                },
            ],
        }
    }

    #[test]
    fn test_format_active_plan_context() {
        let formatted = format_active_plan_context(&sample_plan()).unwrap();
        assert!(formatted.contains("<active_plan id=\"jwt-auth\""));
        assert!(formatted.contains("progress=\"1/2 completed\""));
        assert!(formatted.contains("Endpoint (id: \"2\", status: IN_PROGRESS)"));
        assert!(formatted.contains("[concept_refs: c-1]"));
        assert!(formatted.contains("[files: crates/core/src/handler.rs]"));
        assert!(formatted.contains("[depends_on: 1]"));
        assert!(formatted.contains(": Write handler"));
    }

    #[test]
    fn test_format_plan_step_execution_prompt() {
        let plan = sample_plan();

        let prompt = format_plan_step_execution_prompt(&plan, "2", "_XYZ").unwrap();
        assert!(prompt.contains("Target Plan Step Execution: [jwt-auth:2] Endpoint"));
        assert!(prompt.contains("Already Completed Prerequisites:"));
        assert!(prompt.contains("Architectural Concept (System Invariants & Focus):"));
        assert!(prompt.contains("- #c-1 Stateless JWT: Use asymmetric ECDSA"));
        assert!(prompt.contains("- **Target Files (to modify)**: crates/core/src/handler.rs"));
        assert!(prompt
            .contains("- **Reference Files (read-only context)**: crates/core/src/contract.rs"));
        assert!(prompt.contains("- **Addressed Concept Nodes**: c-1"));
        assert!(prompt.contains("- **Prerequisite Steps (must be DONE)**: 1"));

        assert!(is_plan_step_execution_prompt(&format!(" \n\t{}", prompt)));
        let target = extract_plan_step_execution_target(&prompt).unwrap();
        assert_eq!(target.plan_id.as_deref(), Some("jwt-auth"));
        assert_eq!(target.step_id, "2");
        let ws_target = extract_plan_step_execution_target(&format!("  \n  {}", prompt)).unwrap();
        assert_eq!(ws_target.plan_id.as_deref(), Some("jwt-auth"));
        assert_eq!(ws_target.step_id, "2");
        assert!(prompt.contains("- [x] #1 Models"));
        assert!(prompt.contains("- **ID**: `2`"));
        assert!(prompt.contains("<success_XYZ summary=\"...\" />"));
        assert!(prompt.contains("<error_XYZ message=\"...\" />"));
    }

    #[test]
    fn test_format_plan_step_retry_prompt() {
        let prompt =
            format_plan_step_retry_prompt("I think we are done.", Some("No edits were applied"));
        assert!(is_plan_step_execution_prompt(&prompt));
        assert!(prompt.contains("Correction Required"));
        assert!(prompt.contains("<previous_response>"));
        assert!(prompt.contains("I think we are done."));
        assert!(prompt.contains("No edits were applied"));
        assert!(prompt.contains("terminal success tag"));

        let empty = format_plan_step_retry_prompt("   ", None);
        assert!(empty.contains("(empty response)"));
        assert!(!empty.contains("<previous_response>"));
        assert!(!empty.contains("Diagnostic Feedback"));
    }

    #[test]
    fn test_plan_horizon_prompts_and_schema() {
        let plan = sample_plan();
        let sys = build_plan_horizon_system_prompt();
        assert!(sys.contains("Wave Horizon Planning"));
        assert!(sys.contains("Concept Immutability"));
        assert!(sys.contains("Horizon Synthesis Contract"));
        assert!(sys.contains("Strict Blocker Protocol"));
        assert!(sys.contains("Compilability Invariants"));
        assert!(sys.contains("Leaf-Only Files Invariant"));
        assert!(sys.contains("Reference Files Contract"));
        assert!(sys.contains("DAG Dependency Contract"));

        let ctx_files = vec![ContextFileContent {
            path: "crates/core/src/lib.rs".to_string(),
            content: "pub fn init() {}".to_string(),
            access: tauqe_protocol::ContextAccess::ReadOnly,
        }];
        let user_prompt = build_plan_horizon_user_prompt(
            &plan,
            &ctx_files,
            Some("fn generate_token()"),
            None,
            Some("Focus on EdDSA"),
        );
        assert!(user_prompt.contains("# Plan Horizon Synthesis: [jwt-auth] JWT Authentication"));
        assert!(user_prompt.contains("Immutable Source of Truth"));
        assert!(user_prompt.contains("## Completed Foundation"));
        assert!(user_prompt.contains("- [x] #1 Models"));
        assert!(user_prompt.contains("Focus on EdDSA"));
        assert!(user_prompt.contains("crates/core/src/lib.rs"));
        assert!(user_prompt.contains("fn generate_token()"));
        assert!(user_prompt.contains("readonly_files"));

        let schema = plan_refine_response_schema();
        assert!(schema.get("properties").is_some());
        assert!(!schema.to_string().contains("$ref"), "Schema must not contain recursive $ref");
        assert!(schema.to_string().contains("readonly_files"));
        assert!(schema.to_string().contains("depends_on"));
        assert!(schema.to_string().contains("New unfinished horizon"));
        let def = plan_refine_response_schema_definition();
        assert_eq!(def["name"], "plan_refinement");

        let raw_success = r#"{
            "status": "success",
            "explanation": "Decomposed into model and endpoint steps",
            "reason": null,
            "items": [
                {
                    "id": "1",
                    "title": "Model definition",
                    "details": "JWT claims struct",
                    "status": "todo",
                    "files": ["crates/core/src/models.rs"],
                    "readonly_files": ["crates/core/src/contract.rs"],
                    "children": []
                }
            ]
        }"#;
        let parsed = parse_plan_refinement_output(raw_success).unwrap();
        assert_eq!(parsed.status, "success");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].id, "1");
        assert_eq!(parsed.items[0].readonly_files, vec!["crates/core/src/contract.rs"]);

        let raw_blocked = r#"```json
        {
            "status": "blocked",
            "explanation": "Cannot proceed",
            "reason": "Missing cryptographic library in cargo dependencies",
            "context_requests": [],
            "items": []
        }
        ```"#;
        let parsed_blocked = parse_plan_refinement_output(raw_blocked).unwrap();
        assert_eq!(parsed_blocked.status, "blocked");
        assert_eq!(
            parsed_blocked.reason.as_deref(),
            Some("Missing cryptographic library in cargo dependencies")
        );
        assert!(parsed_blocked.items.is_empty());

        let raw_needs_ctx = r#"{
            "status": "needs_context",
            "explanation": "Need to inspect toolchain module",
            "reason": null,
            "context_requests": ["crates/core/src/toolchain.rs"],
            "items": []
        }"#;
        let parsed_ctx = parse_plan_refinement_output(raw_needs_ctx).unwrap();
        assert_eq!(parsed_ctx.status, "needs_context");
        assert_eq!(parsed_ctx.context_requests, vec!["crates/core/src/toolchain.rs"]);
        assert!(parsed_ctx.items.is_empty());

        let raw_deep = r#"{
            "status": "success",
            "explanation": "Deep 5-level decomposition",
            "reason": null,
            "items": [
                {
                    "id": "1",
                    "title": "Level 1",
                    "details": null,
                    "status": "todo",
                    "children": [
                        {
                            "id": "1.1",
                            "title": "Level 2",
                            "details": null,
                            "status": "todo",
                            "children": [
                                {
                                    "id": "1.1.1",
                                    "title": "Level 3",
                                    "details": null,
                                    "status": "todo",
                                    "children": [
                                        {
                                            "id": "1.1.1.1",
                                            "title": "Level 4",
                                            "details": null,
                                            "status": "todo",
                                            "children": [
                                                {
                                                    "id": "1.1.1.1.1",
                                                    "title": "Level 5",
                                                    "details": null,
                                                    "status": "todo"
                                                }
                                            ]
                                        }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        let parsed_deep = parse_plan_refinement_output(raw_deep).unwrap();
        assert_eq!(
            parsed_deep.items[0].children[0].children[0].children[0].children[0].id,
            "1.1.1.1.1"
        );
        assert!(parsed_deep.items[0].children[0].children[0].children[0].children[0].is_leaf());
    }

    #[test]
    fn test_format_plan_discussion_prompt() {
        let plan = sample_plan();

        let prompt = format_plan_discussion_prompt(&plan, &["2".to_string()], "Should we use EdDSA?");
        assert!(prompt.contains("# Plan Discussion: [jwt-auth] JWT Authentication"));
        assert!(prompt.contains("Implement secure tokens"));
        assert!(prompt.contains("<active_plan id=\"jwt-auth\""));
        assert!(prompt.contains("Items Focused by User for Discussion:"));
        assert!(prompt.contains("- **#2 Endpoint**"));
        assert!(prompt.contains("Should we use EdDSA?"));
        assert!(prompt.contains("Instructions for Model:"));
    }

    #[test]
    fn test_plan_discussion_prompt_detection_and_attribute_escaping() {
        let plan = Plan {
            id: "auth-v2".to_string(),
            title: "OAuth & JWT".to_string(),
            description: Some("Tokens & Refresh".to_string()),
            ..Default::default()
        };
        let prompt = format_plan_discussion_prompt(&plan, &[], "Clarify signing");
        assert!(is_plan_discussion_prompt(&prompt));
        assert!(is_plan_discussion_prompt(&format!(" \n\t{}", prompt)));
        assert!(!is_plan_discussion_prompt("regular message"));
        assert!(prompt.contains("<active_plan id=\"auth-v2\" title=\"OAuth &amp; JWT\""));
    }

    #[test]
    fn test_build_plan_horizon_user_prompt_with_diagnostics() {
        let plan = sample_plan();
        let diagnostics = PlanHorizonDiagnostics {
            blocked_step_id: "2",
            blocked_reason: Some("Missing crypto dependency"),
            toolchain_status: Some("cargo check: 1 error"),
        };
        let prompt = build_plan_horizon_user_prompt(&plan, &[], None, Some(diagnostics), None);
        assert!(prompt.contains("# Plan Horizon Synthesis: [jwt-auth] JWT Authentication"));
        assert!(prompt.contains("## Architectural Concept"));
        assert!(prompt.contains("## Completed Foundation"));
        assert!(prompt.contains("- [x] #1 Models"));
        assert!(prompt.contains("## Blocked Step Diagnostics"));
        assert!(prompt.contains("- **Blocked Step**: #2 Endpoint"));
        assert!(prompt.contains("- **Coder Blocker Report**: Missing crypto dependency"));
        assert!(prompt.contains("- **Toolchain Status**: cargo check: 1 error"));
        assert!(prompt.contains("Architect Instructions"));

        // Without diagnostics the failure section is omitted entirely.
        let plain = build_plan_horizon_user_prompt(&plan, &[], None, None, None);
        assert!(!plain.contains("Blocked Step Diagnostics"));

        // A plan with no DONE steps reports an empty foundation.
        let mut no_done = sample_plan();
        for item in &mut no_done.items {
            item.status = PlanItemStatus::Todo;
        }
        let no_done_prompt = build_plan_horizon_user_prompt(&no_done, &[], None, None, None);
        assert!(no_done_prompt.contains("*(No completed steps yet)*"));
    }
}
