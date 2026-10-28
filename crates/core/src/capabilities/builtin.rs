//! Built-in capability modules.
//!
//! Each module is the single source of truth for one protocol capability: it
//! owns the tags it recognizes, the prompt fragments it contributes, and the
//! decoder that turns model output into normalized [`LlmCommand`] values.

use std::sync::OnceLock;

use tauqe_protocol::{
    DiscussionPlanAction, DiscussionPlanUpdate, DocTopic, EditProtocolKind, LlmCapability,
    LlmCommand, LlmOutcome, ModelResult, PatchStrategy, ProjectConfig, ProjectParameterSpec,
    PromptLayoutKind,
};

use crate::edits::protocol::xml::engine::{self, XmlTagSpec};
use crate::edits::protocol::xml::tags::{ParsedPlanItem, ParsedPlanTag};
use crate::edits::protocol::{TerminalOutcome, VerifyOnSuccess, VerifyTarget, XmlEditProtocol};
use crate::edits::strategy::StrategyProfile;
use crate::mockups::MOCKUPS_DIR;

use super::registry::{
    resolve_specs, CapabilityModule, ContractContext, ContractRule, PromptRenderContext, XmlParser,
};
use super::schema;

/// Sentence describing the `<file>` element shared by every layout policy.
///
/// The element and its `access` attribute are prompt-side payload rather than a
/// command channel the model emits, so both layouts describe them identically
/// and only the container that delivers them differs.
const FILE_ELEMENT_CONTRACT: &str = "Every file is rendered as a `<file path=\"...\" access=\"...\">` element, and `access` is authoritative: `access=\"read_only\"` marks reference material, `access=\"editable\"` marks a file you may modify.";

/// Builds the rule describing how the context material reaches the model.
///
/// The rule renders in the vocabulary of the declared layout policy — the single
/// `<context>` block of the semantic layout, or the transcript and its
/// `<context_manifest>` tail of the chronological one — and never mixes the two.
pub(crate) fn context_surface_rule(ctx: &ContractContext<'_>) -> ContractRule {
    let container = match ctx.prompt_layout {
        PromptLayoutKind::Semantic => "The project state arrives as one `<context>` block inside the system prompt: the `<file>` elements follow one another in insertion order, and a file that entered the context in a later round is appended at the end of the same block.",
        PromptLayoutKind::Chronological => "The project state arrives as a transcript: `<file>` elements are appended as their own messages in the order they entered the context, each one after the interaction record that observed it, while a `<context_manifest>` tail lists the path and access of every file currently in context without repeating its content.",
    };
    ContractRule::new(
        "Context Surface",
        format!("{container} {FILE_ELEMENT_CONTRACT}"),
    )
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

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::message_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        // The duty is identical in both contours: an analytical request is
        // answered in prose within the turn that finally holds the material.
        // Only the name of the vehicle differs, because the composed structured
        // schema carries the answer in `message` in every dialogue contour. The
        // rule therefore follows the transport alone: reading the declared
        // intent would make the byte-stable head impossible.
        let body = if ctx.protocol != EditProtocolKind::Xml {
            "Deliver every user-facing explanation, answer, and summary as conversational text in the `message` field and in the user's language. Format it as readable Markdown: prefer short paragraphs, use bullet lists for enumerations and fenced code blocks for code or commands, and reserve headings for answers long enough to need them; never emit a dense uninterrupted wall of text. An analytical or exploratory request is answered in prose, never deferred: as soon as the files, searches, or documentation you requested are available, the same turn must carry the analysis the developer asked for. Never conclude a discussion with an empty `message`, and never let an outcome field stand in for an answer you have not written."
        } else {
            "Deliver every user-facing explanation, answer, and summary as conversational text in the user's language, formatted as readable Markdown: prefer short paragraphs, use bullet lists for enumerations and fenced code blocks for code or commands, and reserve headings for answers long enough to need them; never emit a dense uninterrupted wall of text. An analytical or exploratory request is answered in prose, never deferred: as soon as the files, searches, or documentation you requested are available, the same turn must carry the analysis the developer asked for. Never close a turn silently, and never let a terminal outcome stand in for an answer you have not written."
        };
        Some(ContractRule::new("Conversational Response", body))
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

/// Sentence publishing the mockups sandbox to the edit surface.
///
/// A sketch has no place in the change set of the project, so the rule names the
/// sandbox where one belongs instead of leaving the model to infer that a
/// prototype cannot be written at all. The path travels from its single
/// declaration, so the prompt and the admission check never spell it
/// differently.
fn mockups_sandbox_sentence() -> String {
    format!(
        "The `{MOCKUPS_DIR}` directory is a sandbox isolated from Git and excluded from project verification: files there may be created and overwritten freely, while every other path obeys the ordinary edit rules."
    )
}

/// Sentence publishing the protected paths the project declares.
///
/// The patterns travel as one backtick-quoted list so a reader copies an exact
/// spelling, and a declaration that protects nothing publishes no sentence at
/// all. Iteration keeps the declaration order, so identical state renders
/// identical bytes.
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

static FILE_EDITS_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// File edit operations (replace, create, overwrite, move, delete).
pub(crate) struct FileEditsModule;

impl CapabilityModule for FileEditsModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::FileEdits
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("edits")
    }

    /// The pre-canonical spelling of the edit container.
    ///
    /// Transcripts, persisted sessions, and model output produced before
    /// canonicalization still carry it, so it keeps decoding, streaming, and
    /// cleaning exactly like the canonical tag it resolves to.
    fn tag_aliases(&self) -> &'static [&'static str] {
        &["tauqe_edits"]
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(
            &FILE_EDITS_SPECS,
            &[
                "edits",
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

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::changes_fragment(PatchStrategy::SearchReplace))
    }

    /// The change set is the one surface whose fields follow the strategy that
    /// addresses the replaced lines, so it is described for that strategy
    /// instead of once for all of them.
    fn json_schema_fragment_for(&self, patch_strategy: PatchStrategy) -> Option<serde_json::Value> {
        Some(schema::changes_fragment(patch_strategy))
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        // The sandbox and the declared trust boundaries are project facts rather
        // than regime decisions, so both sentences render into the byte-stable
        // head: one module states one rule, and the head stays identical for
        // every dialogue contour.
        let mut body = String::from(
            "Prefer the smallest coherent modification that satisfies the intent. Whenever you propose file edits, precede them with a concise 1-3 sentence explanation of what changed, why, and how the edits achieve the goal; never output edits alone.",
        );
        body.push(' ');
        body.push_str(&mockups_sandbox_sentence());
        if let Some(protected) = protected_paths_sentence(ctx.render.project) {
            body.push(' ');
            body.push_str(&protected);
        }
        Some(ContractRule::new("Minimal Coherent Change", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        // The strategy decides which vocabulary describes the change set, so the
        // reference never advertises a field the active decoder would reject.
        // The profile owns that vocabulary, so the instruction, the wire schema,
        // and the diagnostics of a failed patch cannot describe different fields.
        let profile = StrategyProfile::of(ctx.patch_strategy);
        Some(profile.render_xml_instruction(&|base: &str| ctx.tag_name(base)))
    }

    fn render_structured_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some(
            StrategyProfile::of(ctx.patch_strategy)
                .structured_instruction
                .to_string(),
        )
    }

    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        marker: Option<&str>,
        editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        let mut commands = Vec::new();
        // The anchored vehicle carries the change set as one JSON array, so it is
        // decoded beside the search/replace grammar and independently of it.
        match XmlEditProtocol.parse_anchor_edits(normalized_text, editable_paths) {
            Ok(operations) => commands.extend(
                operations
                    .into_iter()
                    .map(|operation| LlmCommand::Edit { operation }),
            ),
            Err(message) => commands.push(LlmCommand::ProtocolError { message }),
        }
        match XmlParser::new(marker).parse_output(normalized_text, editable_paths) {
            ModelResult::Edit { edits, outcome, .. } => {
                if let Some(message) = outcome.error {
                    commands.push(LlmCommand::ProtocolError { message });
                }
                for operation in edits {
                    commands.push(LlmCommand::Edit { operation });
                }
            }
            ModelResult::Answer { .. } => {}
        }
        commands
    }
}

static DISCOVERY_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Context and symbol discovery (context requests and repository searches).
pub(crate) struct DiscoveryModule;

