//! Direct contract rules and structured protocol instructions per [`LlmCapability`].
//!
//! Each [`LlmCapability`] directly maps to its contract rule, delta rule, and
//! structured instruction without a `CapabilityModule` trait or `CapabilityRegistry`
//! indirection.

use crate::config::project::{ProjectConfig, ProjectParameterSpec};
use crate::llm::CapabilityTree;
use crate::llm::LlmCapability;

use crate::edits::strategy::StrategyProfile;
use crate::prompt::ContourKind;

use super::regime;
use super::PromptRenderContext;

/// Sentence describing the `<file>` element shared by every layout policy.
const FILE_ELEMENT_CONTRACT: &str = "Every file is rendered as a `<file path=\"...\" access=\"...\">` element, and `access` is authoritative: `access=\"read_only\"` marks reference material, `access=\"editable\"` marks a file you may modify.";

const PLAN_STRUCTURED_CORE_BODY: &str = "Every plan travels as one structured field: `plan_create` authors a new plan and `plan_update` modifies an existing one, and both carry `action`, plan `id`, `title`, `description`, `concept`, and `items`. Conceptual Foundation Invariant: `concept` is the mandatory 'What' — every plan MUST carry at least one concept node with an `id` (`c-...`), a `title`, and its `details`, and a plan created without it is strictly prohibited. `items` are the 'How' steps; a step may carry `concept_refs` (`c-...`), `files`, `readonly_files`, and `outer_docs`, or omit them while awaiting refinement. File Attributes Contract: `files` lists target files to create or modify and is loaded as editable context; `readonly_files` lists reference and contract files such as subsystem conventions, protocol types, and interfaces and is loaded as read-only context, never modified. External Specification Contract: `outer_docs` lists the external specifications the step is implemented against, each entry naming a document the project declares in `[[outer_docs]]`: `id` addresses a declared root document and `id/relative.md` addresses a document below that root, never climbing above it. Leaf-Only Files Invariant: `files`, `readonly_files`, and `outer_docs` belong exclusively to executable leaf steps without children, and a parent container item never carries them. Always provide a clear `description` explaining the overall architecture, boundaries, and rationale. Plan Creation Invariant: only create a plan when explicitly instructed or confirmed by the user; whenever you propose a new plan, precede the payload with a concise conversational explanation and populate `plan_create` in the same response, carrying `concept` (mandatory) and optional `items`. Language Consistency: formulate the plan title, description, concept nodes, step titles, and step details in the same language as the user's request.";

const PLAN_STRUCTURED_REPL_DELTA: &str = "In REPL mode, you may ONLY propose new plans (`action: \"save\"`). Updating plan statuses, closing steps, or modifying existing plans from REPL mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Reins Invariant: never populate `plan_update` without an explicit user instruction or confirmation to formulate a plan; questions, analysis, and exploratory discussion are answered strictly in conversational text in `message`.";

const PLAN_STRUCTURED_DISCUSSION_DELTA: &str = "In plan discussion mode, populate the structured `plan_update` field with `action: \"save\"|\"update\"`. Plan Action Semantics: use `action: \"save\"` to create or completely overwrite the whole plan, and `action: \"update\"` strictly to modify specific nodes or items in place by ID without touching unmentioned ones. Code modifications requested directly by the developer are permitted alongside the discussion, and they travel the ordinary transactional path. Reins Invariant: never mutate the plan (leave `plan_update: null`) without an explicit user instruction to modify it; questions, analysis, trade-off reviews, and critique are answered strictly in conversational text in `message`.";
/// Guarantee the harness states about every proposed change set.
///
/// The rule an isolated step receives renders this sentence, so every
/// contour that verifies a change set promises the same deterministic
/// verification instead of drifting apart.
pub(crate) const CODE_VERIFICATION_GUARANTEE: &str = "After every change set you propose, the harness verifies it automatically and deterministically with the checks the project declares, such as compilation, linters, and tests. A change set that fails a check is rejected and never reaches the repository: you receive the clean diagnostics and fix only the lines they name.";
pub struct ContractRule {
    pub(crate) title: String,
    pub(crate) body: String,
}

