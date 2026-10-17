//! Built-in capability modules.
//!
//! Each module is the single source of truth for one protocol capability: it
//! owns the tags it recognizes, the prompt fragments it contributes, and the
//! decoder that turns model output into normalized [`LlmCommand`] values.

use std::sync::OnceLock;

use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, LlmCapability, LlmCommand, LlmOutcome, ModelResult,
};

use crate::edits::protocol::xml::engine::XmlTagSpec;
use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use crate::edits::protocol::{TerminalOutcome, VerifyOnSuccess, VerifyTarget};

use super::registry::{resolve_specs, CapabilityModule, PromptRenderContext, XmlParser};

/// Chooses the buffer a decoder parses.
///
/// A marker-bound protocol normalizes the raw text internally, so the raw
/// buffer is authoritative; otherwise the pre-normalized buffer is used.
fn working_text<'a>(marker: Option<&str>, normalized_text: &'a str, raw_text: &'a str) -> &'a str {
    if marker.is_some() {
        raw_text
    } else {
        normalized_text
    }
}

/// Textual conversational response.
pub(crate) struct TextMessageModule;

impl CapabilityModule for TextMessageModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::TextMessage
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("message")
    }

    fn render_contract_rule(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- Conversational Response: deliver every user-facing explanation, answer, and summary as conversational text in the user's language.".to_string())
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `message`: Conversational explanation or answer to the user.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        let cleaned = XmlParser::new(marker).clean_assistant_text(text);
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

    fn render_contract_rule(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- Minimal Coherent Change: prefer the smallest coherent modification that satisfies the intent, and explain each edit (what changed, why, and how it achieves the goal).".to_string())
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
        let mut instruction = format!(
            "- Code edits: wrap operations in <{edits} summary=\"...\">...</{edits}>. Use <{edit} path=\"...\"> with a <{search}>...</{search}> block and a <{replace}>...</{replace}> block for existing files, <{create} path=\"...\">...</{create}> for new files, <{overwrite} path=\"...\">...</{overwrite}> for a full rewrite, <{move_tag} from=\"...\" to=\"...\" /> to move or rename, and <{delete_tag} path=\"...\" /> to delete.\n  - The `summary` attribute of <{edits}> is the Git commit message: imperative mood, English, lowercase, no trailing period.\n  - An <{edit}> `path` MUST match a file present in <context>; a <{create}> path MUST be a valid repository-relative path.\n  - A <{search}> block MUST match EXACTLY ONE location in the target file, including whitespace and line breaks, and MUST stay as concise as possible while unique.\n  - Prefer <{edit}> for local changes and <{overwrite}> with the COMPLETE new content when most of a file changes; never use <{create}> for a path that already exists.\n  - Several operations may appear inside one <{edits}> block; if no code changes are needed, answer in plain text without any XML edit tags."
        );
        if !ctx.editable_paths.is_empty() {
            instruction.push_str(&format!(
                "\n  - Permitted editable files: {}.",
                ctx.editable_paths.join(", ")
            ));
        }
        Some(instruction)
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `changes`: array of edit operations with `op` ('replace'|'create'|'delete'|'move'|'overwrite') plus `path`, `to`, `old_text`, `new_text`, and `content`.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_output(text, editable_paths) {
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
        resolve_specs(&DISCOVERY_SPECS, &["context_request", "repo_search"])
    }

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let request = ctx.tag_name("context_request");
        let search = ctx.tag_name("repo_search");
        Some(format!(
            "- Discovery First: request missing files with <{request} path=\"...\" access=\"read_only|editable\" /> and locate symbols with <{search} path=\"...\" regex=\"...\" />; place these tags at the end of the response, outside markdown code."
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let request = ctx.tag_name("context_request");
        let search = ctx.tag_name("repo_search");
        Some(format!(
            "- Discovery: request missing files with <{request} path=\"...\" access=\"read_only|editable\" /> and locate symbols with <{search} path=\"...\" regex=\"...\" /> (use path=\"\" or \".\" for the repository root). Emit these tags at the end of the response, outside markdown code, and request several files in one round. Regex uses Rust / ripgrep syntax (\\b, \\w, \\d, (?:...), (?i)); lookaround assertions and backreferences are not supported. Files already present in <context> need no request."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `context_requests`: files to add to context before editing (`path`, or `editable:<path>` for files you need to modify). Request missing files listed in <repo_map> first and never request files already in context.\n- `repo_searches`: repository search requests (`path`, `regex`) using Rust / ripgrep regex syntax (\\b, \\w, \\d, (?:...), (?i); no lookarounds or backreferences).".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        let parser = XmlParser::new(marker);
        let mut commands = Vec::new();
        for request in parser.parse_context_requests(text) {
            commands.push(LlmCommand::ContextRequest {
                path: request.path,
                access: request.access,
            });
        }
        for search in parser.parse_repo_searches(text) {
            commands.push(LlmCommand::RepoSearch {
                path: search.path,
                regex: search.regex,
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let doc = ctx.tag_name("doc_request");
        Some(format!(
            "- Self-Knowledge: request authoritative documentation via <{doc} topic=\"overview|context|workflow|git|interface|review|plan|all\" /> before answering questions about identity, capabilities, or workflows."
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let doc = ctx.tag_name("doc_request");
        Some(format!(
            "- Documentation: request authoritative system docs with <{doc} topic=\"overview|context|workflow|git|interface|review|plan|all\" />."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `doc_requests`: array of system documentation topics (`overview`, `context`, `workflow`, `git`, `interface`, `review`, `plan`, `all`) to request before answering questions about identity, capabilities, concepts, or workflows; use an empty array when no documentation is needed.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        XmlParser::new(marker)
            .parse_doc_requests(text)
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
pub(crate) fn map_verify_on_success(on_success: VerifyOnSuccess) -> tauqe_protocol::VerifyOnSuccess {
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let verify = ctx.tag_name("verify");
        Some(format!(
            "- Verification Discipline: rely on automatic verification; emit <{verify} target=\"all|check|test|clippy\" on_success=\"silent|report\" /> only when the user explicitly asks to run verification or inspect codebase status."
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
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_verify_request(text) {
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

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `plan_update`: structured plan creation or modification (`action`, `id`, `title`, `description`, `concept`, `items`), or null when the plan is not being changed.".to_string())
    }

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let plan = ctx.tag_name("plan");
        Some(format!(
            "- Plan Invariants: every plan needs a concept section defining 'What'; steps define 'How'; propose plans via <{plan} action=\"save\" id=\"...\" title=\"...\">...</{plan}> and never mutate existing plans without an explicit user instruction."
        ))
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
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        let (plans, _diagnostics) = XmlParser::new(marker).parse_plan_tags(text);
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
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        let (plans, _diagnostics) = XmlParser::new(marker).parse_plan_tags(text);
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let scratchpad = ctx.tag_name("scratchpad");
        Some(format!(
            "- Working Memory: use <{scratchpad}>...</{scratchpad}> to hold hypotheses, verified findings, and context intent across discovery and healing rounds; scratchpad notes never enter history."
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
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_scratchpad(text) {
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let success = ctx.tag_name("success");
        let error = ctx.tag_name("error");
        Some(format!(
            "- Terminal Outcome: conclude every turn with an explicit <{success} summary=\"...\" /> on success or <{error} message=\"...\" /> on failure."
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
        Some("- `outcome`: terminal outcome object with `status` ('none' while the turn continues, 'success' when it succeeds, 'error' when it fails), `summary` for a success summary, and `message` for an error message; unrelated fields stay empty.".to_string())
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_terminal_outcome(text) {
            Some(TerminalOutcome::Success { summary }) => vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Success { summary },
            }],
            Some(TerminalOutcome::Error { message }) => vec![LlmCommand::Outcome {
                outcome: LlmOutcome::Error { message },
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let blocked = ctx.tag_name("blocked");
        Some(format!(
            "- Semantic Blocker: emit <{blocked} reason=\"...\" details=\"...\" /> when a precondition or assumption fails instead of guessing or silently failing."
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
        raw_text: &str,
        marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_terminal_outcome(text) {
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

    fn render_contract_rule(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let language = ctx.tag_name("user_language");
        Some(format!(
            "- Language Consistency: answer in the user's language and report it via <{language}>language</{language}>; keep internal reasoning in English."
        ))
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
        let text = working_text(marker, normalized_text, raw_text);
        match XmlParser::new(marker).parse_user_language(text) {
            Some(language) => vec![LlmCommand::UserLanguage { language }],
            None => Vec::new(),
        }
    }
}