impl CapabilityModule for DiscoveryModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Discovery
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("context_request")
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

    /// Discovery populates several schema properties, so its fragment declares
    /// the context request together with every repository outline and listing
    /// request in one place.
    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::discovery_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let request = ctx.tag_name(self.canonical_tag()?);
            let search = ctx.tag_name("repo_search");
            let dirs = ctx.tag_name("repo_dirs");
            let list = ctx.tag_name("dir_list");
            if ctx.has_eager_repo_map {
                format!(
                    "Explore codebase structure before loading files. The repository symbol map is already rendered in this prompt, so never request it again. Locate subsystems with <{dirs} />, list files in a directory with <{list} path=\"...\" recursive=\"true|false\" />, locate symbols with <{search} path=\"...\" regex=\"...\" />, and load full file contents with <{request} path=\"...\" access=\"read_only|editable\" /> only when editing or deep inspection is necessary. A discovery tag performs a request only when it appears verbatim at the very end of the response, outside markdown code: emitting the tag itself is the only way to run it, describing the intent in prose performs nothing, and the tag is never deferred to a later response. Batch every request of a round together."
                )
            } else {
                let map = ctx.tag_name("repo_map");
                format!(
                    "Explore codebase structure before loading files. Locate subsystems with <{dirs} />, list files in a directory with <{list} path=\"...\" recursive=\"true|false\" />, inspect symbol contours and type contracts with Tree-sitter <{map} path=\"path/to/file.rs\" /> (extracts signatures, structs, and traits without function bodies to prevent context bloat), locate symbols with <{search} path=\"...\" regex=\"...\" />, and load full file contents with <{request} path=\"...\" access=\"read_only|editable\" /> only when editing or deep inspection is necessary. A discovery tag performs a request only when it appears verbatim at the very end of the response, outside markdown code: emitting the tag itself is the only way to run it, describing the intent in prose performs nothing, and the tag is never deferred to a later response. Batch every request of a round together."
                )
            }
        } else if ctx.has_eager_repo_map {
            // The structured contour states the same investigation duty through
            // the schema fields, so no control tag reaches a prompt whose
            // decoder accepts none.
            "Explore codebase structure before loading files. The repository symbol map is already rendered in this prompt, so never request it again. Set `repo_dirs` to true only when the repository layout is still unknown, list tracked files of a directory with `dir_lists` (`path`, `recursive`), locate symbols with `repo_searches` (`path`, `regex`), and load full file contents with `context_requests` (a repository path, or a path prefixed with `editable:`) only when editing or deep inspection is necessary. A populated field performs the request: describing the intent in prose performs nothing, and a request is never deferred to a later response. Batch every request of a round together.".to_string()
        } else {
            "Explore codebase structure before loading files. Set `repo_dirs` to true only when the repository layout is still unknown, list tracked files of a directory with `dir_lists` (`path`, `recursive`), inspect symbol contours and type contracts with `repo_maps` (`path` per request, returning signatures, structs, and traits without function bodies so context stays small), locate symbols with `repo_searches` (`path`, `regex`), and load full file contents with `context_requests` (a repository path, or a path prefixed with `editable:`) only when editing or deep inspection is necessary. A populated field performs the request: describing the intent in prose performs nothing, and a request is never deferred to a later response. Batch every request of a round together.".to_string()
        };
        Some(ContractRule::new("Discovery First", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let request = ctx.tag_name(self.canonical_tag()?);
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("doc_request")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&DOC_REQUEST_SPECS, &["doc_request"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("doc_requests")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::doc_requests_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let topics = crate::docs::topic_hint();
        // The `<system_documentation>` block is a prompt-side feedback payload,
        // not a request vehicle, so it stays in both contours. The rule states
        // the two-tier policy: conceptual consultation is answered at once from
        // the baseline understanding of the harness, while a detail that must be
        // exact waits for the authoritative documentation.
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let doc = ctx.tag_name(self.canonical_tag()?);
            format!(
                "Never invent exact details. Answer conceptual questions about your identity, capabilities, and working processes, and help the developer set up a project, choose a workflow, or organize the work, from your baseline understanding of this harness, without a documentation request. Details that must be exact — parameters, defaults, budgets, keyboard shortcuts, configuration keys, and API surfaces — are not knowable from that understanding: environment behavior is declared in `tauqe.toml` at the repository root, so they are answered only from the authoritative documentation requested via <{doc} topic=\"{topics}\" />, where topic=\"config\" covers configuration and project preparation. When such a detail is needed, emit the request, output nothing else except a short note, and answer strictly from the returned <system_documentation> blocks in the user's language."
            )
        } else {
            format!(
                "Never invent exact details. Answer conceptual questions about your identity, capabilities, and working processes, and help the developer set up a project, choose a workflow, or organize the work, from your baseline understanding of this harness, without a documentation request. Details that must be exact — parameters, defaults, budgets, keyboard shortcuts, configuration keys, and API surfaces — are not knowable from that understanding: environment behavior is declared in `tauqe.toml` at the repository root, so they are answered only from the authoritative documentation requested through the `doc_requests` field (topics: {topics}), where the `config` topic covers configuration and project preparation. When such a detail is needed, populate that field, keep the rest of the response to a short note, and answer strictly from the returned <system_documentation> blocks in the user's language."
            )
        };
        Some(ContractRule::new("Self-Knowledge", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let doc = ctx.tag_name(self.canonical_tag()?);
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("verify")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&VERIFY_SPECS, &["verify"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("verify")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::verify_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let verify = ctx.tag_name(self.canonical_tag()?);
            format!(
                "Toolchain verification is performed automatically and deterministically by the harness. Emit <{verify} target=\"all|check|test|clippy\" on_success=\"silent|report\" /> only when the user explicitly asks to run verification, compiler checks, or tests to inspect codebase status, and keep on_success=\"silent\" unless the user asked to see raw command output."
            )
        } else {
            "Toolchain verification is performed automatically and deterministically by the harness. Populate the `verify` field with `target` 'all', 'check', 'test', or 'clippy' and `on_success` 'silent' or 'report' only when the user explicitly asks to run verification, compiler checks, or tests to inspect codebase status, and keep `on_success` at 'silent' unless the user asked to see raw command output.".to_string()
        };
        Some(ContractRule::new("Code Verification", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let verify = ctx.tag_name(self.canonical_tag()?);
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
/// Intent-independent trunk of the structured plan rule.
///
/// It states only the invariants every plan carries — the mandatory concept
/// tree, the file attribute contract and its leaf-only restriction, the step
/// formulation contract, the plan creation invariant, and language consistency —
/// so it renders byte-identically whichever dialogue regime authors the plan.
/// The actions a regime authorizes live in the delta constants below.
const PLAN_STRUCTURED_CORE_BODY: &str = "Every plan travels as one structured `plan_update` field carrying `action`, plan `id`, `title`, `description`, `concept`, and `items`. Conceptual Foundation Invariant: Every plan MUST contain an architectural concept tree in `concept` defining the core requirements, design decisions, and system invariants ('What'). Inside `concept`, define architectural nodes carrying an `id` (`c-...`), a `title`, and their `details`. Plan creation without `concept` containing at least one concept node is strictly prohibited. Implementation steps in `items` define 'How', may be included with `concept_refs` (`c-...`), `files`, and `readonly_files`, or omitted if awaiting deep refinement. File Attributes Contract: `files` lists target files to create or modify (loaded as editable context), while `readonly_files` lists reference or contract files such as subsystem conventions, protocol types, and interfaces (loaded as read-only context and never modified). Leaf-Only Files Invariant: `files` and `readonly_files` must be specified exclusively for executable leaf steps without children; parent container items and task groups must never carry files. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Always provide a clear `description` explaining the overall architecture, boundaries, and rationale. Plan Creation Invariant: Only create a plan when explicitly instructed or confirmed by the user. Whenever you propose a new plan, you MUST precede the payload with a concise conversational explanation of the proposed plan, and you MUST populate `plan_update` in the same response, carrying both `concept` (mandatory) and optional `items`. Language Consistency: Formulate the plan title, description, concept nodes, step titles, and step details in the same language as the user's request.";

/// Regime delta of the structured plan rule for a develop turn.
///
/// It names the only plan action a develop turn may take and the reins that
/// guard it, together with the planning objective of that regime.
const PLAN_STRUCTURED_DEVELOP_DELTA: &str = "In Develop mode, you may ONLY propose new plans (`action: \"save\"`). Updating plan statuses, closing steps, or modifying existing plans from Develop mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Reins Invariant: Never populate `plan_update` with a payload without an explicit user instruction or confirmation to formulate a plan. Questions, analysis, architectural reviews, or exploratory discussion must be answered strictly in conversational text in `message`. Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn or deep file inspection.";

/// Regime delta of the structured plan rule for a plan discussion.
///
/// It declares the whole plan action surface the Discussion mutation mode
/// authorizes, states that direct code changes travel the ordinary transactional
/// path, and carries the collaborative planning objective.
const PLAN_STRUCTURED_DISCUSSION_DELTA: &str = "In plan discussion mode, populate the structured `plan_update` field with `action: \"save\"|\"update\"|\"delete\"`. Plan Action Semantics: use `action: \"save\"` to create or completely overwrite/replace the entire plan; use `action: \"update\"` strictly to modify specific individual nodes or items in place by ID without touching unmentioned items; use `action: \"delete\"` to remove the plan. Code modifications requested directly by the developer are permitted alongside the discussion, and they travel the ordinary transactional path. Reins Invariant: Never mutate the plan (leave `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`. Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn.";

/// Intent-independent trunk of the XML plan rule.
///
/// The trunk states the invariants every plan carries and the shape example of
/// the contour, so it renders identically whichever dialogue regime authors the
/// plan. The authorized actions live in [`plan_xml_delta`].
fn plan_xml_core(ctx: &ContractContext<'_>, plan: &str) -> String {
    let description = ctx.tag_name("description");
    let concept = ctx.tag_name("concept");
    let concept_item = ctx.tag_name("concept_item");
    let details = ctx.tag_name("details");
    let item = ctx.tag_name("item");
    format!(
        "Every plan travels as one structured <{plan} action=\"save\" id=\"plan-id\" title=\"Plan Title\">...</{plan}> container. Conceptual Foundation Invariant: Every plan MUST contain an architectural concept tree in <{concept}>...</{concept}> defining the core requirements, design decisions, and system invariants ('What'). Inside <{concept}>, define architectural nodes using structured <{concept_item} id=\"c-...\" title=\"...\"><{details}>...</{details}></{concept_item}> tags. Plan creation without <{concept}> containing at least one <{concept_item}> is strictly prohibited. Implementation steps in <{item}> define 'How', may be included with concept_refs=\"c-...\", files=\"...\", and readonly=\"...\", or omitted if awaiting deep refinement. File Attributes Contract: the `files` attribute lists target files to create or modify (loaded as editable context), while the `readonly` attribute lists reference or contract files such as subsystem conventions, protocol types, and interfaces (loaded as read-only context and never modified). Leaf-Only Files Invariant: the `files` and `readonly` attributes must be specified exclusively for executable leaf steps without child items; parent container items and task groups must never have files. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Always provide a clear <{description}> explaining the overall architecture, boundaries, and rationale. Plan Creation Invariant: Only create a plan when explicitly instructed or confirmed by the user. Whenever you propose a new plan, you MUST precede the tag with a concise conversational explanation of the proposed plan, and you MUST emit the structured <{plan} action=\"save\" ...> tag in the same response containing both <{concept}> (mandatory) and optional <{item}> elements. Language Consistency: Formulate the plan title, description, concept nodes, step titles, and step details in the same language as the user's request.\nStructured Plan XML Format:\n<{plan} action=\"save\" id=\"plan-id\" title=\"Plan Title\">\n  <{description}>Overall architecture and rationale</{description}>\n  <{concept}>\n    <{concept_item} id=\"c-1\" title=\"Core Invariant\">\n      <{details}>Design rationale and invariants</{details}>\n    </{concept_item}>\n  </{concept}>\n  <{item} id=\"1\" title=\"Step Title\" concept_refs=\"c-1\" files=\"path/to/file.rs\" readonly=\"path/to/contract.rs\">\n    <{details}>Engineering objective and verification criteria</{details}>\n  </{item}>\n</{plan}>"
    )
}

/// Regime delta of the XML plan rule, carrying the authorized plan actions.
///
/// A develop turn may only propose a new plan, while a plan discussion may also
/// modify one in place and delete it; both variants state that the direct code
/// changes a discussion permits travel the ordinary transactional path.
fn plan_xml_delta(ctx: &ContractContext<'_>, plan: &str) -> String {
    if ctx.intent.is_plan_discussion() {
        format!(
            "In plan discussion mode, <{plan} action=\"save\" ...> creates a plan, <{plan} action=\"update\" ...> modifies specific concept nodes or items in place by ID without touching unmentioned items, and <{plan} action=\"delete\" ...> removes the plan. Code modifications requested directly by the developer are permitted alongside the discussion, and they travel the ordinary transactional path. Reins Invariant: Never emit a plan tag without an explicit user instruction to formulate or modify a plan. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text without plan tags. Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn."
        )
    } else {
        format!(
            "In Develop mode, you may ONLY propose new plans (<{plan} action=\"save\" ...>). Updating plan statuses, closing steps, or modifying existing plans from Develop mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Reins Invariant: Never emit a plan tag without an explicit user instruction or confirmation to formulate a plan. Questions, analysis, architectural reviews, or exploratory discussion must be answered strictly in conversational text without plan tags. Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn or deep file inspection."
        )
    }
}

/// Project plan creation.
pub(crate) struct PlanCreationModule;

impl CapabilityModule for PlanCreationModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::PlanCreation
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("plan")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        plan_specs()
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("plan_update")
    }

    /// The plan payload reuses the authoritative discussion schema, so both
    /// contours describe the very same plan shape.
    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::plan_update_fragment())
    }

    /// The nested plan tree travels as shared definitions, referenced from the
    /// fragment by pointer.
    fn json_schema_defs(&self) -> Option<serde_json::Value> {
        Some(schema::plan_update_definitions())
    }

    /// Core rule: the invariants every plan carries, whichever regime authors it.
    ///
    /// The rule is deliberately free of [`ContractContext::intent`]: the head it
    /// renders into is shared by every dialogue contour, so the plan actions a
    /// regime authorizes are declared by [`CapabilityModule::render_delta_rule`].
    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let plan = ctx.tag_name(self.canonical_tag()?);
        let body = if ctx.protocol == EditProtocolKind::Xml {
            plan_xml_core(ctx, &plan)
        } else {
            PLAN_STRUCTURED_CORE_BODY.to_string()
        };
        Some(ContractRule::new("Local Engineering Plans", body))
    }

    /// Delta rule: which plan actions the declared regime authorizes.
    fn render_delta_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let plan = ctx.tag_name(self.canonical_tag()?);
        let body = if ctx.protocol == EditProtocolKind::Xml {
            plan_xml_delta(ctx, &plan)
        } else if ctx.intent.is_plan_discussion() {
            PLAN_STRUCTURED_DISCUSSION_DELTA.to_string()
        } else {
            PLAN_STRUCTURED_DEVELOP_DELTA.to_string()
        };
        Some(ContractRule::new("Local Engineering Plan Actions", body))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `plan_update`: structured plan creation or modification (`action`, `id`, `title`, `description`, `concept`, `items`), or null when the plan is not being changed.".to_string())
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let plan = ctx.tag_name(self.canonical_tag()?);
        let concept = ctx.tag_name("concept");
        let concept_item = ctx.tag_name("concept_item");
        let item = ctx.tag_name("item");
        let details = ctx.tag_name("details");
        let description = ctx.tag_name("description");
        Some(format!(
            "- Plans: propose a structured plan inside <{plan} action=\"save\" id=\"...\" title=\"...\">...</{plan}> only when explicitly instructed or confirmed by the user. A plan is a two-section document: a mandatory <{concept}> section of <{concept_item} id=\"c-...\" title=\"...\"><{details}>...</{details}></{concept_item}> nodes defining 'What', and <{item} id=\"...\" title=\"...\" concept_refs=\"c-...\" files=\"...\" readonly=\"...\"><{details}>...</{details}></{item}> steps defining 'How'. <{description}> carries the overall architecture and rationale. The `files` attribute lists target files loaded as editable context and `readonly` lists reference or contract files loaded as read-only; both are allowed only on executable leaf steps without children. In Develop mode only new plans may be proposed; plan modifications belong to discussion mode."
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("plan")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        plan_specs()
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("plan_update")
    }

    /// A plan update decodes through the very same structured field as a plan
    /// creation, so both capabilities declare one fragment.
    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::plan_update_fragment())
    }

    fn json_schema_defs(&self) -> Option<serde_json::Value> {
        Some(schema::plan_update_definitions())
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("scratchpad")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&SCRATCHPAD_SPECS, &["scratchpad"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("scratchpad")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::scratchpad_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let scratchpad = ctx.tag_name(self.canonical_tag()?);
            format!(
                "Use <{scratchpad}>...</{scratchpad}> to keep working hypotheses, verified findings, and the purpose of requested files across multi-round discovery and verification healing cycles. Keep notes concise and bullet-shaped; notes stay isolated to the current turn and never enter persisted history."
            )
        } else {
            "Use the `scratchpad` field to keep working hypotheses, verified findings, and the purpose of requested files across multi-round discovery and verification healing cycles. Keep notes concise and bullet-shaped; notes stay isolated to the current turn and never enter persisted history.".to_string()
        };
        Some(ContractRule::new("Turn Scratchpad (Working Memory)", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let scratchpad = ctx.tag_name(self.canonical_tag()?);
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("success")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&OUTCOME_SPECS, &["success", "error"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("outcome")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::outcome_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let success = ctx.tag_name(self.canonical_tag()?);
            let error = ctx.tag_name("error");
            format!(
                "Conclude every turn with an explicit terminal outcome that carries its own payload: <{success} summary=\"...\" /> on success or <{error} message=\"...\" /> on failure. A terminal status without its payload is a protocol defect rather than a result, so it never concludes a turn as a validated conclusion: a success states a non-empty summary of what was achieved, and a failure states the message that the turn failed."
            )
        } else {
            "Conclude every turn by populating the `outcome` field with an explicit terminal status that carries its own payload: `status` 'success' together with a non-empty `summary` when the turn succeeds, `status` 'error' together with a `message` when it fails, or `status` 'skip' together with a `reason` when the work is already satisfied and no change is required. A terminal status without its payload is a protocol defect rather than a result, so it never concludes a turn as a validated conclusion.".to_string()
        };
        Some(ContractRule::new("Terminal Outcome", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let success = ctx.tag_name(self.canonical_tag()?);
        let error = ctx.tag_name("error");
        Some(format!(
            "- Terminal outcome: conclude with <{success} summary=\"...\" /> on success or <{error} message=\"...\" /> on failure, and always state the payload of the status you declare."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `outcome`: terminal outcome object with `status` ('none' while the turn continues, 'success' when it succeeds, 'error' when it fails, 'skip' when the work is already satisfied and no change is required), `summary` for a success summary, `message` for an error message, and `reason` for a skip reason; unrelated fields stay empty. Every terminal status carries its own payload: a non-empty `summary` for 'success', the `message` for 'error', and the `reason` for 'skip'. A terminal status without its payload is a protocol defect rather than a result.".to_string())
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

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("blocked")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&BLOCKED_SPECS, &["blocked"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("blocked")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::blocked_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let blocked = ctx.tag_name(self.canonical_tag()?);
            format!(
                "Emit <{blocked} reason=\"...\" details=\"...\" /> when a precondition or assumption fails instead of guessing or silently failing."
            )
        } else {
            "Set the `blocked` field with `is_blocked` true, a `reason`, and optional `details` when a precondition or assumption fails, instead of guessing or silently failing.".to_string()
        };
        Some(ContractRule::new("Semantic Blocker", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let blocked = ctx.tag_name(self.canonical_tag()?);
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

static ABORT_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Unconditional abort requiring host-environment or developer action.
///
/// An abort is not a blocker. A blocker reports a failed precondition and the
/// harness replans around it; an abort reports work the harness cannot perform
/// at all — approving a native build, installing a package, starting a daemon —
/// and stops the turn so the developer can act. The tag therefore carries two
/// human-facing payloads: `reason` names what blocked the work, and
/// `recommendation` carries the exact command or instruction to run.
pub(crate) struct AbortModule;

/// Decodes every `<abort ... />` invocation from normalized protocol text.
///
/// The reason travels as an attribute because it names what blocked the work,
/// while the recommendation may travel either as an attribute or as the body of
/// a paired tag, so a longer instruction with line breaks stays readable. An
/// invocation that carries neither payload is dropped instead of producing an
/// empty abort: a stray tag must never stop a turn for a reason nobody stated.
///
/// The structured decoder reuses this routine for its control-tag fallback, so
/// an abort reaches the harness as one payload whichever vehicle carried it.
pub(crate) fn abort_invocations(text: &str) -> Vec<(String, String)> {
    let Some(spec) = engine::tag_spec("abort") else {
        return Vec::new();
    };
    engine::iter_tags(text, spec)
        .filter_map(|found| {
            let reason = found.attr("reason").unwrap_or_default();
            let recommendation = found.attr_or_body(&["recommendation"]).unwrap_or_default();
            if reason.trim().is_empty() && recommendation.trim().is_empty() {
                return None;
            }
            Some((reason, recommendation))
        })
        .collect()
}

impl CapabilityModule for AbortModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::Abort
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("abort")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&ABORT_SPECS, &["abort"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("abort")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::abort_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if ctx.protocol == EditProtocolKind::Xml {
            let abort = ctx.tag_name(self.canonical_tag()?);
            format!(
                "An abort is not a blocker. Emit a self-closing <{abort} reason=\"...\" recommendation=\"...\" /> invocation when the remaining work needs an action only a human can perform on the host system: approving a native build, installing a package or a system utility, starting a daemon or a database, or authenticating against an external registry. The harness stops the turn at once and rolls back the incomplete change set, so `reason` names the concrete missing capability while `recommendation` carries the exact terminal command or instruction the developer must run. Replacing the required work with a stub, a surrogate, or a placeholder to keep the turn going is strictly prohibited."
            )
        } else {
            "An abort is not a blocker. Set the `abort` field with `is_aborted` true when the remaining work needs an action only a human can perform on the host system: approving a native build, installing a package or a system utility, starting a daemon or a database, or authenticating against an external registry. The harness stops the turn at once and rolls back the incomplete change set, so `reason` names the concrete missing capability while `recommendation` carries the exact terminal command or instruction the developer must run. Replacing the required work with a stub, a surrogate, or a placeholder to keep the turn going is strictly prohibited.".to_string()
        };
        Some(ContractRule::new("Unconditional Abort", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let abort = ctx.tag_name(self.canonical_tag()?);
        Some(format!(
            "- Unconditional abort: emit <{abort} reason=\"...\" recommendation=\"...\" /> when the repair needs a human action in the host terminal, such as installing a package or a system utility, approving build scripts, starting a database or a daemon, or authenticating an external registry. Never emit a stub, a surrogate, or a placeholder implementation instead, and never escalate the decision to an architect replan: name the concrete missing capability in `reason` and the exact copy-pasteable command in `recommendation`. The tag may stay self-closing, or carry the instruction as its body when the recommendation needs several lines."
        ))
    }

    fn render_structured_instruction(&self, _ctx: &PromptRenderContext<'_>) -> Option<String> {
        Some("- `abort`: host-environment abort object with `is_aborted` (true only when the remaining work needs an action only a human can perform on the host system), `reason` (the concrete missing capability), and `recommendation` (the exact command or instruction the developer must run); keep `is_aborted` false and both strings empty when nothing needs a developer action.".to_string())
    }

    /// Decodes every `<abort reason="..." recommendation="..." />` invocation.
    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        _marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        abort_invocations(normalized_text)
            .into_iter()
            .map(|(reason, recommendation)| LlmCommand::Abort {
                reason,
                recommendation,
            })
            .collect()
    }
}

static USER_LANGUAGE_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

/// Detected primary natural language of the user prompt.
pub(crate) struct UserLanguageModule;

impl CapabilityModule for UserLanguageModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::UserLanguage
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("user_language")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&USER_LANGUAGE_SPECS, &["user_language"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("user_language")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::user_language_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let body = if let Some(language) = ctx.target_language {
            format!(
                "The user's language is {language}. All conversational explanations, answers, and messages MUST be in {language}. Do NOT switch to English for conversational responses unless requested."
            )
        } else if ctx.protocol == EditProtocolKind::Xml {
            // The XML contour names the control tag through the active turn
            // marker, whichever dialogue regime renders the rule: a plan
            // discussion runs on the configured transport, so it spells its
            // language tag exactly like the interactive dialogue.
            let language = ctx.tag_name(self.canonical_tag()?);
            format!(
                "Always formulate conversational explanations and user-facing answers in the same language as the user's request. Identify the user's language using <{language}>language_name</{language}> (e.g. <{language}>Russian</{language}>). Do NOT switch to English for conversational responses unless requested."
            )
        } else {
            "Always formulate conversational explanations and user-facing answers in the same language as the user's request. Identify the user's language in the `user_language` field (e.g. \"Russian\"). Do NOT switch to English for conversational responses unless requested.".to_string()
        };
        Some(ContractRule::new("Language Consistency", body))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let language = ctx.tag_name(self.canonical_tag()?);
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

/// Project-declared custom commands published to the model as typed tools.
///
/// The module never invents an operation: it renders exactly the allowlist the
/// `[project.commands]` declaration carries, so the model learns which project
/// operations exist while remaining unable to compose, chain, or extend a
/// command line. It contributes no control tag, because an invocation travels
/// through a typed field rather than through protocol markup.
pub(crate) struct CustomToolsModule;

/// Reports whether `text` already closes a sentence.
fn ends_sentence(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | ':' | '!' | '?'))
}

/// Renders one parameter contract as `name="default"` entries.
///
/// A default travels with its parameter because leaving the parameter out is how
/// a caller selects it, and the author-provided description follows the entry
/// when the declaration carries one. Entries keep declaration order, so
/// identical state renders identical bytes.
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

/// Renders the declared allowlist as one bullet per runnable command.
///
/// A bullet states what the command is for, which parameters it accepts
/// together with their declared defaults, and the command line that actually
/// runs. The command line shows the literal arguments the declaration carries
/// and one placeholder per declared parameter, so a caller reads the signature
/// it may invoke instead of guessing it.
///
/// A declaration whose reference chain is broken is skipped instead of being
/// described: `ProjectConfig` validation rejects such a configuration before a
/// turn runs, so the render path never guesses a command line that resolution
/// already refused. Iteration follows the declaration map order, so identical
/// state renders identical bytes.
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

static CUSTOM_TOOL_SPECS: OnceLock<Vec<&'static XmlTagSpec>> = OnceLock::new();

impl CapabilityModule for CustomToolsModule {
    fn capability(&self) -> LlmCapability {
        LlmCapability::CustomTools
    }

    fn canonical_tag(&self) -> Option<&'static str> {
        Some("custom_tool")
    }

    fn xml_tag_specs(&self) -> &'static [&'static XmlTagSpec] {
        resolve_specs(&CUSTOM_TOOL_SPECS, &["custom_tool"])
    }

    fn json_property_name(&self) -> Option<&'static str> {
        Some("custom_tools")
    }

    fn json_schema_fragment(&self) -> Option<serde_json::Value> {
        Some(schema::custom_tools_fragment())
    }

    fn render_contract_rule(&self, ctx: &ContractContext<'_>) -> Option<ContractRule> {
        let project = ctx.render.project?;
        if project.commands.is_empty() {
            return None;
        }
        let tools = render_custom_tool_list(project);
        Some(ContractRule::new(
            "Project Custom Tools (No Arbitrary Shell)",
            format!(
                "The project declares a closed allowlist of commands in `[project.commands]`. You may invoke them strictly by name:{tools}\n  An invocation runs one declared command line unchanged: you cannot compose, chain, extend, or redirect it, and you have no shell to fall back on. A declared parameter may be supplied by name, and a parameter left out keeps its declared default; anything the declaration does not name is rejected instead of reaching a process. A supplied value is one literal argument: it carries no shell syntax and is never split, expanded, or interpreted. When no declared command covers the required operation, report a semantic blocker instead of improvising one."
            ),
        ))
    }

    fn render_xml_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let project = ctx.project?;
        if project.commands.is_empty() {
            return None;
        }
        let tag = ctx.tag_name(self.canonical_tag()?);
        let tools = render_custom_tool_list(project);
        Some(format!(
            "- Project custom tools: invoke one declared command with <{tag} name=\"...\" /> to run it, and read its exit status, stdout, and stderr in the next round. Supply a declared parameter as one further attribute named after it, as in <{tag} name=\"...\" n=\"10\" />, and omit a parameter to keep its declared default. Only declared names and declared parameters are accepted:{tools}\n  An attribute value is one literal argument: it carries no shell syntax and is never split, expanded, or interpreted, and an attribute the declaration does not name is rejected instead of being forwarded to a process. When no declared command covers the required operation, report a semantic blocker instead of improvising one."
        ))
    }

    fn render_structured_instruction(&self, ctx: &PromptRenderContext<'_>) -> Option<String> {
        let project = ctx.project?;
        if project.commands.is_empty() {
            return None;
        }
        let names = project
            .commands
            .keys()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "- `custom_tools`: array of typed project command invocations (`name`, `args`) restricted to the declared allowlist ({names}). Each invocation runs exactly the declared command line, so `args` stays empty; an empty array requests no execution."
        ))
    }

    /// Decodes every `<custom_tool name="..." param="..." />` invocation.
    fn decode_xml(
        &self,
        normalized_text: &str,
        _raw_text: &str,
        _marker: Option<&str>,
        _editable_paths: &[String],
    ) -> Vec<LlmCommand> {
        custom_tool_invocations(normalized_text)
            .into_iter()
            .map(|(name, args)| LlmCommand::CustomTool { name, args })
            .collect()
    }
}

/// Collects the invocation arguments carried by a `<custom_tool>` header.
///
/// Every attribute except `name` is one declared parameter value, so
/// `<custom_tool name="log-tail" n="10" />` decodes into `{"n": "10"}`. An
/// invocation that names no parameter keeps an absent argument object rather
/// than an empty one, and the attributes keep their source order, so identical
/// markup always produces identical arguments.
fn invocation_arguments(header: &str) -> serde_json::Value {
    let mut arguments = serde_json::Map::new();
    for (key, value) in header_attributes(header) {
        if key == "name" {
            continue;
        }
        arguments.insert(key, serde_json::Value::String(value));
    }
    if arguments.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(arguments)
    }
}

/// Decodes every `<custom_tool name="..." param="..." />` invocation from
/// normalized protocol text.
///
/// The `name` attribute addresses one declared command and every remaining
/// attribute is one declared parameter value, so the tag header itself is the
/// typed argument object. Whether a name or a parameter is actually declared is
/// decided by the harness at execution time, where an unknown one is answered
/// with a diagnostic block instead of reaching a process.
///
/// The structured decoder reuses this routine for its control-tag fallback, so
/// an invocation reaches the harness as one payload whichever vehicle carried
/// it.
pub(crate) fn custom_tool_invocations(text: &str) -> Vec<(String, serde_json::Value)> {
    let Some(spec) = engine::tag_spec("custom_tool") else {
        return Vec::new();
    };
    engine::iter_tags(text, spec)
        .filter_map(|found| {
            let name = found.attr("name")?;
            Some((name, invocation_arguments(found.header)))
        })
        .collect()
}

/// Reads every `key="value"` assignment of a tag header, in source order.
///
/// The scanner reads one assignment at a time: the key is the token directly
/// before `=`, and the value is the quoted text that follows. An unquoted value
/// is read up to the next whitespace, `>` or `/`, so the slash of a self-closing
/// tag never leaks into a value and a quoted value may contain both spaces and
/// equals signs. A header truncated inside a value yields the assignments
/// completed so far, because decoding runs on a finished round rather than on a
/// live stream.
fn header_attributes(header: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut rest = header;
    while let Some(equals) = rest.find('=') {
        let key = rest[..equals]
            .rsplit(|c: char| c.is_whitespace() || c == '<' || c == '/')
            .find(|part| !part.is_empty())
            .unwrap_or_default()
            .to_string();

        let value_part = rest[equals + 1..].trim_start();
        let Some(quote) = value_part
            .chars()
            .next()
            .filter(|quote| *quote == '"' || *quote == '\'')
        else {
            let end = value_part
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .unwrap_or(value_part.len());
            if !key.is_empty() {
                attributes.push((key, value_part[..end].trim().to_string()));
            }
            rest = &value_part[end..];
            continue;
        };

        let body = &value_part[quote.len_utf8()..];
        let Some(end) = body.find(quote) else {
            break;
        };
        if !key.is_empty() {
            attributes.push((key, body[..end].to_string()));
        }
        rest = &body[end + quote.len_utf8()..];
    }
    attributes
}

#[cfg(test)]
mod tests {
    use crate::capabilities::registry::CapabilityModule;
    use tauqe_protocol::{EditProtocolKind, PromptLayoutKind, TurnIntent};

    use super::*;

    /// Parses a `[project]` declaration from its JSON form.
    fn project(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the project section parses")
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
    fn header_attributes_read_every_assignment_in_source_order() {
        let attributes =
            header_attributes("<custom_tool name=\"log-tail\" n=\"10\" follow='true' bare=1 />");

        assert_eq!(
            attributes,
            vec![
                ("name".to_string(), "log-tail".to_string()),
                ("n".to_string(), "10".to_string()),
                ("follow".to_string(), "true".to_string()),
                ("bare".to_string(), "1".to_string()),
            ]
        );

        // A quoted value keeps its spaces and equals signs.
        let quoted = header_attributes("<custom_tool name=\"run\" query=\"a = b\" />");
        assert_eq!(
            quoted,
            vec![
                ("name".to_string(), "run".to_string()),
                ("query".to_string(), "a = b".to_string()),
            ]
        );
    }

    #[test]
    fn the_module_decodes_parameters_into_a_typed_argument_object() {
        let module = CustomToolsModule;
        let text = "<custom_tool name=\"log-tail\" n=\"10\" />";

        assert_eq!(
            module.decode_xml(text, text, None, &[]),
            vec![LlmCommand::CustomTool {
                name: "log-tail".to_string(),
                args: serde_json::json!({ "n": "10" }),
            }]
        );
    }

    #[test]
    fn an_invocation_without_parameters_keeps_an_absent_argument_object() {
        let module = CustomToolsModule;
        let text = "<custom_tool name=\"check\" />";

        assert_eq!(
            module.decode_xml(text, text, None, &[]),
            vec![LlmCommand::CustomTool {
                name: "check".to_string(),
                args: serde_json::Value::Null,
            }]
        );
    }

    #[test]
    fn the_prompt_publishes_the_description_and_the_parameter_contract() {
        let config = project(
            r#"{
                "commands": {
                    "log-tail": {
                        "command": "just",
                        "args": ["log-tail"],
                        "description": "Print the last N lines of the server journal",
                        "parameters": [{ "name": "n", "default": "50" }]
                    }
                }
            }"#,
        );
        let render = PromptRenderContext::new(&[], None).with_project(Some(&config));
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render);

        let module = CustomToolsModule;
        let rule = module
            .render_contract_rule(&ctx)
            .expect("a declared allowlist publishes a rule");
        assert_eq!(rule.title, "Project Custom Tools (No Arbitrary Shell)");
        assert!(rule
            .body
            .contains("`log-tail`: Print the last N lines of the server journal."));
        // The command line carries one placeholder per declared parameter, so a
        // caller reads the signature it may invoke instead of guessing it.
        assert!(rule.body.contains("Runs `just log-tail [n]`"));
        assert!(rule.body.contains("Parameters: [n=\"50\"]"));

        let instruction = module
            .render_xml_instruction(&ctx.render)
            .expect("a declared allowlist publishes an instruction");
        assert!(instruction.contains("<custom_tool name"));
        assert!(instruction.contains("Runs `just log-tail [n]`"));
        assert!(instruction.contains("Parameters: [n=\"50\"]"));

        // Without a declaration neither fragment is rendered, so the prompt
        // never advertises a tool surface the decoder rejects.
        let bare = PromptRenderContext::new(&[], None);
        assert!(module.render_xml_instruction(&bare).is_none());
        assert!(module
            .render_contract_rule(&ContractContext::new(
                TurnIntent::InteractiveRepl,
                EditProtocolKind::Xml,
                bare,
            ))
            .is_none());
    }

    #[test]
    fn the_anchor_strategy_renders_range_fields_and_the_change_set_vehicle() {
        let module = FileEditsModule;
        let render =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::LineAnchor);
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render);

        let instruction = module
            .render_xml_instruction(&ctx.render)
            .expect("the edit module always renders an instruction");
        assert!(instruction.contains("<file_edits>"));
        assert!(instruction.contains("`start`"));
        assert!(instruction.contains("`end`"));
        assert!(instruction.contains("bottom of the file upwards"));
        assert!(!instruction.contains("<edit path"));

        let structured = module
            .render_structured_instruction(&ctx.render)
            .expect("the edit module always renders a structured instruction");
        assert!(structured.contains("`start`"));
        assert!(structured.contains("`end`"));
        assert!(!structured.contains("`old_text`"));

        // The declared schema follows the same strategy, so the advertised
        // fields and the advertised reference never disagree.
        let fragment = module
            .json_schema_fragment_for(PatchStrategy::LineAnchor)
            .expect("the change set is always declared");
        let properties = fragment["changes"]["items"]["properties"]
            .as_object()
            .expect("the change entry declares its fields");
        assert!(properties.contains_key("start"));
        assert!(properties.contains_key("end"));
        assert!(!properties.contains_key("old_text"));
    }

    #[test]
    fn the_abort_module_decodes_the_reason_and_the_recommendation() {
        let module = AbortModule;

        // A self-closing invocation carries both payloads as attributes.
        let self_closing = "<abort reason=\"Native build approval required\" recommendation=\"pnpm approve-builds\" />";
        assert_eq!(
            module.decode_xml(self_closing, self_closing, None, &[]),
            vec![LlmCommand::Abort {
                reason: "Native build approval required".to_string(),
                recommendation: "pnpm approve-builds".to_string(),
            }]
        );

        // A paired invocation carries a longer instruction in its body.
        let paired =
            "<abort reason=\"Toolchain missing\">Install the SDK:\n  rustup component add clippy</abort>";
        assert_eq!(
            module.decode_xml(paired, paired, None, &[]),
            vec![LlmCommand::Abort {
                reason: "Toolchain missing".to_string(),
                recommendation: "Install the SDK:\n  rustup component add clippy".to_string(),
            }]
        );

        // An invocation that states nothing is dropped instead of producing an
        // empty abort that would stop a turn for no stated reason.
        assert!(module
            .decode_xml("<abort />", "<abort />", None, &[])
            .is_empty());

        // Prose that merely names the tag decodes nothing.
        let prose = "Finish the turn with an <abort> tag.";
        assert!(module.decode_xml(prose, prose, None, &[]).is_empty());
    }

    #[test]
    fn the_abort_module_renders_the_contract_rule_and_the_instruction() {
        let module = AbortModule;
        let render = PromptRenderContext::new(&[], None);
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render);

        let rule = module
            .render_contract_rule(&ctx)
            .expect("the abort rule is always rendered");
        assert_eq!(rule.title, "Unconditional Abort");
        assert!(rule.body.contains("not a blocker"));
        assert!(rule.body.contains("recommendation"));
        assert!(rule.body.contains("stub"));
        assert!(rule.body.contains("rolls back the incomplete change set"));

        let instruction = module
            .render_xml_instruction(&ctx.render)
            .expect("the abort instruction is always rendered");
        assert!(instruction.contains("recommendation"));
        assert!(instruction.contains("stub"));
        assert!(instruction.contains("approving build scripts"));
    }

    #[test]
    fn the_abort_rule_and_instruction_fold_the_turn_marker() {
        let module = AbortModule;
        let render = PromptRenderContext::new(&[], Some("M12345"));
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render);

        let rule = module
            .render_contract_rule(&ctx)
            .expect("the abort rule is rendered");
        assert!(rule.body.contains("abort_M12345"));
        assert!(!rule.body.contains("abort reason"));

        let instruction = module
            .render_xml_instruction(&ctx.render)
            .expect("the abort instruction is rendered");
        assert!(instruction.contains("abort_M12345"));
        assert!(!instruction.contains("abort reason"));
    }

    #[test]
    fn the_conversational_response_rule_requires_a_written_answer() {
        let module = TextMessageModule;

        let develop = ContractContext::new(
            TurnIntent::InteractiveRepl,
            EditProtocolKind::Xml,
            PromptRenderContext::new(&[], None),
        );
        let rule = module
            .render_contract_rule(&develop)
            .expect("the conversational response rule is always rendered");
        assert_eq!(rule.title, "Conversational Response");
        // The language obligation survives, and the answer obligation is added
        // beside it instead of replacing it.
        assert!(rule.body.contains("in the user's language"));
        assert!(rule.body.contains("never deferred"));
        assert!(rule.body.contains("same turn must carry the analysis"));
        assert!(rule.body.contains("Never close a turn silently"));
        assert!(!rule.body.contains("`message`"));

        let discussion = ContractContext::new(
            TurnIntent::PlanDiscussion,
            EditProtocolKind::Structured,
            PromptRenderContext::new(&[], None),
        );
        let rule = module
            .render_contract_rule(&discussion)
            .expect("the discussion response rule is always rendered");
        assert!(rule.body.contains("`message` field"));
        assert!(rule
            .body
            .contains("Never conclude a discussion with an empty `message`"));
        assert!(rule
            .body
            .contains("never let an outcome field stand in for an answer"));
    }

    #[test]
    fn the_conversational_response_rule_requires_readable_markdown() {
        let module = TextMessageModule;

        // The formatting duty is identical in both contours: the rendered rule
        // always obligates readable Markdown instead of a dense wall of text.
        for intent in [TurnIntent::InteractiveRepl, TurnIntent::PlanDiscussion] {
            let ctx = ContractContext::new(
                intent,
                EditProtocolKind::Xml,
                PromptRenderContext::new(&[], None),
            );
            let rule = module
                .render_contract_rule(&ctx)
                .expect("the conversational response rule is always rendered");
            assert!(rule.body.contains("readable Markdown"), "{intent:?}");
            assert!(rule.body.contains("bullet lists"), "{intent:?}");
            assert!(rule.body.contains("fenced code blocks"), "{intent:?}");
            assert!(rule.body.contains("wall of text"), "{intent:?}");
        }
    }

    /// Builds the contract context of one wire contour for an interactive turn.
    fn contour(protocol: EditProtocolKind) -> ContractContext<'static> {
        ContractContext::new(
            TurnIntent::InteractiveRepl,
            protocol,
            PromptRenderContext::new(&[], None),
        )
    }

    /// Renders the contract rule of `module` in the declared wire contour.
    fn rule_in(module: &dyn CapabilityModule, protocol: EditProtocolKind) -> ContractRule {
        let capability = module.capability();
        module
            .render_contract_rule(&contour(protocol))
            .unwrap_or_else(|| panic!("{capability} renders a contract rule"))
    }

    /// Builds the contract context of one wire contour for a plan discussion.
    fn discussion_contour(protocol: EditProtocolKind) -> ContractContext<'static> {
        ContractContext::new(
            TurnIntent::PlanDiscussion,
            protocol,
            PromptRenderContext::new(&[], None),
        )
    }

    /// Renders the contract rule of `module` for a plan discussion.
    fn discussion_rule_in(
        module: &dyn CapabilityModule,
        protocol: EditProtocolKind,
    ) -> ContractRule {
        let capability = module.capability();
        module
            .render_contract_rule(&discussion_contour(protocol))
            .unwrap_or_else(|| panic!("{capability} renders a discussion contract rule"))
    }

    /// Renders the contour delta rule of `module` in the declared wire contour.
    fn delta_in(module: &dyn CapabilityModule, protocol: EditProtocolKind) -> ContractRule {
        let capability = module.capability();
        module
            .render_delta_rule(&contour(protocol))
            .unwrap_or_else(|| panic!("{capability} renders a delta rule"))
    }

    /// Renders the contour delta rule of `module` for a plan discussion.
    fn discussion_delta_in(
        module: &dyn CapabilityModule,
        protocol: EditProtocolKind,
    ) -> ContractRule {
        let capability = module.capability();
        module
            .render_delta_rule(&discussion_contour(protocol))
            .unwrap_or_else(|| panic!("{capability} renders a discussion delta rule"))
    }

    #[test]
    fn every_module_core_rule_is_identical_across_dialogue_intents() {
        use crate::capabilities::registry::CapabilityRegistry;

        let registry = CapabilityRegistry::global();
        for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
            for capability in LlmCapability::ALL {
                let module = match registry.module(*capability) {
                    Some(module) => module,
                    None => panic!("missing module for {capability}"),
                };
                let repl = module.render_contract_rule(&contour(protocol));
                let discussion = module.render_contract_rule(&discussion_contour(protocol));
                match (repl, discussion) {
                    (None, None) => {}
                    (Some(repl), Some(discussion)) => {
                        assert_eq!(
                            repl.title, discussion.title,
                            "{capability} titles its core rule differently in {protocol}"
                        );
                        assert_eq!(
                            repl.body, discussion.body,
                            "{capability} conditions its core rule on the intent in {protocol}"
                        );
                    }
                    (repl, discussion) => panic!(
                        "{capability} renders a core rule in one dialogue intent only: {:?} vs {:?}",
                        repl.map(|rule| rule.title),
                        discussion.map(|rule| rule.title)
                    ),
                }
            }
        }
    }

    #[test]
    fn the_plan_action_semantics_travel_in_the_delta_rule() {
        let module = PlanCreationModule;

        let develop = delta_in(&module, EditProtocolKind::Xml);
        assert_eq!(develop.title, "Local Engineering Plan Actions");
        assert!(develop.body.contains("ONLY propose new plans"));
        assert!(develop.body.contains("Reins Invariant"));
        assert!(develop.body.contains("Planning Objective: Plan synthesis"));
        assert!(!develop.body.contains("Conceptual Foundation Invariant"));
        assert!(!develop.body.contains("Structured Plan XML Format"));

        let discussion = discussion_delta_in(&module, EditProtocolKind::Xml);
        assert_eq!(discussion.title, "Local Engineering Plan Actions");
        assert!(discussion.body.contains("<plan action=\"save\" ...>"));
        assert!(discussion.body.contains("<plan action=\"update\" ...>"));
        assert!(discussion.body.contains("<plan action=\"delete\" ...>"));
        assert!(discussion.body.contains("ordinary transactional path"));
        assert!(discussion
            .body
            .contains("Planning Objective: Collaborative"));

        let structured_develop = delta_in(&module, EditProtocolKind::Structured);
        assert!(structured_develop.body.contains("ONLY propose new plans"));
        assert!(!structured_develop.body.contains("<plan"));
        assert_no_operational_tags(&structured_develop);

        let structured_discussion = discussion_delta_in(&module, EditProtocolKind::Structured);
        assert!(structured_discussion.body.contains("action: \"update\""));
        assert!(structured_discussion
            .body
            .contains("ordinary transactional path"));
        assert_no_operational_tags(&structured_discussion);
    }

    #[test]
    fn the_context_surface_rule_renders_the_vocabulary_of_the_active_policy() {
        let semantic = context_surface_rule(&contour(EditProtocolKind::Xml));
        assert_eq!(semantic.title, "Context Surface");
        assert!(semantic.body.contains("one `<context>` block"));
        assert!(semantic
            .body
            .contains("`<file path=\"...\" access=\"...\">`"));
        assert!(semantic.body.contains("access=\"read_only\""));
        assert!(semantic.body.contains("access=\"editable\""));
        assert!(!semantic.body.contains("<context_manifest>"));
        assert!(semantic.body.ends_with(FILE_ELEMENT_CONTRACT));

        let chronological_ctx =
            contour(EditProtocolKind::Xml).with_prompt_layout(PromptLayoutKind::Chronological);
        let chronological = context_surface_rule(&chronological_ctx);
        assert_eq!(chronological.title, "Context Surface");
        assert!(chronological.body.contains("transcript"));
        assert!(chronological.body.contains("<context_manifest>"));
        assert!(!chronological.body.contains("`<context>` block"));
        assert!(chronological.body.ends_with(FILE_ELEMENT_CONTRACT));

        // The two vocabularies never mix, and the shared payload description is
        // identical in both: only the container that delivers the files differs.
        assert_ne!(semantic.body, chronological.body);
    }

    /// Operational tag prefixes a structured contract must never advertise.
    const OPERATIONAL_TAG_PREFIXES: &[&str] = &[
        "<context_request",
        "<repo_search",
        "<repo_dirs",
        "<repo_map",
        "<dir_list",
        "<doc_request",
        "<verify",
        "<plan",
        "<concept",
        "<item",
        "<description",
        "<details",
        "<scratchpad",
        "<success",
        "<error",
        "<blocked",
        "<abort",
        "<user_language",
        "<custom_tool",
        "<edits",
        "<edit",
        "<create",
        "<overwrite",
        "<delete",
        "<move",
        "<search",
        "<replace",
    ];

    /// Asserts that a structured contract names no operational tag.
    fn assert_no_operational_tags(rule: &ContractRule) {
        for prefix in OPERATIONAL_TAG_PREFIXES {
            assert!(
                !rule.body.contains(prefix),
                "rule '{}' advertises {prefix} in the structured contour: {}",
                rule.title,
                rule.body
            );
        }
    }

    #[test]
    fn the_discovery_rule_is_dualized_by_the_wire_format() {
        let module = DiscoveryModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Discovery First");
        assert!(xml.body.contains("<context_request path="));
        assert!(xml.body.contains("<repo_search path="));
        assert!(xml.body.contains("<repo_dirs />"));
        assert!(xml.body.contains("<dir_list path="));
        // Without an eager symbol map the on-demand map request is advertised.
        assert!(xml.body.contains("<repo_map path="));
        assert!(xml
            .body
            .contains("verbatim at the very end of the response"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Discovery First");
        for field in [
            "`repo_dirs`",
            "`dir_lists`",
            "`repo_searches`",
            "`repo_maps`",
            "`context_requests`",
        ] {
            assert!(structured.body.contains(field), "missing {field}");
        }
        assert!(structured
            .body
            .contains("A populated field performs the request"));
        assert!(structured
            .body
            .contains("never deferred to a later response"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_discovery_rule_follows_the_eager_symbol_map_in_both_contours() {
        let module = DiscoveryModule;
        for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
            let render = PromptRenderContext::new(&[], None).with_eager_repo_map(true);
            let ctx = ContractContext::new(TurnIntent::InteractiveRepl, protocol, render);
            let rule = module
                .render_contract_rule(&ctx)
                .expect("the discovery rule is always rendered");
            assert!(
                !rule.body.contains("repo_map"),
                "the eager outline suppresses the on-demand map request in {protocol}: {}",
                rule.body
            );
            assert!(rule.body.contains("repo_dirs"));
        }
    }

    #[test]
    fn the_documentation_rule_is_dualized_by_the_wire_format() {
        let module = DocRequestModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Self-Knowledge");
        assert!(xml.body.contains("<doc_request"));
        assert!(xml.body.contains("topic=\""));
        assert!(xml.body.contains("<system_documentation>"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Self-Knowledge");
        assert!(structured.body.contains("`doc_requests` field"));
        assert!(structured.body.contains("`config`"));
        // The feedback block is prompt-side, so it stays in both contours.
        assert!(structured.body.contains("<system_documentation>"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_verification_rule_is_dualized_by_the_wire_format() {
        let module = ToolchainVerifyModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Code Verification");
        assert!(xml.body.contains("<verify target="));
        assert!(xml.body.contains("on_success=\"silent\""));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Code Verification");
        assert!(structured.body.contains("`verify` field"));
        assert!(structured.body.contains("'silent'"));
        assert!(structured
            .body
            .contains("explicitly asks to run verification"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_plan_creation_rule_is_dualized_by_the_wire_format() {
        let module = PlanCreationModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Local Engineering Plans");
        assert!(xml.body.contains("<plan action=\"save\""));
        assert!(xml.body.contains("<concept>"));
        assert!(xml.body.contains("Structured Plan XML Format:"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Local Engineering Plans");
        assert!(structured.body.contains("`plan_update`"));
        assert!(structured.body.contains("`concept`"));
        assert!(structured.body.contains("`items`"));
        assert!(structured.body.contains("`files`"));
        assert!(structured.body.contains("`readonly_files`"));
        // Every invariant survives in the structured formulation of the core.
        for invariant in [
            "Conceptual Foundation Invariant",
            "File Attributes Contract",
            "Leaf-Only Files Invariant",
            "Step Formulation Contract",
            "Plan Creation Invariant",
            "Language Consistency",
        ] {
            assert!(structured.body.contains(invariant), "missing {invariant}");
        }
        // The regime-specific action semantics live in the delta hook instead of
        // conditioning the shared core on the declared intent.
        assert!(!structured.body.contains("ONLY propose new plans"));
        assert!(!structured.body.contains("Planning Objective"));
        assert!(!structured.body.contains("Structured Plan XML Format"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_discussion_plan_rule_follows_the_declared_contour() {
        let module = PlanCreationModule;

        // Both contours authorize the same plan operations, and they declare
        // them in the delta hook while the core rule states the invariants.
        let structured = discussion_rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Local Engineering Plans");
        assert!(structured.body.contains("`plan_update`"));
        assert!(!structured.body.contains("<plan"));

        let structured_delta = discussion_delta_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured_delta.title, "Local Engineering Plan Actions");
        assert!(structured_delta.body.contains("action: \"update\""));
        assert!(structured_delta
            .body
            .contains("ordinary transactional path"));

        let xml = discussion_rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Local Engineering Plans");
        assert!(!xml.body.contains("<plan action=\"update\""));

        let xml_delta = discussion_delta_in(&module, EditProtocolKind::Xml);
        assert!(xml_delta.body.contains("<plan action=\"update\" ...>"));
        assert!(xml_delta.body.contains("<plan action=\"delete\" ...>"));
        assert!(xml_delta.body.contains("ordinary transactional path"));
        assert!(!xml_delta.body.contains("`plan_update`"));
    }

    #[test]
    fn the_scratchpad_rule_is_dualized_by_the_wire_format() {
        let module = ScratchpadModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Turn Scratchpad (Working Memory)");
        assert!(xml.body.contains("<scratchpad>...</scratchpad>"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Turn Scratchpad (Working Memory)");
        assert!(structured.body.contains("`scratchpad` field"));
        assert!(structured.body.contains("never enter persisted history"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_outcome_rule_is_dualized_by_the_wire_format() {
        let module = OutcomeModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Terminal Outcome");
        assert!(xml.body.contains("<success summary="));
        assert!(xml.body.contains("<error message="));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Terminal Outcome");
        assert!(structured.body.contains("`outcome` field"));
        assert!(structured.body.contains("'success'"));
        assert!(structured.body.contains("`summary`"));
        assert!(structured.body.contains("'error'"));
        assert!(structured.body.contains("`message`"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_outcome_contract_requires_a_payload_in_both_contours() {
        let module = OutcomeModule;

        // The XML contour states that a terminal tag carries its own payload and
        // that a status without one is a protocol defect rather than a result.
        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert!(xml.body.contains("carries its own payload"));
        assert!(xml.body.contains("protocol defect"));
        assert!(xml.body.contains("non-empty summary"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert!(structured.body.contains("carries its own payload"));
        assert!(structured.body.contains("protocol defect"));
        assert!(structured.body.contains("non-empty `summary`"));
        assert_no_operational_tags(&structured);

        // The protocol reference formulates the same obligation, so the rule and
        // the schema cannot describe different contracts.
        let render = PromptRenderContext::new(&[], None);
        let instruction = module
            .render_structured_instruction(&render)
            .expect("the outcome publishes a structured instruction");
        assert!(instruction.contains("carries its own payload"));
        assert!(instruction.contains("protocol defect"));

        // The composed schema carries the contract of the property it declares,
        // so a structured turn reads it from the schema of its own turn.
        let schema = crate::capabilities::registry::CapabilityRegistry::global()
            .compose_structured_schema(
                &tauqe_protocol::CapabilitySet::interactive_repl(),
                PatchStrategy::SearchReplace,
            );
        let description = schema["properties"]["outcome"]["properties"]["summary"]["description"]
            .as_str()
            .expect("the composed schema describes the summary");
        assert!(description.contains("Non-empty"), "{description}");
    }

    #[test]
    fn the_blocked_rule_is_dualized_by_the_wire_format() {
        let module = BlockedModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Semantic Blocker");
        assert!(xml.body.contains("<blocked reason="));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Semantic Blocker");
        assert!(structured.body.contains("`blocked` field"));
        assert!(structured.body.contains("`is_blocked`"));
        assert!(structured.body.contains("`reason`"));
        assert!(structured.body.contains("`details`"));
        assert!(structured
            .body
            .contains("instead of guessing or silently failing"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_abort_rule_is_dualized_by_the_wire_format() {
        let module = AbortModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Unconditional Abort");
        assert!(xml.body.contains("<abort reason="));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Unconditional Abort");
        assert!(structured.body.contains("`abort` field"));
        assert!(structured.body.contains("`is_aborted`"));
        assert!(structured.body.contains("`reason`"));
        assert!(structured.body.contains("`recommendation`"));
        // The prohibition on surrogates survives the dualization.
        assert!(structured.body.contains("not a blocker"));
        assert!(structured.body.contains("stub"));
        assert!(structured.body.contains("surrogate"));
        assert!(structured.body.contains("placeholder"));
        assert!(structured
            .body
            .contains("rolls back the incomplete change set"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_language_rule_is_dualized_by_the_wire_format() {
        let module = UserLanguageModule;

        let xml = rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Language Consistency");
        assert!(xml
            .body
            .contains("<user_language>language_name</user_language>"));
        assert!(xml.body.contains("<user_language>Russian</user_language>"));

        let structured = rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Language Consistency");
        assert!(structured.body.contains("`user_language` field"));
        assert!(structured.body.contains("\"Russian\""));
        assert!(!structured.body.contains("<user_language"));
        assert_no_operational_tags(&structured);
    }

    #[test]
    fn the_language_rule_follows_the_declared_contour_in_a_discussion() {
        let module = UserLanguageModule;

        let structured = discussion_rule_in(&module, EditProtocolKind::Structured);
        assert_eq!(structured.title, "Language Consistency");
        assert!(structured.body.contains("`user_language` field"));
        assert!(!structured.body.contains("<user_language"));

        let xml = discussion_rule_in(&module, EditProtocolKind::Xml);
        assert_eq!(xml.title, "Language Consistency");
        assert!(xml
            .body
            .contains("<user_language>language_name</user_language>"));
        assert!(!xml.body.contains("`user_language` field"));
    }

    /// Asserts that rendered structured text names no operational tag.
    fn assert_no_operational_tag_in(text: &str, origin: &str) {
        for prefix in OPERATIONAL_TAG_PREFIXES {
            assert!(
                !text.contains(prefix),
                "{origin} advertises {prefix} in the structured contour: {text}"
            );
        }
    }

    #[test]
    fn no_structured_contract_advertises_an_operational_tag() {
        use crate::capabilities::registry::CapabilityRegistry;

        let registry = CapabilityRegistry::global();
        for capability in LlmCapability::ALL {
            let module = match registry.module(*capability) {
                Some(module) => module,
                None => panic!("missing module for {capability}"),
            };
            // Both the core rule and the delta hook reach a structured prompt,
            // so neither may advertise an operational control tag.
            if let Some(rule) = module.render_contract_rule(&contour(EditProtocolKind::Structured))
            {
                assert_no_operational_tags(&rule);
            }
            if let Some(rule) = module.render_delta_rule(&contour(EditProtocolKind::Structured)) {
                assert_no_operational_tags(&rule);
            }
        }
    }

    #[test]
    fn no_structured_instruction_advertises_an_operational_tag() {
        use crate::capabilities::registry::CapabilityRegistry;

        let registry = CapabilityRegistry::global();
        let editable: [String; 0] = [];
        let render = PromptRenderContext::new(&editable, None);

        // The protocol-instruction block is the second surface a structured
        // turn advertises, so an audited rule alone would leave it unchecked.
        let mut checked = 0;
        for capability in LlmCapability::ALL {
            let module = match registry.module(*capability) {
                Some(module) => module,
                None => panic!("missing module for {capability}"),
            };
            let Some(instruction) = module.render_structured_instruction(&render) else {
                continue;
            };
            checked += 1;
            assert_no_operational_tag_in(&instruction, capability.as_str());
        }
        assert!(
            checked > 0,
            "a structured turn renders a protocol instruction for its mapped capabilities"
        );
    }

    #[test]
    fn the_structured_contour_renders_no_control_tag_anywhere() {
        use crate::capabilities::registry::CapabilityRegistry;
        use tauqe_protocol::CapabilitySet;

        let registry = CapabilityRegistry::global();
        let editable: [String; 0] = [];
        let set = CapabilitySet::interactive_repl();

        // The core contract of a structured turn spells every invariant with
        // the fields of the strict response schema, so the rule that enumerates
        // control tags is never rendered there.
        let render = PromptRenderContext::new(&editable, None);
        let ctx = ContractContext::new(
            TurnIntent::InteractiveRepl,
            EditProtocolKind::Structured,
            render,
        );
        let contract = registry.compose_core_contract_rules(&set, &ctx);
        let delta = registry.compose_delta_contract_rules(&set, &ctx);
        assert_no_operational_tag_in(&contract, "the structured core contract");
        assert_no_operational_tag_in(&delta, "the structured delta contract");
        assert!(!contract.contains("Operational Protocol Tags"));
        assert_no_operational_tag_in(&contract, "the structured core contract");
        assert!(!contract.contains("Operational Protocol Tags"));

        // The protocol instruction block describes the same surface and names a
        // schema field for every capability it advertises.
        let render = PromptRenderContext::new(&editable, None);
        let instructions = registry.compose_structured_protocol_instructions(&set, &render);
        assert_no_operational_tag_in(&instructions, "the structured protocol instructions");
        for field in ["`message`", "`changes`", "`doc_requests`", "`outcome`"] {
            assert!(instructions.contains(field), "missing {field}");
        }
    }
    /// Builds a `[project]` declaration carrying the declared protected patterns.
    fn project_with_protected(patterns: &[&str]) -> ProjectConfig {
        let mut project = ProjectConfig::default();
        project.boundaries.protected = patterns.iter().map(|p| (*p).to_string()).collect();
        project
    }

    /// Renders the edit rule of one contour for an interactive turn.
    fn edit_rule(protocol: EditProtocolKind, project: Option<&ProjectConfig>) -> ContractRule {
        let render = PromptRenderContext::new(&[], None).with_project(project);
        let ctx = ContractContext::new(TurnIntent::InteractiveRepl, protocol, render);
        FileEditsModule
            .render_contract_rule(&ctx)
            .expect("the edit module always renders a contract rule")
    }

    /// Renders the edit rule of one contour for a plan discussion.
    fn discussion_edit_rule(
        protocol: EditProtocolKind,
        project: Option<&ProjectConfig>,
    ) -> ContractRule {
        let render = PromptRenderContext::new(&[], None).with_project(project);
        let ctx = ContractContext::new(TurnIntent::PlanDiscussion, protocol, render);
        FileEditsModule
            .render_contract_rule(&ctx)
            .expect("the edit module always renders a contract rule")
    }

    #[test]
    fn the_edit_rule_lists_the_protected_paths_of_the_project() {
        let project = project_with_protected(&["Justfile", "docs/specs/**"]);
        let rule = edit_rule(EditProtocolKind::Xml, Some(&project));

        assert_eq!(rule.title, "Minimal Coherent Change");
        assert!(rule.body.contains("`Justfile`"));
        assert!(rule.body.contains("`docs/specs/**`"));
        assert!(rule.body.contains("categorically forbidden"));
        assert!(rule.body.contains("`read_only`"));
    }

    #[test]
    fn the_edit_rule_names_no_protected_path_without_a_declaration() {
        // An absent section and a section that protects nothing both publish no
        // prohibition, so the prompt never advertises a boundary nobody declared.
        let bare = edit_rule(EditProtocolKind::Xml, None);
        assert!(!bare.body.contains("protected"));
        assert!(!bare.body.contains("categorically forbidden"));

        let empty = project_with_protected(&[]);
        let undeclared = edit_rule(EditProtocolKind::Xml, Some(&empty));
        assert_eq!(undeclared.body, bare.body);
    }

    #[test]
    fn the_sandbox_sentence_follows_the_edit_surface() {
        use crate::capabilities::registry::CapabilityRegistry;
        use tauqe_protocol::CapabilitySet;

        let registry = CapabilityRegistry::global();
        let render = PromptRenderContext::new(&[], None);
        let full = registry.compose_core_contract_rules(
            &CapabilitySet::interactive_repl(),
            &ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render),
        );
        assert!(full.contains(MOCKUPS_DIR));
        assert!(full.contains("sandbox isolated from Git"));

        // A surface without the edit capability publishes no boundary at all, so
        // the sandbox never appears where no file can be written.
        let render = PromptRenderContext::new(&[], None);
        let without_edits =
            CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::DocRequest]);
        let narrowed = registry.compose_core_contract_rules(
            &without_edits,
            &ContractContext::new(TurnIntent::InteractiveRepl, EditProtocolKind::Xml, render),
        );
        assert!(!narrowed.contains(MOCKUPS_DIR));
        assert!(!narrowed.contains("Minimal Coherent Change"));
    }

    #[test]
    fn the_boundary_rule_is_identical_across_dialogue_intents() {
        let project = project_with_protected(&["Justfile"]);
        let repl = edit_rule(EditProtocolKind::Xml, Some(&project));
        let discussion = discussion_edit_rule(EditProtocolKind::Xml, Some(&project));

        // One module states one rule: the sandbox and the declared boundaries are
        // project facts rather than regime decisions, so the head stays identical.
        assert_eq!(repl.title, discussion.title);
        assert_eq!(repl.body, discussion.body);
        assert!(repl.body.contains("`Justfile`"));
        assert!(repl.body.contains(MOCKUPS_DIR));
    }

    #[test]
    fn the_boundary_rule_keeps_the_contour_dictionary_clean() {
        let project = project_with_protected(&["Justfile", "docs/specs/**"]);

        for protocol in [EditProtocolKind::Xml, EditProtocolKind::Structured] {
            let rule = edit_rule(protocol, Some(&project));
            // The paths and the sandbox are prompt-side facts, so both contours
            // describe the same boundaries without naming a control tag.
            assert!(rule.body.contains("`Justfile`"));
            assert!(rule.body.contains(MOCKUPS_DIR));
            assert_no_operational_tags(&rule);
        }
    }
}