impl ContractRule {
    pub(crate) fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

pub fn render_contract_rules(rules: &[ContractRule]) -> String {
    let mut rendered = String::new();
    for (index, rule) in rules.iter().enumerate() {
        rendered.push_str(&format!(
            "{}. {}: {}\n",
            index + 1,
            rule.title,
            rule.body.trim_end()
        ));
    }
    rendered
}

pub struct ContractContext<'a> {
    pub(super) render: PromptRenderContext<'a>,
    has_eager_repo_map: bool,
    /// Typed kind of the contour that drives the turn.
    ///
    /// The contour is declared by the loop that owns the turn, so the rules of a
    /// regime follow that declaration instead of a composition of leaves.
    pub(crate) contour: ContourKind,
}

impl<'a> ContractContext<'a> {
    pub(crate) fn new(render: PromptRenderContext<'a>) -> Self {
        let has_eager_repo_map = render.has_eager_repo_map;
        Self {
            render,
            has_eager_repo_map,
            contour: ContourKind::Dialogue,
        }
    }

    /// Declares the contour that drives the turn.
    pub(crate) fn with_contour(mut self, contour: ContourKind) -> Self {
        self.contour = contour;
        self
    }
}

/// Builds the rule describing how the context material reaches the model.
fn context_surface_rule() -> ContractRule {
    let container = "The project state arrives as a transcript: `<file>` elements are appended as their own messages in the order they entered the context, each one after the interaction record that observed it, while a `<context_manifest>` tail lists the path and access of every file currently in context without repeating its content.";
    ContractRule::new(
        "Context Surface",
        format!("{container} {FILE_ELEMENT_CONTRACT}"),
    )
}

/// Sentence publishing the protected paths the project declares.
fn protected_paths_sentence(project: Option<&ProjectConfig>) -> Option<String> {
    let project = project?;
    let mut patterns: Vec<String> = Vec::new();
    for pattern in &project.boundaries.protected {
        let pattern = pattern.trim();
        if !pattern.is_empty() {
            patterns.push(format!("`{pattern}`"));
        }
    }
    if patterns.is_empty() {
        return None;
    }
    let listed = patterns.join(", ");
    Some(format!(
        "The project declares {listed} as protected trust boundaries: editing them is categorically forbidden, and such a file is requested only as `read_only` reference material."
    ))
}

fn ends_sentence(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | ':' | '!' | '?'))
}

