//! Built-in capability modules.
//!
//! Each module is the single source of truth for one protocol capability: it
//! owns the tags it recognizes, the prompt fragments it contributes, and the
//! decoder that turns model output into normalized [`LlmCommand`] values.

use std::sync::OnceLock;

use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, DocTopic, LlmCapability, LlmCommand, LlmOutcome,
    ModelResult,
};

use crate::edits::protocol::xml::engine::XmlTagSpec;
use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use crate::edits::protocol::{TerminalOutcome, VerifyOnSuccess, VerifyTarget};

use super::registry::{
    resolve_specs, CapabilityModule, ContractContext, ContractRule, PromptRenderContext, XmlParser,
};

/// Textual conversational response.
pub(crate) struct TextMessageModule;

impl CapabilityModule for TextMessageModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::TextMessage
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("message")
    }

    fn render_contract_rule(&self, _ctx: &ContractContext<'_>) -> Option<ContractRule> {
        Some(ContractRule::new(
            "Conversational Response",
            "Deliver every user-facing explanation, answer, and summary as conversational text in the user's language.",
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `message`: Conversational explanation or answer to the user.".to_string())
    }

    fn decode_xml(
        &self,
        _normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let cleaned = XmlParser::new(marker).clean_assistant_text(raw_text);
        if cleaned.trim().is_empty() {
            Vec::new()
        } else {
            vec![LlmCommand::Text { content: cleaned }]
        }
    }
}

static FILE_EDITS_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// File edit operations (replace, create, overwrite, move, delete).
pub(crate) struct FileEditsModule;

impl CapabilityModule for FileEditsModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::FileEdits
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(
            &FILE_EDITS_SPECS,
            &[
                "tauqe_edits",
                "edit",
                "search",
                "replace",
                "with",
                "create",
                "overwrite",
                "delete",
                "move",
            ],
        )
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("changes")
    }

    fn render_contract_rule(&self, _ctx: &ContractContext<'_>) -> Option<ContractRule> {
        Some(ContractRule::new(
            "Minimal Coherent Change",
            "Prefer the smallest coherent modification that satisfies the intent. Whenever you propose file edits, precede them with a concise 1-3 sentence explanation of what changed, why, and how the edits achieve the goal; never output edits alone.",
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let edits = ctx.tag_name("tauqe_edits");
        let edit = ctx.tag_name("edit");
        let search = ctx.tag_name("search");
        let replace = ctx.tag_name("replace");
        let create = ctx.tag_name("create");
        let overwrite = ctx.tag_name("overwrite");
        let move_tag = ctx.tag_name("move");
        let delete_tag = ctx.tag_name("delete");
        let instruction = format!(
            "- Code edits: wrap operations in <{edits} summary=\"...\">...</{edits}>. Use <{edit} path=\"...\"> with a <{search}>...</{search}> block and a <{replace}>...</{replace}> block for existing files, <{create} path=\"...\">...</{create}> for new files, <{overwrite} path=\"...\">...</{overwrite}> for a full rewrite, <{move_tag} from=\"...\" to=\"...\" /> to move or rename, and <{delete_tag} path=\"...\" /> to delete.\n  - The `summary` attribute of <{edits}> is the Git commit message: imperative mood, English, lowercase, no trailing period.\n  - An <{edit}> `path` MUST match a file present in <context>; a <{create}> path MUST be a valid repository-relative path.\n  - Files inside <context> declare their status through the `access` attribute: `access=\"editable\"` files may be modified directly, while a file marked `access=\"read_only\"` is provided for reference and becomes editable automatically once you propose a change to it.\n  - A <{search}> block MUST match EXACTLY ONE location in the target file, including whitespace and line breaks, and MUST stay as concise as possible while unique.\n  - Prefer <{edit}> for local changes and <{overwrite}> with the COMPLETE new content when most of a file changes; never use <{create}> for a path that already exists.\n  - Several operations may appear inside one <{edits}> block; if no code changes are needed, answer in plain text without any XML edit tags."
        );
        Some(instruction)
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `changes`: array of edit operations with `op` ('replace'|'create'|'delete'|'move'|'overwrite') plus `path`, `to`, `old_text`, `new_text`, and `content`.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_output(normalized_text, editable_paths) {
            ModelResult::Edit { edits, outcome, .. } => {
                let mut commands = Vec::new();
                if let Some(message) = outcome.error {
                    commands.push(LlmCommand::ProtocolError { message });
                }
                for operation in edits {
                    commands.push(LlmCommand::Edit { operation });
                }
                commands
            }
            ModelResult::Answer { .. } => Vec::new(),
        }
    }
}

static DISCOVERY_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Context and symbol discovery (context requests and repository searches).
pub(crate) struct DiscoveryModule;

impl CapabilityModule for DiscoveryModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(
            &DISCOVERY_SPECS,
            &[
                "context_request",
                "repo_search",
                "repo_dirs",
                "repo_map",
                "dir_list",
            ],
        )
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let request = ctx.tag_name("context_request");
        let search = ctx.tag_name("repo_search");
        let dirs = ctx.tag_name("repo_dirs");
        let list = ctx.tag_name("dir_list");
        let body = if ctx.has_eager_repo_map {
            format!(
                "Explore codebase structure before loading files. The repository symbol map is already rendered in this prompt, so never request it again. Locate subsystems with <{dirs} />, list files in a directory with <{list} path=\"...\" recursive=\"true|false\" />, locate symbols with <{search} path=\"...\" regex=\"...\" />, and load full file contents with <{request} path=\"...\" access=\"read_only|editable\" /> only when editing or deep inspection is necessary. A discovery tag performs a request only when it appears verbatim at the very end of the response, outside markdown code: emitting the tag itself is the only way to run it, describing the intent in prose performs nothing, and the tag is never deferred to a later response. Batch every request of a round together."
            )
        } else {
            let map = ctx.tag_name("repo_map");
            format!(
                "Explore codebase structure before loading files. Locate subsystems with <{dirs} />, list files in a directory with <{list} path=\"...\" recursive=\"true|false\" />, inspect symbol contours and type contracts with Tree-sitter <{map} path=\"path/to/file.rs\" /> (extracts signatures, structs, and traits without function bodies to prevent context bloat), locate symbols with <{search} path=\"...\" regex=\"...\" />, and load full file contents with <{request} path=\"...\" access=\"read_only|editable\" /> only when editing or deep inspection is necessary. A discovery tag performs a request only when it appears verbatim at the very end of the response, outside markdown code: emitting the tag itself is the only way to run it, describing the intent in prose performs nothing, and the tag is never deferred to a later response. Batch every request of a round together."
            )
        };
        Some(ContractRule::new("Discovery First", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let request = ctx.tag_name("context_request");
        let search = ctx.tag_name("repo_search");
        let dirs = ctx.tag_name("repo_dirs");
        let list = ctx.tag_name("dir_list");
        let discovery = if ctx.has_eager_repo_map {
            format!(
                "- Discovery: the repository symbol map is already rendered in this prompt, so never request it again; locate symbols with <{search} path=\"...\" regex=\"...\" /> (use path=\"\" or \".\" for the repository root) and request full files with <{request} path=\"...\" access=\"read_only|editable\" /> only when modifying or thoroughly inspecting them"
            )
        } else {
            let map = ctx.tag_name("repo_map");
            format!(
                "- Discovery: inspect symbol contours and API contracts with Tree-sitter <{map} path=\"path/to/file.rs\" /> (returns function signatures, types, and traits without function bodies; prefer this over loading full files to avoid context bloat), request full files with <{request} path=\"...\" access=\"read_only|editable\" /> only when modifying or thoroughly inspecting them, and locate symbols with <{search} path=\"...\" regex=\"...\" /> (use path=\"\" or \".\" for the repository root)"
            )
        };
        Some(format!(
            "{discovery}. Render the compact directory tree with <{dirs} /> when subsystem locations are unknown, and list tracked files of a directory with <{list} path=\"...\" recursive=\"true|false\" />. Emit discovery tags at the end of the response, outside markdown code, and batch multiple requests in one round. A tag performs a request only when it appears verbatim at the very end of the response, outside markdown code: emitting the tag itself is the only way to run it, describing the intent in prose performs nothing, and the tag is never deferred to a later response. Regex uses Rust / ripgrep syntax (\\b, \\w, \\d, (?:...), (?i); no lookarounds or backreferences). Files already present in <context> need no request. A directory listing covers Git-tracked files only (shallow by default, `recursive=\"true\"` widens to all files under the prefix, truncated at entry limit)."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `context_requests`: full files to load into context (`path`, or `editable:<path>` for files to modify); request only when modifying or deep inspection is necessary, never for files already in context.\n- `repo_maps`: targeted Tree-sitter symbol map requests for specific subsystems or files (`path`), returning signatures, types, and traits without function bodies; prefer this over full file requests to inspect APIs without context bloat.\n- `repo_searches`: repository search requests (`path`, `regex`) using Rust / ripgrep regex syntax (\\b, \\w, \\d, (?:...), (?i); no lookarounds or backreferences).\n- `repo_dirs`: set true to render the compact repository directory tree when the repository layout is still unknown, and false otherwise.\n- `dir_lists`: directory listing requests (`path`, `recursive`) to inspect tracked files under a prefix. Populate these arrays to run requests: describing the intent in prose performs nothing and triggers no discovery round.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let parser = XmlParser::new(marker);
        let mut commands = Vec::new();
        for request in parser.parse_context_requests(normalized_text) {
            commands.push(LlmCommand::ContextRequest {
                path: request.path,
                access: request.access,
            });
        }
        for search in parser.parse_repo_searches(normalized_text) {
            commands.push(LlmCommand::RepoSearch {
                path: search.path,
                regex: search.regex,
            });
        }
        if parser.parse_repo_dirs(normalized_text) {
            commands.push(LlmCommand::RepoDirs);
        }
        for path in parser.parse_repo_maps(normalized_text) {
            commands.push(LlmCommand::RepoMap { path });
        }
        for request in parser.parse_dir_lists(normalized_text) {
            commands.push(LlmCommand::DirList {
                path: request.path,
                recursive: request.recursive,
            });
        }
        commands
    }
}

static DOC_REQUEST_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// System documentation requests.
pub(crate) struct DocRequestModule;

impl CapabilityModule for DocRequestModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::DocRequest
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&DOC_REQUEST_SPECS, &["doc_request"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("doc_requests")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let doc = ctx.tag_name("doc_request");
        let topics = crate::docs::topic_hint();
        Some(ContractRule::new(
            "Self-Knowledge",
            format!(
                "Never guess answers about your identity, capabilities, concepts, workflows, keyboard shortcuts, or configuration. Environment behavior is declared in `tauqe.toml` at the repository root; questions about its parameters, defaults, budgets, or the recommended project layout are answered only from the authoritative documentation requested via <{doc} topic=\"{topics}\" />, where topic=\"config\" covers configuration and project preparation. Output nothing else except a short note, and answer strictly from the returned <system_documentation> blocks in the user's language."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let doc = ctx.tag_name("doc_request");
        let topics = crate::docs::topic_hint();
        Some(format!(
            "- Documentation: request authoritative system docs with <{doc} topic=\"{topics}\" />."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        let topics = DocTopic::ALL
            .iter()
            .map(|topic| format!("`{}`", topic.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "- `doc_requests`: array of system documentation topics ({topics}) to request before answering questions about identity, capabilities, concepts, or workflows; use an empty array when no documentation is needed."
        ))
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        XmlParser::new(marker)
            .parse_doc_requests(normalized_text)
            .into_iter()
            .map(|topic| LlmCommand::DocRequest { topic })
            .collect()
    }
}

static VERIFY_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Explicit deterministic toolchain verification requests.
pub(crate) struct ToolchainVerifyModule;

/// Maps the edit-protocol verification target onto the normalized command target.
pub(crate) fn map_verify_target(target: VerifyTarget) -> tauqe_protocol::VerifyTarget {
    match target {
        VerifyTarget::All => tauqe_protocol::VerifyTarget::All,
        VerifyTarget::Check => tauqe_protocol::VerifyTarget::Check,
        VerifyTarget::Test => tauqe_protocol::VerifyTarget::Test,
        VerifyTarget::Clippy => tauqe_protocol::VerifyTarget::Clippy,
    }
}

/// Maps the edit-protocol success policy onto the normalized command policy.
pub(crate) fn map_verify_on_success(
    on_success: VerifyOnSuccess,
) -> tauqe_protocol::VerifyOnSuccess {
    match on_success {
        VerifyOnSuccess::Silent => tauqe_protocol::VerifyOnSuccess::Silent,
        VerifyOnSuccess::Report => tauqe_protocol::VerifyOnSuccess::Report,
    }
}

impl CapabilityModule for ToolchainVerifyModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::ToolchainVerify
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&VERIFY_SPECS, &["verify"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("verify")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let verify = ctx.tag_name("verify");
        Some(ContractRule::new(
            "Code Verification",
            format!(
                "Toolchain verification is performed automatically and deterministically by the harness. Emit <{verify} target=\"all|check|test|clippy\" on_success=\"silent|report\" /> only when the user explicitly asks to run verification, compiler checks, or tests to inspect codebase status, and keep on_success=\"silent\" unless the user asked to see raw command output."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let verify = ctx.tag_name("verify");
        Some(format!(
            "- Verification: emit <{verify} target=\"all|check|test|clippy\" on_success=\"silent|report\" /> only on explicit request; use on_success=\"silent\" unless the user asked to see raw command output."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `verify`: deterministic toolchain verification request; `target` is 'none' when no verification is needed, otherwise 'all', 'check', 'test', or 'clippy', and `on_success` is 'silent' or 'report'.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_verify_request(normalized_text, raw_text) {
            Some(request) => vec![LlmCommand::Verify {
                target: map_verify_target(request.target),
                on_success: map_verify_on_success(request.on_success),
            }],
            None => Vec::new(),
        }
    }
}

static PLAN_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

fn plan_specs() -> &'static [&'static XmlTagSpec] {
    resolve_specs(&PLAN_SPECS, &["plan"])
}

/// Converts a parsed plan tag into a normalized plan command.
fn plan_tag_to_command(tag: ParsedPlanTag) -> LlmCommand {
    let action = match tag.action.as_str() {
        "save" => DiscussionPlanAction::Save,
        "delete" => DiscussionPlanAction::Delete,
        _ => DiscussionPlanAction::Update,
    };
    LlmCommand::Plan {
        update: DiscussionPlanUpdate {
            action,
            id: Some(tag.id),
            title: tag.title,
            description: tag.description,
            concept: tag.concept,
            items: tag
                .items
                .into_iter()
                .map(ParsedPlanItem::into_plan_item)
                .collect(),
        },
    }
}

/// Contract rule body used when plans are discussed through structured output.
const PLAN_DISCUSSION_BODY: &str = "When formulating or updating a task plan, populate the structured `plan_update` field in your response object with `action: \"save\"|\"update\"|\"delete\"`, plan `id`, `title`, `description`, `concept`, and `items`. Conceptual Foundation Invariant: Every plan in TAUQE is a two-section document and MUST have an architectural concept tree in `concept` defining requirements, design decisions, and system invariants ('What'). When creating a new plan (`action: \"save\"`), you MUST populate `concept` with at least one concept node. Creating a plan without a concept tree is strictly prohibited. Implementation steps in `items` ('How') are either outlined with `concept_refs` pointing to concept nodes, or left empty (`items: []`) to be synthesized subsequently by deep refinement (`plan_refine`). File Attributes Contract: `files` lists target files to create or modify (loaded as editable context), while `readonly_files` lists reference or contract files (loaded as read-only context and never modified). Leaf-Only Files Invariant: `files` and `readonly_files` must be specified exclusively for executable leaf steps without children; parent container items and task groups must have an empty `files: []` and `readonly_files: []` lists. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite/replace the entire plan; use `action: \"update\"` strictly to modify specific individual nodes or items in place by ID without touching unmentioned items; use `action: \"delete\"` to remove the plan. Reins Invariant: Never mutate the plan (leave `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`. Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants; define what state must be achieved and what invariants preserved. Always provide a clear `description` explaining the overall architecture, boundaries, and rationale. Language Consistency: Formulate plan title, description, concept titles/details, and step titles/details in the same language as the plan description and user's request. In <active_plan>, items marked with [FOCUSED] are focused by the user.";

/// Contract rule body used when plans are authored through XML edit tags.
fn plan_xml_body(ctx: &ContractContext<'_>) -> String {
    let plan = ctx.tag_name("plan");
    let description = ctx.tag_name("description");
    let concept = ctx.tag_name("concept");
    let concept_item = ctx.tag_name("concept_item");
    let details = ctx.tag_name("details");
    let item = ctx.tag_name("item");
    format!(
        "When formulating a task plan, wrap structured plan items in <{plan} action=\"save\" id=\"plan-id\" title=\"Plan Title\">...</{plan}>. In Develop mode, you may ONLY propose new plans (action=\"save\"). Updating plan statuses, closing steps, or modifying existing plans from Develop mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Conceptual Foundation Invariant: Every plan MUST contain an architectural concept tree in <{concept}>...</{concept}> defining the core requirements, design decisions, and system invariants ('What'). Inside <{concept}>, define architectural nodes using structured <{concept_item} id=\"c-...\" title=\"...\"><{details}>...</{details}></{concept_item}> tags. Plan creation without <{concept}> containing at least one <{concept_item}> is strictly prohibited. Implementation steps in <{item}> define 'How', may be included with concept_refs=\"c-...\", files=\"...\", and readonly=\"...\", or omitted if awaiting deep refinement. File Attributes Contract: the `files` attribute lists target files to create or modify (loaded as editable context), while the `readonly` attribute lists reference or contract files such as subsystem conventions, protocol types, and interfaces (loaded as read-only context and never modified). Leaf-Only Files Invariant: the `files` and `readonly` attributes must be specified exclusively for executable leaf steps without child items; parent container items and task groups must never have files. Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn or deep file inspection. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Always provide a clear <{description}> explaining the overall architecture, boundaries, and rationale. Plan Creation Invariant: Whenever you propose a new plan, you MUST precede the tag with a concise conversational explanation of the proposed plan, and you MUST emit the structured <{plan} action=\"save\" ...> tag in the same response containing both <{concept}> (mandatory) and optional <{item}> elements. Language Consistency: Formulate the plan title, description, concept nodes, step titles, and step details in the same language as the user's request.\nStructured Plan XML Format:\n<{plan} action=\"save\" id=\"plan-id\" title=\"Plan Title\">\n  <{description}>Overall architecture and rationale</{description}>\n  <{concept}>\n    <{concept_item} id=\"c-1\" title=\"Core Invariant\">\n      <{details}>Design rationale and invariants</{details}>\n    </{concept_item}>\n  </{concept}>\n  <{item} id=\"1\" title=\"Step Title\" concept_refs=\"c-1\" files=\"path/to/file.rs\" readonly=\"path/to/contract.rs\">\n    <{details}>Engineering objective and verification criteria</{details}>\n  </{item}>\n</{plan}>"
    )
}

/// Project plan creation.
pub(crate) struct PlanCreationModule;

impl CapabilityModule for PlanCreationModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::PlanCreation
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        plan_specs()
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("plan_update")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        Some(if ctx.intent.is_readonly_discussion() {
            ContractRule::new("Local Engineering Plans", PLAN_DISCUSSION_BODY)
        } else {
            ContractRule::new("Local Engineering Plans", plan_xml_body(ctx))
        })
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `plan_update`: structured plan creation or modification (`action`, `id`, `title`, `description`, `concept`, `items`), or null when the plan is not being changed.".to_string())
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let plan = ctx.tag_name("plan");
        let concept = ctx.tag_name("concept");
        let concept_item = ctx.tag_name("concept_item");
        let item = ctx.tag_name("item");
        let details = ctx.tag_name("details");
        let description = ctx.tag_name("description");
        Some(format!(
            "- Plans: propose a structured plan inside <{plan} action=\"save\" id=\"...\" title=\"...\">...</{plan}>. A plan is a two-section document: a mandatory <{concept}> section of <{concept_item} id=\"c-...\" title=\"...\"><{details}>...</{details}></{concept_item}> nodes defining 'What', and <{item} id=\"...\" title=\"...\" concept_refs=\"c-...\" files=\"...\" readonly=\"...\"><{details}>...</{details}></{item}> steps defining 'How'. <{description}> carries the overall architecture and rationale. The `files` attribute lists target files loaded as editable context and `readonly` lists reference or contract files loaded as read-only; both are allowed only on executable leaf steps without children. In Develop mode only new plans may be proposed; plan modifications belong to discussion mode."
        ))
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let (plans, _diagnostics) = XmlParser::new(marker).parse_plan_tags(normalized_text);
        plans
            .into_iter()
            .filter(|tag| tag.action == "save")
            .map(plan_tag_to_command)
            .collect()
    }
}

/// Project plan modification.
pub(crate) struct PlanUpdateModule;

impl CapabilityModule for PlanUpdateModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::PlanUpdate
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        plan_specs()
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("plan_update")
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let (plans, _diagnostics) = XmlParser::new(marker).parse_plan_tags(normalized_text);
        plans
            .into_iter()
            .filter(|tag| tag.action != "save")
            .map(plan_tag_to_command)
            .collect()
    }
}

static SCRATCHPAD_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Ephemeral turn-scoped working memory.
pub(crate) struct ScratchpadModule;

impl CapabilityModule for ScratchpadModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Scratchpad
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&SCRATCHPAD_SPECS, &["scratchpad"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("scratchpad")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let scratchpad = ctx.tag_name("scratchpad");
        Some(ContractRule::new(
            "Turn Scratchpad (Working Memory)",
            format!(
                "Use <{scratchpad}>...</{scratchpad}> to keep working hypotheses, verified findings, and the purpose of requested files across multi-round discovery and verification healing cycles. Keep notes concise and bullet-shaped; notes stay isolated to the current turn and never enter persisted history."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let scratchpad = ctx.tag_name("scratchpad");
        Some(format!(
            "- Working memory: keep transient notes inside <{scratchpad}>...</{scratchpad}> or <{scratchpad} note=\"...\" />. Notes are preserved within the current turn and re-injected in later rounds; they are discarded when the turn ends and never enter history."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `scratchpad`: turn-local working memory notes holding hypotheses, verified findings, and the purpose of context still needed; use an empty string when there is nothing to record. Notes live only inside the current turn, are re-injected in later rounds, and never enter history.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_scratchpad(normalized_text, raw_text) {
            Some(notes) => vec![LlmCommand::Scratchpad { notes }],
            None => Vec::new(),
        }
    }
}

static OUTCOME_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Explicit terminal success or error outcome.
pub(crate) struct OutcomeModule;

impl CapabilityModule for OutcomeModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Outcome
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&OUTCOME_SPECS, &["success", "error"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("outcome")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let success = ctx.tag_name("success");
        let error = ctx.tag_name("error");
        Some(ContractRule::new(
            "Terminal Outcome",
            format!(
                "Conclude every turn with an explicit <{success} summary=\"...\" /> on success or <{error} message=\"...\" /> on failure."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let success = ctx.tag_name("success");
        let error = ctx.tag_name("error");
        Some(format!(
            "- Terminal outcome: conclude with <{success} summary=\"...\" /> on success or <{error} message=\"...\" /> on failure."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `outcome`: terminal outcome object with `status` ('none' while the turn continues, 'success' when it succeeds, 'error' when it fails, 'skip' when the work is already satisfied and no change is required), `summary` for a success summary, `message` for an error message, and `reason` for a skip reason; unrelated fields stay empty.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_terminal_outcome(normalized_text) {
            Some(TerminalOutcome::Success { summary }) => vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Success { summary },
            }],
            Some(TerminalOutcome::Error { message }) => vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Error { message },
            }],
            Some(TerminalOutcome::Skip { reason }) => vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Skip { reason },
            }],
            _ => Vec::new(),
        }
    }
}

static BLOCKED_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Semantic blocker emitted when a precondition or assumption fails.
pub(crate) struct BlockedModule;

impl CapabilityModule for BlockedModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Blocked
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&BLOCKED_SPECS, &["blocked"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("blocked")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let blocked = ctx.tag_name("blocked");
        Some(ContractRule::new(
            "Semantic Blocker",
            format!(
                "Emit <{blocked} reason=\"...\" details=\"...\" /> when a precondition or assumption fails instead of guessing or silently failing."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let blocked = ctx.tag_name("blocked");
        Some(format!(
            "- Blocked: when a precondition or assumption fails, emit <{blocked} reason=\"...\" details=\"...\" /> to trigger semantic recovery."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `blocked`: semantic blocker object with `is_blocked` (true only when a precondition or assumption fails), `reason`, and `details`; report a blocker instead of guessing or silently failing.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_terminal_outcome(normalized_text) {
            Some(TerminalOutcome::Blocked { reason, details }) => {
                vec![LlmCommand::Blocked { reason, details }]
            }
            _ => Vec::new(),
        }
    }
}

static USER_LANGUAGE_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Detected primary natural language of the user prompt.
pub(crate) struct UserLanguageModule;

impl CapabilityModule for UserLanguageModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::UserLanguage
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&USER_LANGUAGE_SPECS, &["user_language"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("user_language")
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if let Some(language) = ctx.target_language {
            format!(
                "The user's language is {language}. All conversational explanations, answers, and messages MUST be in {language}. Do NOT switch to English for conversational responses unless requested."
            )
        } else if ctx.intent.is_readonly_discussion() {
            "Always formulate conversational explanations and user-facing answers in `message` in the same language as the user's request. Identify the user's language in the `user_language` field (e.g. \"Russian\"). Do NOT switch to English for conversational responses unless requested.".to_string()
        } else {
            let language = ctx.tag_name("user_language");
            format!(
                "Always formulate conversational explanations and user-facing answers in the same language as the user's request. Identify the user's language using <{language}>language_name</{language}> (e.g. <{language}>Russian</{language}>). Do NOT switch to English for conversational responses unless requested."
            )
        };
        Some(ContractRule::new("Language Consistency", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let language = ctx.tag_name("user_language");
        Some(format!(
            "- Language: report the primary language of the user's request with <{language}>language</{language}>."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `user_language`: primary language of the user's request.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        match XmlParser::new(marker).parse_user_language(normalized_text, raw_text) {
            Some(language) => vec![LlmCommand::UserLanguage { language }],
            None => Vec::new(),
        }
    }
}