fn render_parameter_contract(parameters: &[ProjectParameterSpec]) -> String {
    let entries: Vec<String> = parameters
        .iter()
        .map(|parameter| {
            let mut entry = match parameter.default.as_deref() {
                Some(default) => format!("{}=\"{}\"", parameter.name, default),
                None => parameter.name.clone(),
            };
            if let Some(doc) = parameter
                .doc
                .as_deref()
                .map(str::trim)
                .filter(|doc| !doc.is_empty())
            {
                entry.push_str(&format!(" ({doc})"));
            }
            entry
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

fn render_custom_tool_list(project: &ProjectConfig) -> String {
    let mut rendered = String::new();
    for (name, spec) in &project.commands {
        let Ok(resolved) = project.resolve(&format!("commands.{name}"), spec) else {
            continue;
        };
        let mut line = resolved.command;
        for argument in &resolved.args {
            line.push(' ');
            line.push_str(argument);
        }
        for parameter in &resolved.parameters {
            line.push(' ');
            line.push('[');
            line.push_str(&parameter.name);
            line.push(']');
        }

        rendered.push_str(&format!("\n  - `{name}`: "));
        if let Some(description) = resolved
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            rendered.push_str(description);
            if !ends_sentence(description) {
                rendered.push('.');
            }
            rendered.push(' ');
        }
        rendered.push_str(&format!("Runs `{line}`"));
        if let Some(secs) = resolved.timeout_secs {
            rendered.push_str(&format!(" within a {secs}s timeout"));
        }
        if !resolved.parameters.is_empty() {
            rendered.push_str(&format!(
                ". Parameters: {}",
                render_parameter_contract(&resolved.parameters)
            ));
        }
    }
    rendered
}

/// Closing sentence of the research rule: it states how one round of discovery is
/// dispatched, and both variants of the rule end with it.
const RESEARCH_ROUND_DISCIPLINE: &str = "A turn proceeds across autonomous rounds: choosing `research` executes your requests and immediately starts the next round of the same turn with the loaded context; choosing `answer`, `edit`, or `plan` concludes the turn. Operational Action Contract: `message` carries your conversational voice, while `action` dictates the turn state. The `answer` branch carries no payload because it is the completion marker: choose `answer` when your response in `message` is final and completely answers the developer without further context. Zero Promissory Prose Invariant: Never promise in `message` to inspect, search, or read files in the future. If you need context, you MUST populate `context_requests` or `repo_searches` with concrete items in this exact response. Stating that you are loading or searching for files while leaving research arrays empty or selecting `answer` is strictly prohibited. Send every request of a round in one response instead of spreading discovery across multiple turns, and when asked to inspect or modify code that is not in context, dispatch those requests rather than asking the developer for paths or answering with passive advice.";

pub fn contract_rule(capability: LlmCapability, ctx: &ContractContext<'_>) -> Option<ContractRule> {
    match capability {
        LlmCapability::TextMessage => Some(ContractRule::new(
            "Conversational Response",
            "Answer in the language of the developer's request. Speak naturally like an experienced engineer explaining architecture to a peer: prefer plain, connected explanations over robotic bureaucracy or compressed shorthand. Organize answers around what the developer needs to understand, not the chronological order of your investigation; synthesize findings into a coherent explanation rather than presenting working notes or exploration logs. State the core conclusion before diving into code details. Assume general programming expertise, but do not assume familiarity with repository-specific implementation: briefly explain the architectural role and relevance of referenced code, as identifiers and paths alone are insufficient. Include specific symbols only when they clarify cause and effect or help locate code. Be concise by eliminating praise, filler, and repetitive statements, never by cutting out necessary explanations. Format the answer as clean, readable Markdown that aids comprehension without visual clutter: short paragraphs, bullet lists for enumerations, fenced code blocks for code or commands, and headings only when structure demands them. An analytical or exploratory request is answered in prose and never deferred: once the files, searches, or documentation you requested are available, the same turn carries the written analysis the developer asked for. An empty answer is never a valid conclusion, and no terminal signal stands in for an answer you have not written.",
        )),
        LlmCapability::EditReplace => {
            let mut body = String::from(
                "Prefer the smallest coherent modification that satisfies the intent: change only what the task requires, and never reformat, rename, or refactor code the task does not touch.",
            );
            if let Some(protected) = protected_paths_sentence(ctx.render.project) {
                body.push(' ');
                body.push_str(&protected);
            }
            Some(ContractRule::new("Minimal Coherent Change", body))
        }
        LlmCapability::ContextRequest => {
            let body = if ctx.has_eager_repo_map {
                format!(
                    "Locate symbols and text with `repo_searches` using Rust / ripgrep regex, list the tracked files of a directory with `dir_lists`, and set `repo_dirs` only when the repository layout is still unknown; the repository symbol map is already rendered in this prompt, so never request it again. Load a file whole with `context_requests` when you intend to edit it or inspect it deeply. {RESEARCH_ROUND_DISCIPLINE}"
                )
            } else {
                format!(
                    "Locate symbols and text with `repo_searches` using Rust / ripgrep regex, list the tracked files of a directory with `dir_lists`, set `repo_dirs` only when the repository layout is still unknown, and inspect the symbol contour of a subsystem with `repo_maps`. Load a file whole with `context_requests` when you intend to edit it or inspect it deeply. {RESEARCH_ROUND_DISCIPLINE}"
                )
            };
            Some(ContractRule::new("Research First", body))
        }
        LlmCapability::DocRequest => {
            let topics = crate::docs::topic_hint();
            let body = format!(
                "Never invent exact details. Answer conceptual questions about your identity, capabilities, and working processes, and help the developer set up a project, choose a workflow, or organize the work, from your baseline understanding of this harness, without a documentation request. Details that must be exact — parameters, defaults, budgets, keyboard shortcuts, configuration keys, and API surfaces — are not knowable from that understanding: environment behavior is declared in `tauqe.toml` at the repository root, so they are answered only from the authoritative documentation requested through the `tauqe_doc_requests` field (topics: {topics}), where the `config` topic covers configuration and project preparation. When such a detail is needed, populate that field, keep the rest of the response to a short note, and answer strictly from the returned <system_documentation> blocks in the user's language."
            );
            Some(ContractRule::new("Self-Knowledge", body))
        }
        LlmCapability::PlanCreation => Some(ContractRule::new(
            "Local Engineering Plans",
            PLAN_STRUCTURED_CORE_BODY.to_string(),
        )),
        LlmCapability::Blocked => Some(ContractRule::new(
            "Semantic Blocker",
            "Set the `blocked` field with `is_blocked` true, a `reason`, and optional `details` when a precondition or assumption fails, instead of guessing or silently failing.",
        )),
        LlmCapability::Abort => Some(ContractRule::new(
            "Unconditional Abort",
            "An abort is not a blocker. Set the `abort` field with `is_aborted` true when the remaining work needs an action only a human can perform on the host system: approving a native build, installing a package or a system utility, starting a daemon or a database, or authenticating against an external registry. The harness stops the turn at once and rolls back the incomplete change set, so `reason` names the concrete missing capability while `recommendation` carries the exact terminal command or instruction the developer must run. Replacing the required work with a stub, a surrogate, or a placeholder to keep the turn going is strictly prohibited.",
        )),
        LlmCapability::CustomTools => {
            let project = ctx.render.project?;
            if project.commands.is_empty() {
                return None;
            }
            let tools = render_custom_tool_list(project);
            Some(ContractRule::new(
                "Project Custom Tools",
                format!(
                    "The project declares a closed allowlist of commands in `[project.commands]`: you invoke a command strictly by the name it declares, an argument is one literal value that carries no shell syntax, a parameter is supplied by its declared name and keeps its declared default when set to null, and anything the declaration does not name is rejected instead of reaching a process. When no declared command covers the required operation, report a semantic blocker instead of improvising one. The commands this project makes available:{tools}"
                ),
            ))
        }
        LlmCapability::Scratchpad
        | LlmCapability::CommitMessage
        | LlmCapability::EditCreate
        | LlmCapability::EditDelete
        | LlmCapability::EditMove
        | LlmCapability::EditMoveDir
        | LlmCapability::EditOverwrite
        | LlmCapability::RepoSearch
        | LlmCapability::RepoDirs
        | LlmCapability::RepoMap
        | LlmCapability::DirList
        | LlmCapability::OuterDocRequest
        | LlmCapability::PlanUpdate
        | LlmCapability::ReviewUpdate
        | LlmCapability::TurnError
        | LlmCapability::TurnSkip => None,
    }
}

pub fn delta_rule(capability: LlmCapability, expr: &CapabilityTree) -> Option<ContractRule> {
    match capability {
        LlmCapability::PlanCreation => {
            let body = if expr.contains(LlmCapability::PlanUpdate) {
                PLAN_STRUCTURED_DISCUSSION_DELTA.to_string()
            } else {
                PLAN_STRUCTURED_REPL_DELTA.to_string()
            };
            Some(ContractRule::new("Local Engineering Plan Actions", body))
        }
        _ => None,
    }
}

/// Renders the operational rules a capability adds on top of the response schema.
///
/// The schema is the single source of the response structure: it declares every
/// field with its type, its required-ness, its enum, and its description, so an
/// instruction never restates one. What survives here is what a field cannot
/// express — how a change set addresses the lines it replaces.
pub fn structured_instruction(
    capability: LlmCapability,
    ctx: &PromptRenderContext<'_>,
) -> Option<String> {
    match capability {
        LlmCapability::EditReplace => Some(
            StrategyProfile::of(ctx.patch_strategy)
                .structured_instruction
                .to_string(),
        ),
        _ => None,
    }
}

fn enabled_capabilities<'a>(expr: &'a CapabilityTree) -> impl Iterator<Item = LlmCapability> + 'a {
    LlmCapability::ALL
        .iter()
        .copied()
        .filter(move |cap| expr.contains(*cap))
}

pub fn compose_contract_rules(expr: &CapabilityTree, ctx: &ContractContext<'_>) -> String {
    let mut rules = regime::shared_trunk_rules(ctx, expr);
    rules.insert(rules.len().min(1), context_surface_rule());
    for capability in enabled_capabilities(expr) {
        if let Some(rule) = contract_rule(capability, ctx) {
            rules.push(rule);
        }
    }
    // The rules a declared regime alone states follow the ones every regime
    // shares, so one dense sequence carries the trunk beside the specifics of
    // the turn it composes.
    for rule in regime::intent_delta_rules(ctx, expr) {
        rules.push(rule);
    }
    for capability in enabled_capabilities(expr) {
        if let Some(rule) = delta_rule(capability, expr) {
            rules.push(rule);
        }
    }
    render_contract_rules(&rules)
}

pub fn compose_structured_protocol_instructions(
    expr: &CapabilityTree,
    ctx: &PromptRenderContext<'_>,
) -> String {
    compose(expr, ctx, |capability, render_ctx| {
        structured_instruction(capability, render_ctx)
    })
}

fn compose(
    expr: &CapabilityTree,
    ctx: &PromptRenderContext<'_>,
    render: impl Fn(LlmCapability, &PromptRenderContext<'_>) -> Option<String>,
) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for capability in enabled_capabilities(expr) {
        if let Some(block) = render(capability, ctx) {
            blocks.push(block);
        }
    }
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_context() -> ContractContext<'static> {
        ContractContext::new(PromptRenderContext::new(&[], None))
    }

    #[test]
    fn the_research_rule_states_the_discovery_algorithm() {
        let eager =
            ContractContext::new(PromptRenderContext::new(&[], None).with_eager_repo_map(true));
        let rule = contract_rule(LlmCapability::ContextRequest, &eager).expect("rule rendered");
        assert_eq!(rule.title, "Research First");
        for tool in [
            "repo_searches",
            "dir_lists",
            "repo_dirs",
            "context_requests",
        ] {
            assert!(
                rule.body.contains(tool),
                "the eager rule never names {tool}"
            );
        }
        assert!(rule.body.contains("already rendered"));
        assert!(rule.body.contains("never request it again"));
        assert!(!rule.body.contains("repo_maps"));

        let on_demand = plain_context();
        let rule = contract_rule(LlmCapability::ContextRequest, &on_demand).expect("rule rendered");
        assert!(rule.body.contains("repo_maps"));
        assert!(rule.body.contains("Rust / ripgrep regex"));
        assert!(rule.body.contains("one response"));
    }

    #[test]
    fn the_research_rule_states_the_action_contract_and_the_promissory_invariant() {
        let ctx = plain_context();
        let rule = contract_rule(LlmCapability::ContextRequest, &ctx).expect("rule rendered");

        // A model that reads only the discovery rule still learns where its answer
        // text lives and that the bare `answer` branch is the marker closing the
        // turn, so the paragraph states the contract before the prohibition.
        assert!(rule.body.contains("Operational Action Contract"));
        assert!(rule
            .body
            .contains("`message` carries your conversational voice"));
        assert!(rule.body.contains("`action` dictates the turn state"));
        assert!(rule.body.contains("carries no payload"));
        assert!(rule.body.contains("completion marker"));
        assert!(rule
            .body
            .contains("choosing `answer`, `edit`, or `plan` concludes the turn"));

        // Promissory prose is the failure the paragraph exists to forbid: a promise
        // to read a file is a research request, so it must arrive with the arrays
        // that carry it in the same response.
        assert!(rule.body.contains("Zero Promissory Prose Invariant"));
        assert!(rule.body.contains("Never promise in `message`"));
        assert!(rule
            .body
            .contains("MUST populate `context_requests` or `repo_searches`"));
        assert!(rule.body.contains("strictly prohibited"));

        let contract = rule
            .body
            .find("Operational Action Contract")
            .expect("contract");
        let invariant = rule
            .body
            .find("Zero Promissory Prose Invariant")
            .expect("invariant");
        assert!(
            contract < invariant,
            "the contract precedes the invariant it governs"
        );
    }

    fn project(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the project section parses")
    }

    #[test]
    fn structured_schema_is_composed_from_the_capability_tree() {
        let render = PromptRenderContext::new(&[], None);
        let expr = CapabilityTree::root([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::any_of(
                "action",
                [
                    CapabilityTree::named_set(
                        "research",
                        [
                            CapabilityTree::leaf(LlmCapability::ContextRequest),
                            CapabilityTree::leaf(LlmCapability::CommitMessage),
                        ],
                    ),
                    CapabilityTree::named_set(
                        "edit",
                        [
                            CapabilityTree::leaf(LlmCapability::EditReplace),
                            CapabilityTree::leaf(LlmCapability::CommitMessage),
                        ],
                    ),
                ],
            ),
        ]);

        let schema = crate::capabilities::capability_expr_schema(&expr, &render);
        let variants = schema["properties"]["action"]["anyOf"]
            .as_array()
            .expect("an exclusion admits a discriminated union");
        assert_eq!(variants.len(), 2);
        for (variant, kind) in variants.iter().zip(["research", "edit"]) {
            assert_eq!(variant["properties"]["kind"]["enum"][0], kind);
        }
    }

    #[test]
    fn the_prompt_renders_the_description_and_the_parameter_contract() {
        let config = project(
            r#"{
                "commands": {
                    "check": {
                        "command": "just",
                        "args": ["check"],
                        "description": "Type-check the workspace.",
                        "timeout_secs": 600
                    },
                    "log-tail": {
                        "command": "just",
                        "args": ["log-tail"],
                        "description": "Print the last N lines of the server journal",
                        "parameters": [
                            { "name": "n", "default": "50", "doc": "Line count" },
                            { "name": "follow" }
                        ]
                    }
                }
            }"#,
        );

        let rendered = render_custom_tool_list(&config);
        assert_eq!(
            rendered,
            "\n  - `check`: Type-check the workspace. Runs `just check` within a 600s timeout\
             \n  - `log-tail`: Print the last N lines of the server journal. Runs `just log-tail [n] [follow]`. Parameters: [n=\"50\" (Line count), follow]"
        );
    }

    #[test]
    fn the_custom_tools_rule_states_the_contract_then_the_available_commands() {
        let config = project(
            r#"{
                "commands": {
                    "check": {
                        "command": "just",
                        "args": ["check"],
                        "description": "Type-check the workspace."
                    }
                }
            }"#,
        );
        let render = PromptRenderContext::new(&[], None).with_project(Some(&config));
        let ctx = ContractContext::new(render);
        let rule = contract_rule(LlmCapability::CustomTools, &ctx).expect("rule rendered");
        assert_eq!(rule.title, "Project Custom Tools");
        assert!(rule.body.contains("closed allowlist"));
        assert!(rule.body.contains("one literal value"));
        assert!(rule.body.contains("rejected"));
        assert!(rule.body.contains("semantic blocker"));
        assert!(rule.body.contains("`check`"));
        assert!(
            rule.body.find("closed allowlist") < rule.body.find("`check`"),
            "the contract is stated before the commands it governs"
        );

        let empty = ProjectConfig::default();
        let render = PromptRenderContext::new(&[], None).with_project(Some(&empty));
        assert!(contract_rule(LlmCapability::CustomTools, &ContractContext::new(render)).is_none());
    }

    #[test]
    fn the_custom_tools_rule_follows_the_capability_surface() {
        let config = project(
            r#"{
                "commands": {
                    "check": {
                        "command": "just",
                        "args": ["check"],
                        "description": "Type-check the workspace."
                    }
                }
            }"#,
        );
        let render = PromptRenderContext::new(&[], None).with_project(Some(&config));
        let ctx = ContractContext::new(render);

        // A surface that cannot invoke a project command advertises neither the
        // principle of the allowlist nor a command it declares.
        let without =
            CapabilityTree::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]);
        let composed = compose_contract_rules(&without, &ctx);
        assert!(!composed.contains("Project Custom Tools"));
        assert!(!composed.contains("`check`"));

        // The declared capability renders the static principle beside the dynamic
        // command list of the project.
        let with =
            CapabilityTree::from_leaves([LlmCapability::TextMessage, LlmCapability::CustomTools]);
        let composed = compose_contract_rules(&with, &ctx);
        assert!(composed.contains("Project Custom Tools"));
        assert!(composed.contains("closed allowlist"));
        assert!(composed.contains("`check`"));
    }
    #[test]
    fn the_conversational_response_rule_requires_a_written_answer() {
        let ctx = plain_context();
        let rule = contract_rule(LlmCapability::TextMessage, &ctx).expect("rule rendered");
        assert_eq!(rule.title, "Conversational Response");
        assert!(rule.body.contains("language of the developer's request"));
        assert!(rule.body.contains("readable Markdown"));
        assert!(rule.body.contains("programming expertise"));
        assert!(rule.body.contains("eliminating praise, filler"));
        assert!(rule.body.contains("same turn carries the written analysis"));
        assert!(rule
            .body
            .contains("empty answer is never a valid conclusion"));
        // The rule states the answer itself, never the wire fields that carry it.
        assert!(!rule.body.contains("`message`"));
        assert!(!rule.body.contains("commit_message"));
    }

    #[test]
    fn the_removed_capabilities_contribute_no_contract_rule() {
        let ctx = plain_context();
        for capability in [LlmCapability::Scratchpad, LlmCapability::CommitMessage] {
            assert!(
                contract_rule(capability, &ctx).is_none(),
                "{capability} states its semantics through the schema, not through a rule"
            );
        }
    }

    #[test]
    fn the_plan_action_semantics_travel_in_the_delta_rule() {
        let repl_expr = crate::control::repl::repl_capability_expr(None);
        let repl_delta =
            delta_rule(LlmCapability::PlanCreation, &repl_expr).expect("repl delta rendered");
        assert!(repl_delta.body.contains("ONLY propose new plans"));

        let disc_expr = crate::control::repl::repl_capability_expr(Some(
            &crate::discussion::DiscussionTarget::Plan { id: "p1".into() },
        ));
        let disc_delta =
            delta_rule(LlmCapability::PlanCreation, &disc_expr).expect("discussion delta rendered");
        assert!(disc_delta.body.contains("action: \"update\""));
    }

    #[test]
    fn the_edit_rule_lists_the_protected_paths_of_the_project() {
        let mut project = ProjectConfig::default();
        project.boundaries.protected = vec!["Justfile".to_string()];
        let render = PromptRenderContext::new(&[], None).with_project(Some(&project));
        let ctx = ContractContext::new(render);
        let rule = contract_rule(LlmCapability::EditReplace, &ctx).expect("rule rendered");
        assert!(rule.body.contains("`Justfile`"));
    }

    #[test]
    fn composed_text_names_no_entity_the_schema_does_not_declare() {
        let render = PromptRenderContext::new(&[], None);
        let surfaces = [
            (
                "interactive_repl",
                crate::control::repl::repl_capability_expr(None),
                ContourKind::Dialogue,
            ),
            (
                "plan_discussion",
                crate::control::repl::repl_capability_expr(Some(
                    &crate::discussion::DiscussionTarget::Plan {
                        id: "p-1".to_string(),
                    },
                )),
                ContourKind::Dialogue,
            ),
            (
                "review_discussion",
                crate::control::repl::repl_capability_expr(Some(
                    &crate::discussion::DiscussionTarget::Review {
                        id: "r-1".to_string(),
                    },
                )),
                ContourKind::Dialogue,
            ),
            (
                "isolated_step",
                crate::control::turn::turn_capability_expr(),
                ContourKind::Step,
            ),
            (
                "architect_synthesis",
                crate::control::refine::architect_synthesis_capability_expr(),
                ContourKind::Architect,
            ),
        ];
        for (label, surface, contour) in surfaces {
            let ctx =
                ContractContext::new(PromptRenderContext::new(&[], None)).with_contour(contour);
            let composed = [
                ("rules", compose_contract_rules(&surface, &ctx)),
                (
                    "instructions",
                    compose_structured_protocol_instructions(&surface, &render),
                ),
            ];
            for (name, text) in composed {
                for retired in ["outcome", "research_requests", "'none'", "user_language"] {
                    assert!(
                        !text.contains(retired),
                        "the {label} {name} keep the retired entity {retired}"
                    );
                }
            }
        }
    }
}
