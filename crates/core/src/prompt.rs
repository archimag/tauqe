use crate::context::ContextAccess;
use crate::git::state::RepositoryState;
pub mod json;
pub(crate) mod layout;
pub mod outer_docs;
pub mod protocol;
pub mod refine;

use std::borrow::Cow;
use std::collections::HashMap;

use crate::capabilities::{compose_contract_rules, ContractContext, PromptRenderContext};
use crate::context::ContextFileContent;
use crate::edits::anchor::{encode_anchor, ANCHOR_LEN};
use crate::edits::stream::json::decode::first_json_object_str;
use crate::history::HistorySnapshot;
use crate::llm::CapabilityTree;
use crate::plan::Plan;
use crate::prompt::layout::assemble_chronological_messages;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{ChatMessage, JsonSchemaDefinition, ResponseFormat};
use crate::workflow::EditProtocolKind;
use crate::workflow::PatchStrategy;

/// Typed kind of the contour that drives a turn.
///
/// The contour is declared by the loop that owns the turn rather than inferred from
/// the capability surface: a dialogue, an isolated step, and an Architect synthesis
/// share leaves but never share the rules of their regime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContourKind {
    /// An interactive dialogue: the REPL, a plan discussion, or a review discussion.
    #[default]
    Dialogue,
    /// One isolated plan or review step executed inside the working tree.
    Step,
    /// An Architect horizon synthesis that projects a plan instead of patching code.
    Architect,
}

/// Introductory brief of a turn: the contour that drives it and the text it opens with.
///
/// The brief belongs to the loop that drives the turn, so the assembly renders the
/// introduction of the contour instead of selecting one by a flag, and the rules of the
/// regime follow the same typed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBrief {
    pub(crate) contour: ContourKind,
    /// Opening text the turn renders ahead of the Core System Contract.
    intro: String,
}

impl TurnBrief {
    pub(crate) fn dialogue() -> Self {
        Self {
            contour: ContourKind::Dialogue,
            intro: DIALOGUE_INTRO.to_string(),
        }
    }

    /// Builds the brief of one isolated step.
    pub fn step() -> Self {
        Self {
            contour: ContourKind::Step,
            intro: STEP_INTRO.to_string(),
        }
    }

    /// Builds the brief of an Architect horizon synthesis.
    ///
    /// A synthesis reasons about the project rather than about the working tree, so it
    /// opens with the introduction of its own planning role.
    pub(crate) fn architect() -> Self {
        Self {
            contour: ContourKind::Architect,
            intro: ARCHITECT_INTRO.to_string(),
        }
    }
}

impl Default for TurnBrief {
    fn default() -> Self {
        Self::dialogue()
    }
}

/// Opening text of an interactive dialogue.
const DIALOGUE_INTRO: &str = concat!(
    "You are Tauqe AI, a proactive engineering assistant operating inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n",
    "The developer states the task; the harness holds the work in typed contracts, verifies every change, and closes the turn with a Git transaction. A turn runs in autonomous rounds: you may explore the repository across discovery rounds before concluding the turn with your final answer, code edits, or plan. Your role in this dialogue is to act on direct requests, answer open questions in writing, and keep the developer in control of the objective.\n",
    "Environment behavior is declared in `tauqe.toml` at the repository root: model roles, toolchain verification phases, context and repo-map budgets, pinned files, and workflow policy all come from that file. Treat it as the authoritative project configuration surface, and never infer a documented parameter, budget, or project convention from partial source memory.\n\n",
);

/// Opening text of one isolated step.
const STEP_INTRO: &str = concat!(
    "You are Tauqe Coder, the execution engine of one isolated plan step inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n",
    "The developer states the task; the harness holds the work in typed contracts, verifies every change, and closes the turn with a Git transaction. A turn runs in autonomous rounds: you may explore the repository across discovery rounds before implementing edits and verifying them in the concluding round. Your role is to implement the assigned step exactly as designed, without re-architecting it, and to close with a concise summary of what changed and why.\n",
    "Environment behavior is declared in `tauqe.toml` at the repository root: model roles, toolchain verification phases, context and repo-map budgets, pinned files, and workflow policy all come from that file. Treat it as the authoritative project configuration surface, and never infer a documented parameter, budget, or project convention from partial source memory.\n\n",
);

/// Opening text of an Architect horizon synthesis.
const ARCHITECT_INTRO: &str = concat!(
    "You are Tauqe Architect, the planning role inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n",
    "The developer states the task; the harness holds the work in typed contracts, verifies every change, and closes the turn with a Git transaction. Your role is to synthesize the concept tree and the architectural invariants that the implementation steps must satisfy.\n",
    "Environment behavior is declared in `tauqe.toml` at the repository root: model roles, toolchain verification phases, context and repo-map budgets, pinned files, and workflow policy all come from that file. Treat it as the authoritative project configuration surface, and never infer a documented parameter, budget, or project convention from partial source memory.\n\n",
);

pub fn build_squash_commit_prompt(
    commits: &[crate::git::CommitSummary],
    cumulative_diff: &str,
    pinned_files: &[ContextFileContent],
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an expert developer assistant. Your task is to write a single high-quality commit message for squashing a series of commits into one.\n\n");

    if !pinned_files.is_empty() {
        prompt.push_str("## Project Guidelines & Context\n");
        prompt.push_str(
            "Follow any commit message conventions, scopes, and guidelines specified below:\n",
        );
        for file in pinned_files {
            prompt.push_str(&format!(
                "<file path=\"{}\">\n{}\n</file>\n",
                file.path, file.content
            ));
        }
        prompt.push('\n');
    }

    prompt.push_str("## Commits to Squash\n");
    for c in commits {
        prompt.push_str(&format!(
            "- {} {} ({}, {})\n",
            c.hash, c.subject, c.author, c.date
        ));
    }
    prompt.push('\n');

    prompt.push_str("## Cumulative Diff\n");
    prompt.push_str("```diff\n");
    const MAX_DIFF_CHARS: usize = 30_000;
    if cumulative_diff.len() > MAX_DIFF_CHARS {
        let truncate_index = cumulative_diff
            .char_indices()
            .map(|(idx, _)| idx)
            .take_while(|&idx| idx <= MAX_DIFF_CHARS)
            .last()
            .unwrap_or(0);
        prompt.push_str(&cumulative_diff[..truncate_index]);
        prompt.push_str("\n... [diff truncated for length] ...\n");
    } else {
        prompt.push_str(cumulative_diff);
    }
    if !cumulative_diff.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n\n");

    prompt.push_str("## Instructions\n");
    prompt.push_str("1. Write a concise commit message summarizing the squashed commits and cumulative changes according to project guidelines.\n");
    prompt.push_str("2. Respond with a single JSON object matching the enforced schema, placing the commit message in the `commit_message` field. Do NOT include markdown code block fences, explanations, tags, or conversational fluff.\n");

    prompt
}

/// JSON Schema for the history compaction summary output.
///
/// History compaction never produces code changes, so it is a Structured Output
/// operation (see `crates/core/Conventions.md`, section 1.2). The Scribe returns
/// a machine-readable summary object instead of free-form prose recovered by
/// heuristics.
fn history_compaction_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Concise, structured summary capturing completed tasks, key code changes, and the context preserved from the compacted history"
            }
        },
        "required": ["summary"],
        "additionalProperties": false
    })
}

pub(crate) fn history_compaction_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "history_compaction".to_string(),
            description: Some(
                "Concise structured summary of compacted session history".to_string(),
            ),
            schema: Cow::Owned(history_compaction_response_schema()),
            strict: Some(true),
        },
    }
}

pub(crate) fn parse_compaction_summary(raw: &str) -> Option<String> {
    extract_json_string_field(raw, "summary")
}

fn squash_commit_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "commit_message": {
                "type": "string",
                "description": "Final squashed commit message text, following project commit-message conventions, without markdown code fences or surrounding prose"
            }
        },
        "required": ["commit_message"],
        "additionalProperties": false
    })
}

/// Builds the provider [`ResponseFormat`] that enforces the squash commit schema.
pub fn squash_commit_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "squash_commit".to_string(),
            description: Some("Squashed commit message synthesis".to_string()),
            schema: Cow::Owned(squash_commit_response_schema()),
            strict: Some(true),
        },
    }
}

/// Extracts the commit message from the Scribe's structured squash output.
pub fn parse_squash_commit_message(raw: &str) -> Option<String> {
    extract_json_string_field(raw, "commit_message")
}

/// Extracts a single non-empty string field from a model's structured JSON
/// output, tolerating surrounding markdown code fences or wrapper prose.
///
/// The byte scan only inspects ASCII delimiters, so multi-byte UTF-8 content is
/// never sliced across a scalar boundary.
fn extract_json_string_field(raw: &str, field: &str) -> Option<String> {
    let value = parse_first_json_object(raw)?;
    let text = value.get(field)?.as_str()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parses the first JSON object found in a model response.
fn parse_first_json_object(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if value.is_object() {
            return Some(value);
        }
    }
    let candidate = first_json_object_str(raw)?;
    let value = serde_json::from_str::<serde_json::Value>(candidate).ok()?;
    value.is_object().then_some(value)
}

/// The shared token heuristic is a deterministic substrate primitive, so the
/// definition lives in `tauqe-tools` and this re-export keeps
/// `crate::prompt::estimate_tokens` resolvable for every prompt and budget
/// computation of this crate.
pub use tauqe_tools::tokens::{estimate_tokens, estimate_tokens_from_bytes};

/// Returns the declarative `access` attribute value of a context file.
///
/// The rendered context block annotates every file with this attribute instead
/// of grouping files under separate read-only and editable sections, so
/// promoting a file to editable rewrites only its own attribute and never moves
/// it past its neighbors.
fn access_attribute(access: ContextAccess) -> &'static str {
    match access {
        ContextAccess::ReadOnly => "read_only",
        ContextAccess::Editable => "editable",
    }
}

/// Renders the opening tag of one `<file>` element of the context block.
///
/// The function is the single source of the element markup: the context block
/// renders the tag through it and [`context_element_overhead_tokens`] measures
/// the very same bytes, so the price of a wrapper can never drift from the
/// markup the model reads.
fn context_element_open_tag(path: &str, access: ContextAccess) -> String {
    format!(
        "    <file path=\"{path}\" access=\"{}\">\n",
        access_attribute(access)
    )
}

/// Renders the closing tag of one `<file>` element of the context block.
fn context_element_close_tag() -> &'static str {
    "    </file>\n"
}

/// Estimates the token cost the `<file>` wrapper adds to a context file.
///
/// A budget check prices a file as its content plus this wrapper, so a file
/// pays exactly for the markup that surrounds it: the header carries the path,
/// so a deep path costs more than a shallow one, and a change to the element
/// markup changes the estimate with it. Both terms come from the builder that
/// renders the `<context>` block, so the measured wrapper and the rendered
/// wrapper cannot diverge.
pub fn context_element_overhead_tokens(path: &str, access: ContextAccess) -> usize {
    let open = context_element_open_tag(path, access);
    estimate_tokens(&open) + estimate_tokens(context_element_close_tag())
}

/// Renders one `<file>` element of the context block.
///
/// Under [`PatchStrategy::LineAnchor`] the element carries the opaque anchor of
/// every line, so an edit can name a line range instead of echoing the text it
/// replaces. The anchors are seeded by the file's own path, so a token belongs
/// to exactly one file and cannot be reused across files. Both access layers
/// are rendered the same way: a read-only file is as addressable as an editable
/// one, it merely cannot be edited yet.
fn render_context_element(
    file: &ContextFileContent,
    access: ContextAccess,
    patch_strategy: PatchStrategy,
) -> String {
    let content = match patch_strategy {
        PatchStrategy::SearchReplace => file.content.clone(),
        // A file outside the sixteen-bit domain of the codec cannot be
        // addressed by anchors at all, so it stays raw rather than carrying
        // prefixes that would name other lines.
        PatchStrategy::LineAnchor => {
            anchor_lines(&file.path, &file.content).unwrap_or_else(|| file.content.clone())
        }
    };
    let open = context_element_open_tag(&file.path, access);
    let close = context_element_close_tag();
    let mut out = String::with_capacity(open.len() + content.len() + close.len() + 1);
    out.push_str(&open);
    out.push_str(&content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(close);
    out
}

/// Prefixes every line of a file with the opaque anchor naming it.
///
/// The anchor encodes the zero-based index of the line, seeded by the file's
/// repository path, which is the domain the codec permutes: the staging engine
/// decodes a range of anchors of that same path back into the exact slice of
/// lines it replaces. The encoding is local to a single line of one path, so
/// the anchors of an untouched file — and of the untouched leading lines of an
/// edited one — stay identical between the rounds of a turn.
///
/// Returns `None` when the file leaves the sixteen-bit domain of the codec,
/// because such a file cannot be addressed by anchors at all.
fn anchor_lines(path: &str, content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > usize::from(u16::MAX) + 1 {
        return None;
    }
    let mut out = String::with_capacity(content.len() + lines.len() * (ANCHOR_LEN + 3));
    for (index, line) in lines.iter().enumerate() {
        let line_idx = u16::try_from(index).ok()?;
        out.push_str(&encode_anchor(path, line_idx));
        out.push_str(" | ");
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// One rendered `<file>` element kept for reuse across the rounds of a turn.
struct CachedContextElement {
    access: ContextAccess,
    content: String,
    element: String,
}

/// Per-turn memo of rendered context elements, keyed by repository path.
///
/// The memo belongs to the turn rather than to a single prompt assembly, so a
/// file rendered in an earlier round is handed back verbatim and only the files
/// a later round actually changed or appended are encoded again. That keeps the
/// context block append-only and the provider's cached prompt prefix intact.
#[derive(Default)]
pub struct ContextBlockCache {
    elements: HashMap<String, CachedContextElement>,
}

impl ContextBlockCache {
    #[cfg(test)]
    fn cached_files(&self) -> usize {
        self.elements.len()
    }

    /// Returns the rendered element of a file, re-rendering it only when needed.
    ///
    /// A file whose effective access and content are unchanged is reproduced
    /// byte for byte; everything else is the only element the round encodes.
    /// The research journal renders the files a round delivered through this
    /// very method, so a delivered file carries the anchors, the `access`
    /// attribute, and the bytes the context block of the turn renders.
    pub(crate) fn element(
        &mut self,
        file: &ContextFileContent,
        access: ContextAccess,
        patch_strategy: PatchStrategy,
    ) -> &str {
        let reusable = self
            .elements
            .get(&file.path)
            .is_some_and(|cached| cached.access == access && cached.content == file.content);
        if !reusable {
            let element = render_context_element(file, access, patch_strategy);
            self.elements.insert(
                file.path.clone(),
                CachedContextElement {
                    access,
                    content: file.content.clone(),
                    element,
                },
            );
        }
        self.elements
            .get(&file.path)
            .map(|cached| cached.element.as_str())
            .unwrap_or_default()
    }
}

pub(crate) struct PromptAssembly {
    _repo_state: Option<RepositoryState>,
    _context_revision: u64,
    pub(crate) context_files: Vec<ContextFileContent>,
    _workflow: String,
    _edit_protocol: EditProtocolKind,
    history: Option<std::sync::Arc<HistorySnapshot>>,
    repo_map: Option<String>,
    repo_dirs: Option<String>,
    active_plan: Option<Plan>,
    pub(crate) turn_brief: Option<TurnBrief>,
    pub(crate) turn_scratchpad: Option<String>,
    max_scratchpad_chars: usize,
    patch_strategy: PatchStrategy,
    project: Option<crate::config::project::ProjectConfig>,
    outer_docs: Vec<crate::config::OuterDocEntry>,
}

impl PromptAssembly {
    pub(crate) fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
        workflow: impl Into<String>,
        edit_protocol: EditProtocolKind,
        history: Option<std::sync::Arc<HistorySnapshot>>,
        repo_map: Option<String>,
    ) -> Self {
        Self {
            _repo_state: repo_state,
            _context_revision: context_revision,
            context_files,
            _workflow: workflow.into(),
            _edit_protocol: edit_protocol,
            history,
            repo_map,
            repo_dirs: None,
            active_plan: None,
            turn_brief: None,
            turn_scratchpad: None,
            max_scratchpad_chars: crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS,
            patch_strategy: PatchStrategy::default(),
            project: None,
            outer_docs: Vec::new(),
        }
    }

    pub(crate) fn with_project(
        mut self,
        project: Option<crate::config::project::ProjectConfig>,
    ) -> Self {
        self.project = project;
        self
    }

    pub(crate) fn with_outer_docs(mut self, outer_docs: Vec<crate::config::OuterDocEntry>) -> Self {
        self.outer_docs = outer_docs;
        self
    }

    pub(crate) fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }

    pub(crate) fn with_repo_dirs(mut self, repo_dirs: Option<String>) -> Self {
        self.repo_dirs = repo_dirs;
        self
    }

    fn build_system_prompt(&self, protocol: &dyn PromptProtocol) -> String {
        let capabilities = self.effective_capability_expr(protocol);
        let effective_protocol = protocol
            .with_capability_expr(capabilities.clone())
            .with_project(self.project.clone());
        let mut prompt = String::new();
        let turn_brief = self.turn_brief.clone().unwrap_or_default();
        prompt.push_str(&turn_brief.intro);
        let has_eager_repo_map = self.has_eager_repo_map();
        let render_ctx = PromptRenderContext::new(&[], None)
            .with_eager_repo_map(has_eager_repo_map)
            .with_project(self.project.as_ref())
            .with_patch_strategy(self.patch_strategy);
        let contract_ctx = ContractContext::new(render_ctx).with_contour(turn_brief.contour);
        prompt.push_str("## Core System Contract\n");
        prompt.push_str(&compose_contract_rules(&capabilities, &contract_ctx));
        prompt.push('\n');

        let editable_paths: Vec<String> = self
            .context_files
            .iter()
            .filter(|f| f.access == ContextAccess::Editable)
            .map(|f| f.path.clone())
            .collect();
        prompt.push_str(&effective_protocol.system_instructions(
            &editable_paths,
            has_eager_repo_map,
            self.patch_strategy,
        ));

        if capabilities.contains(crate::llm::LlmCapability::OuterDocRequest) {
            if let Some(catalog) = self.format_outer_docs_catalog_block() {
                prompt.push('\n');
                prompt.push_str(&catalog);
            }
        }

        prompt
    }

    fn effective_capability_expr(&self, protocol: &dyn PromptProtocol) -> CapabilityTree {
        protocol.capability_expr()
    }

    fn context_file_access(&self, file: &ContextFileContent) -> ContextAccess {
        file.access
    }

    fn history_snapshot(&self) -> Option<&HistorySnapshot> {
        self.history
            .as_deref()
            .filter(|snapshot| !snapshot.is_empty())
    }

    fn has_eager_repo_map(&self) -> bool {
        self.repo_map
            .as_deref()
            .is_some_and(|map| !map.trim().is_empty())
    }

    fn format_repo_map_block(&self) -> Option<String> {
        self.repo_map.as_deref().and_then(|map| {
            let trimmed = map.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("<repo_map>\n{}\n</repo_map>", trimmed))
            }
        })
    }

    fn format_repo_dirs_block(&self) -> Option<String> {
        self.repo_dirs.as_deref().and_then(|dirs| {
            let trimmed = dirs.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("<repo_dirs>\n{}\n</repo_dirs>", trimmed))
            }
        })
    }

    fn format_repository_outline_block(&self) -> Option<String> {
        if let Some(map) = self.format_repo_map_block() {
            return Some(format!("## Repository Map\n{map}\n\n"));
        }
        self.format_repo_dirs_block()
            .map(|dirs| format!("## Repository Directories\n{dirs}\n\n"))
    }

    fn is_step_contour(&self) -> bool {
        self.turn_brief
            .as_ref()
            .is_some_and(|brief| brief.contour == ContourKind::Step)
    }

    fn format_active_plan_block(&self) -> Option<String> {
        if self.is_step_contour() {
            return None;
        }
        let plan = self.active_plan.as_ref()?;
        let block = crate::plan::format_active_plan_context(plan)?;
        Some(format!("## Active Task Plan Context\n{block}\n\n"))
    }

    fn format_scratchpad_section(&self) -> Option<String> {
        self.format_scratchpad_block()
            .map(|block| format!("## Turn Scratchpad (Working Memory)\n{block}\n\n"))
    }

    fn format_context_manifest_section(&self) -> Option<String> {
        if self.context_files.is_empty() {
            return None;
        }
        let mut out = String::from("## Project Context Manifest\n<context_manifest>\n");
        for file in &self.context_files {
            out.push_str(&format!(
                "    <file path=\"{}\" access=\"{}\" />\n",
                file.path,
                access_attribute(self.context_file_access(file))
            ));
        }
        out.push_str("</context_manifest>\n\n");
        Some(out)
    }

    fn format_outer_docs_catalog_block(&self) -> Option<String> {
        let mut catalog = String::new();
        for entry in &self.outer_docs {
            let Ok(doc) = crate::outer_docs::OuterDoc::from_entry(entry) else {
                continue;
            };
            catalog.push_str(&format!("- {} — {}", doc.id, doc.title));
            if let Some(description) = doc.description.as_deref() {
                catalog.push_str(&format!(": {description}"));
            }
            catalog.push('\n');
        }
        if catalog.is_empty() {
            return None;
        }
        Some(format!(
            "## External Documentation Catalog\nRequest a declared document by its identifier through the `outer_doc_requests` field: `id` addresses the root document exactly as its original text, and `id/relative.md` addresses a document below the directory that holds the root, and never climbs above it.\n{catalog}\n"
        ))
    }

    fn format_scratchpad_block(&self) -> Option<String> {
        self.turn_scratchpad.as_deref().and_then(|pad| {
            let trimmed = pad.trim();
            if trimmed.is_empty() {
                None
            } else {
                let text = if trimmed.chars().count() > self.max_scratchpad_chars {
                    let truncated: String =
                        trimmed.chars().take(self.max_scratchpad_chars).collect();
                    format!("{}\n... [scratchpad truncated for length] ...", truncated)
                } else {
                    trimmed.to_string()
                };
                Some(format!("<scratchpad>\n{}\n</scratchpad>", text))
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn assemble_chat_messages(
        &self,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
    ) -> Vec<ChatMessage> {
        self.assemble_chat_messages_with_context_cache(
            current_prompt,
            protocol,
            &mut ContextBlockCache::default(),
        )
    }

    pub(crate) fn assemble_chat_messages_with_context_cache(
        &self,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
        context_cache: &mut ContextBlockCache,
    ) -> Vec<ChatMessage> {
        assemble_chronological_messages(self, current_prompt, protocol, context_cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEntry;
    use crate::prompt::json::JsonPromptProtocol;

    /// Structured protocol carrying the shared dialogue surface.
    fn dialogue_protocol() -> Box<dyn PromptProtocol> {
        JsonPromptProtocol::edits()
            .with_capability_expr(crate::control::repl::repl_capability_expr(None))
    }

    /// Structured protocol carrying the capability surface of one isolated step.
    fn step_protocol() -> Box<dyn PromptProtocol> {
        JsonPromptProtocol::edits()
            .with_capability_expr(crate::control::turn::turn_capability_expr())
    }
    /// Lifts the operational branch envelope of a composed schema into one surface.
    ///
    /// The wire keeps the operational branches mutually exclusive, so a field of
    /// a branch is addressed through the envelope that owns it. A test that reads
    /// the property of a capability reads the schema after this lift, so it names
    /// the field rather than the branch that carries it.
    fn flattened_surface(schema: &serde_json::Value) -> serde_json::Value {
        let mut objects = vec![schema];
        if let Some(branches) = schema["properties"]["action"]["anyOf"].as_array() {
            objects.extend(branches.iter());
        }
        let mut properties = serde_json::Map::new();
        let mut required: Vec<serde_json::Value> = Vec::new();
        for object in objects {
            let Some(fields) = object["properties"].as_object() else {
                continue;
            };
            for (name, field) in fields {
                if name.as_str() == "action" || name.as_str() == "kind" {
                    continue;
                }
                if properties.contains_key(name) {
                    continue;
                }
                properties.insert(name.clone(), field.clone());
                required.push(serde_json::Value::String(name.clone()));
            }
        }
        let mut flat = serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": required,
            "additionalProperties": false
        });
        if let Some(definitions) = schema.get("$defs") {
            flat["$defs"] = definitions.clone();
        }
        flat
    }

    #[test]
    fn test_build_squash_commit_prompt() {
        let commits = vec![
            crate::git::CommitSummary {
                hash: "abc1234".to_string(),
                author: "Dev".to_string(),
                date: "2025-01-01".to_string(),
                subject: "part 1".to_string(),
            },
            crate::git::CommitSummary {
                hash: "def5678".to_string(),
                author: "Dev".to_string(),
                date: "2025-01-01".to_string(),
                subject: "part 2".to_string(),
            },
        ];
        let pinned = vec![ContextFileContent {
            path: "docs/Conventions.md".to_string(),
            access: ContextAccess::ReadOnly,
            content: "# Conventions\nUse Conventional Commits.\n".to_string(),
            insertion_seq: 0,
        }];

        let prompt = build_squash_commit_prompt(&commits, "+new_code", &pinned);
        assert!(prompt.contains("## Commits to Squash"));
        assert!(prompt.contains("abc1234 part 1"));
        assert!(prompt.contains("docs/Conventions.md"));
        assert!(prompt.contains("Use Conventional Commits."));
        assert!(prompt.contains("+new_code"));
        assert!(prompt.contains("Instructions"));
    }

    #[test]
    fn test_build_squash_commit_prompt_truncation_utf8_safety() {
        let commits = vec![];
        let pinned = vec![];
        // Multi-byte characters (Cyrillic chars are 2 bytes each)
        let multi_byte_text = "Тестовая строка для проверки обрезки по границе UTF-8. ".repeat(700);
        assert!(multi_byte_text.len() > 30_000);

        // Must not panic on truncation:
        let prompt = build_squash_commit_prompt(&commits, &multi_byte_text, &pinned);
        assert!(prompt.contains("diff truncated for length"));
    }

    #[test]
    fn test_prompt_assembly_carries_static_language_and_reasoning_rules() {
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Привет", &proto);
        assert!(!messages[0].content.contains("## Language Directive"));
        assert!(messages[0].content.contains("Response Language"));
        assert!(messages[0].content.contains("Reasoning in English"));
    }

    #[test]
    fn test_prompt_assembly() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
            insertion_seq: 0,
        }];
        let history = Some(std::sync::Arc::new(HistorySnapshot::new(vec![
            HistoryEntry::Summary {
                id: None,
                text: "Initial".to_string(),
            },
        ])));
        let repo_map = Some("crates/core/src/git.rs\n  pub fn get_repository_state()".to_string());
        let assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Structured,
            history,
            repo_map,
        );
        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Explain this code", &proto);

        // The head keeps the contract, the instructions, and the outline; the
        // interaction record and the file body travel through the transcript.
        assert_eq!(messages[0].role, "system");
        let head = &messages[0].content;
        assert!(head.contains("Core System Contract"));
        assert!(head.contains("## Repository Map"));
        assert!(head.contains("<repo_map>"));
        assert!(head.contains("crates/core/src/git.rs"));
        assert!(head.contains("Action Authorization"));
        assert!(!head.contains("src/main.rs"));
        assert!(!head.contains("fn main() {}"));
        assert!(messages
            .iter()
            .any(|message| message.content.contains("## Compacted Session Summary")));
        assert!(messages
            .iter()
            .any(|message| message.content.contains(r#"path="src/main.rs""#)
                && message.content.contains("fn main() {}")));
        assert!(!messages
            .iter()
            .any(|message| message.content.contains("<history>")));
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("Explain this code")
        );
    }

    #[test]
    fn test_prompt_assembly_discussion_mode() {
        let files = vec![
            ContextFileContent {
                path: "src/main.rs".to_string(),
                access: ContextAccess::Editable,
                content: "fn main() {}\n".to_string(),
                insertion_seq: 0,
            },
            ContextFileContent {
                path: "src/lib.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content: "pub fn helper() {}\n".to_string(),
                insertion_seq: 0,
            },
        ];
        let assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Let's discuss architecture", &proto);
        let sys = &messages[0].content;

        // A plan discussion is a dialogue like any other: the rule that
        // restricted it to prose is gone, and direct edits are authorized by
        // the very same Action Authorization rule the REPL states.
        assert!(!sys.contains("Discussion Mode Scope"));
        assert!(!sys.contains("architectural discussion"));
        assert!(sys.contains("Action Authorization"));
        assert!(sys.contains("Context Surface"));
        assert!(!sys.contains(r#"path="src/main.rs""#));

        // A plan discussion is a dialogue like any other: every element carries
        // the layer the file actually resolves to, so an editable file stays
        // editable and a reference file stays read-only instead of the whole
        // context being flattened into a read-only view. The assertion targets
        // the rendered element rather than the whole prompt, because the contract
        // names both attribute values as prompt-side vocabulary.
        // The head states the contract and never carries a file body, so the
        // element sequence is the transcript message that opens the block.
        let context = messages
            .iter()
            .find(|message| message.content.starts_with("<context>\n"))
            .expect("the files travel as a transcript message");
        assert!(context
            .content
            .contains(r#"path="src/main.rs" access="editable""#));
        assert!(!context
            .content
            .contains(r#"path="src/main.rs" access="read_only""#));
        assert!(context
            .content
            .contains(r#"path="src/lib.rs" access="read_only""#));
        assert!(!context
            .content
            .contains(r#"path="src/lib.rs" access="editable""#));
        assert!(!context.content.contains("<editable_files>"));
        assert!(!context.content.contains("<read_only_files>"));

        // The answer obligation is owned by the enabled capability modules: a
        // discussion that gathered its material answers in prose instead of
        // closing the turn silently.
        // The response structure belongs to the schema alone: the contract
        // states the answer obligation and the prompt names no response field as
        // a reference.
        assert!(sys.contains("never closed by silence"));
        assert!(!sys.contains("- `message`:"));
        assert!(!sys.contains("- `context_requests`:"));
        assert!(sys.contains("plan_update"));
        assert!(sys.contains("context_requests"));

        // The assembly contributes no wire-format knowledge of its own.
        assert!(!sys.contains("Mode: Structured Discussion"));
        assert!(!sys.contains("conforming to the discussion schema"));
        assert!(!sys.contains("discussion_response schema"));
        assert!(!sys.contains(
            "with `message`, `plan_update`, `context_requests`, `repo_searches`, and `user_language`"
        ));
    }

    #[test]
    fn test_discussion_renders_stub_protocol_instructions_without_json_schema() {
        use crate::llm::CapabilityTree;
        use crate::llm::LlmCapability;
        use crate::llm::{LlmCommand, TextCommand};
        use crate::prompt::protocol::PromptProtocol;
        use crate::workflow::PatchStrategy;

        /// Protocol stub whose only mode text is its own heading.
        struct StubWireProtocol;

        impl PromptProtocol for StubWireProtocol {
            fn name(&self) -> &'static str {
                "stub-wire"
            }

            fn kind(&self) -> EditProtocolKind {
                EditProtocolKind::Structured
            }

            fn capability_expr(&self) -> CapabilityTree {
                CapabilityTree::set([CapabilityTree::leaf(LlmCapability::TextMessage)])
            }

            fn decode(&self, raw_text: &str, _context_paths: &[String]) -> Vec<LlmCommand> {
                vec![LlmCommand::Text(TextCommand {
                    content: raw_text.to_string(),
                })]
            }

            fn system_instructions(
                &self,
                _editable_paths: &[String],
                _has_eager_repo_map: bool,
                _patch_strategy: PatchStrategy,
            ) -> String {
                "## Mode: Stub Wire\nAnswer in the stub wire format.\n".to_string()
            }

            fn with_capability_expr(&self, _expr: CapabilityTree) -> Box<dyn PromptProtocol> {
                Box::new(StubWireProtocol)
            }

            fn create_stream_filter(
                &self,
                editable_paths: Vec<String>,
                repo_root: std::path::PathBuf,
                staged_contents: std::collections::HashMap<String, String>,
            ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
                Box::new(
                    crate::edits::stream::JsonStreamFilter::new(editable_paths, repo_root)
                        .with_staged_contents(staged_contents),
                )
            }
        }

        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let prompt = assembly.build_system_prompt(&StubWireProtocol);

        assert!(prompt.contains("## Mode: Stub Wire"));
        assert!(!prompt.contains("Mode: Structured Discussion"));
        assert!(!prompt.contains("conforming to the discussion schema"));
        assert!(!prompt.contains("discussion_response schema"));
        assert!(!prompt.contains("Output MUST be a JSON object"));
        assert!(!prompt.contains(
            "with `message`, `plan_update`, `context_requests`, `repo_searches`, and `user_language`"
        ));
    }

    #[test]
    fn test_discussion_instructions_compose_the_edit_surface_once() {
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        // A plan discussion carries the whole dialogue surface, so the structured
        // transport derives the edit mode and composes the registry schema, where
        // the plan payload is already declared. The conversation-specific mode
        // section is therefore never rendered for it.
        let protocol = crate::prompt::json::JsonPromptProtocol::discussion();
        let prompt = assembly.build_system_prompt(&protocol);

        assert_eq!(prompt.matches("Code Modification Protocol").count(), 1);
        assert!(prompt.contains("`changes`"));
        assert!(prompt.contains("`plan_update`"));
        assert!(!prompt.contains("## Mode: Structured Discussion"));
        assert!(!prompt.contains("conforming to the discussion schema"));
        assert!(!prompt.contains(
            "with `message`, `plan_update`, `context_requests`, `repo_searches`, and `user_language`"
        ));
    }

    /// The element rendering of one file is stable: the context revision never
    /// enters it, and promoting a file rewrites only its own attribute.
    #[test]
    fn test_context_element_rendering_is_static_and_revision_independent() {
        let file = |path: &str, access: ContextAccess| ContextFileContent {
            path: path.to_string(),
            access,
            content: format!("// {path}\n"),
            insertion_seq: 0,
        };

        let mut files = [
            file("src/a.rs", ContextAccess::ReadOnly),
            file("src/b.rs", ContextAccess::Editable),
        ];
        let mut cache = ContextBlockCache::default();
        let neighbor = cache
            .element(&files[1], files[1].access, PatchStrategy::SearchReplace)
            .to_string();
        let first = cache
            .element(&files[0], files[0].access, PatchStrategy::SearchReplace)
            .to_string();
        assert!(first.starts_with(r#"    <file path="src/a.rs" access="read_only">"#));
        assert!(!first.contains("revision"));
        assert!(first.contains("// src/a.rs\n"));
        assert!(neighbor.contains(r#"path="src/b.rs" access="editable""#));

        // Promotion rewrites only the access attribute of that one element and
        // never relocates it or reflows its neighbor.
        files[0].access = ContextAccess::Editable;
        let promoted = cache
            .element(&files[0], files[0].access, PatchStrategy::SearchReplace)
            .to_string();
        assert_eq!(promoted.matches(r#"path="src/a.rs""#).count(), 1);
        assert!(promoted.contains(r#"path="src/a.rs" access="editable""#));
        assert_eq!(
            cache
                .element(&files[1], files[1].access, PatchStrategy::SearchReplace)
                .to_string(),
            neighbor
        );
    }

    /// The wrapper estimate and the rendered element come from one builder, so
    /// the price of a file can never disagree with the markup the model reads.
    #[test]
    fn test_context_element_wrapper_is_one_builder_for_render_and_estimate() {
        let file = ContextFileContent {
            path: "src/deep/nested/module.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
            insertion_seq: 0,
        };

        let open = context_element_open_tag(&file.path, file.access);
        let close = context_element_close_tag();
        let content = &file.content;
        let element = render_context_element(&file, file.access, PatchStrategy::SearchReplace);
        assert_eq!(element, format!("{open}{content}{close}"));
        assert_eq!(
            context_element_overhead_tokens(&file.path, file.access),
            estimate_tokens(&open) + estimate_tokens(close)
        );

        // The path travels inside the header, so a deep path costs more than a
        // shallow one even when the content is identical.
        let deep = context_element_overhead_tokens(&file.path, file.access);
        let shallow = context_element_overhead_tokens("a.rs", file.access);
        assert!(deep > shallow);
    }

    #[test]
    fn test_context_element_renders_opaque_anchors_for_every_file() {
        use crate::edits::anchor::encode_anchor;

        let files = [
            ContextFileContent {
                path: "src/a.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content: "alpha\nbeta\n".to_string(),
                insertion_seq: 0,
            },
            ContextFileContent {
                path: "src/b.rs".to_string(),
                access: ContextAccess::Editable,
                content: "gamma".to_string(),
                insertion_seq: 0,
            },
        ];
        let mut cache = ContextBlockCache::default();
        let anchored: Vec<String> = files
            .iter()
            .map(|file| {
                cache
                    .element(file, file.access, PatchStrategy::LineAnchor)
                    .to_string()
            })
            .collect();
        let a = &anchored[0];
        let b = &anchored[1];

        // Both access layers carry anchors, so a read-only file is addressable
        // exactly like an editable one.
        assert!(a.contains(r#"access="read_only""#));
        assert!(b.contains(r#"access="editable""#));
        // Every line names its zero-based index through the codec seeded by its
        // own file, so the same line of two files carries unrelated tokens.
        assert!(a.contains(&format!("{} | alpha", encode_anchor("src/a.rs", 0))));
        assert!(a.contains(&format!("{} | beta", encode_anchor("src/a.rs", 1))));
        assert!(b.contains(&format!("{} | gamma", encode_anchor("src/b.rs", 0))));
        assert!(!a.contains("\nalpha"));
        // The closing newline of the file is still rendered exactly once.
        assert!(!a.contains(&format!("{} | beta\n\n", encode_anchor("src/a.rs", 1))));

        // The search/replace strategy keeps the raw lines untouched. The memo is
        // fresh, because an element an earlier round rendered under another
        // strategy is handed back verbatim.
        let mut raw_cache = ContextBlockCache::default();
        let raw: Vec<String> = files
            .iter()
            .map(|file| {
                raw_cache
                    .element(file, file.access, PatchStrategy::SearchReplace)
                    .to_string()
            })
            .collect();
        assert!(raw[0].contains("\nalpha\nbeta\n"));
        assert!(!raw[0].contains(&format!("{} | ", encode_anchor("src/a.rs", 0))));
    }

    #[test]
    fn test_context_cache_reuses_identical_bytes_and_rerenders_only_changed_files() {
        use crate::edits::anchor::encode_anchor;

        let mut files = [
            ContextFileContent {
                path: "src/a.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content: "alpha\n".to_string(),
                insertion_seq: 0,
            },
            ContextFileContent {
                path: "src/b.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content: "beta\n".to_string(),
                insertion_seq: 0,
            },
        ];

        let mut cache = ContextBlockCache::default();
        let first = cache
            .element(&files[0], files[0].access, PatchStrategy::LineAnchor)
            .to_string();
        let untouched = cache
            .element(&files[1], files[1].access, PatchStrategy::LineAnchor)
            .to_string();
        assert_eq!(cache.cached_files(), 2);

        // An unchanged round is reproduced byte for byte from the memo.
        assert_eq!(
            cache
                .element(&files[0], files[0].access, PatchStrategy::LineAnchor)
                .to_string(),
            first
        );
        assert_eq!(cache.cached_files(), 2);
        assert!(untouched.contains(&format!("{} | beta", encode_anchor("src/b.rs", 0))));

        // Promoting a file rewrites only its own element; its neighbor keeps the
        // exact bytes it already had, so the prefix stays stable.
        files[0].access = ContextAccess::Editable;
        let promoted = cache
            .element(&files[0], files[0].access, PatchStrategy::LineAnchor)
            .to_string();
        assert!(promoted.contains(r#"path="src/a.rs" access="editable""#));
        assert!(promoted.contains(&format!("{} | alpha", encode_anchor("src/a.rs", 0))));
        assert_eq!(
            cache
                .element(&files[1], files[1].access, PatchStrategy::LineAnchor)
                .to_string(),
            untouched
        );
    }

    #[test]
    fn test_anchored_context_falls_back_to_raw_beyond_the_codec_domain() {
        use crate::edits::anchor::encode_anchor;

        // The codec covers `u16::MAX + 1` lines. A file of exactly that length is
        // still addressable; one line more cannot be addressed at all.
        let within_domain: String = (0..=u16::MAX).map(|i| format!("line {i}\n")).collect();
        let beyond_domain = format!("{within_domain}line beyond\n");

        let render = |content: String| {
            let file = ContextFileContent {
                path: "src/big.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content,
                insertion_seq: 0,
            };
            let mut cache = ContextBlockCache::default();
            cache
                .element(&file, file.access, PatchStrategy::LineAnchor)
                .to_string()
        };

        let block = render(within_domain);
        assert!(block.contains(&format!(
            "{} | line 65535",
            encode_anchor("src/big.rs", u16::MAX)
        )));

        let block = render(beyond_domain);
        assert!(block.contains("\nline beyond\n"));
        assert!(!block.contains(&format!("{} | ", encode_anchor("src/big.rs", 0))));
    }

    #[test]
    fn test_stable_prefix_order_and_volatile_tail() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
            insertion_seq: 0,
        }];
        let mut assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Structured,
            Some(std::sync::Arc::new(HistorySnapshot::new(vec![
                HistoryEntry::Summary {
                    id: Some(1),
                    text: "Earlier work".to_string(),
                },
            ]))),
            Some("crate\n  fn main".to_string()),
        );
        assembly.turn_scratchpad = Some("- verified: prefix is stable".to_string());
        assembly.active_plan = Some(Plan::from_parts(
            "prefix-plan".to_string(),
            "Prefix Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![crate::plan::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                ..Default::default()
            }],
        ));

        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Do work", &proto);

        let head = &messages[0].content;
        assert_eq!(messages[0].role, "system");
        assert!(head.contains("## Repository Map"));
        assert!(head.contains("crate\n  fn main"));
        // The context body and the interaction records travel through the
        // transcript, never through the byte-stable head.
        assert!(!head.contains(r#"path="src/main.rs""#));
        assert!(!head.contains("fn main() {}"));
        assert!(messages
            .iter()
            .any(|message| message.content.contains("## Compacted Session Summary")));
        assert!(messages
            .iter()
            .any(|message| message.content.contains("fn main() {}")));

        // The volatile tail carries the plan, the context manifest, and the
        // scratchpad in one growing block, and the task closes the sequence.
        let tail = &messages[messages.len() - 2].content;
        let plan = tail
            .find("## Active Task Plan Context")
            .expect("active plan block");
        let manifest = tail
            .find("## Project Context Manifest")
            .expect("context manifest block");
        let scratchpad = tail
            .find("## Turn Scratchpad (Working Memory)")
            .expect("scratchpad block");
        assert!(plan < manifest);
        assert!(manifest < scratchpad);
        assert!(tail.trim_end().ends_with("</scratchpad>"));
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("Do work")
        );
    }

    #[test]
    fn test_repository_outline_is_map_or_directories_never_both() {
        let base = || {
            PromptAssembly::new(
                None,
                1,
                vec![],
                "git",
                EditProtocolKind::Structured,
                None,
                None,
            )
        };

        // The rendered outline block, as opposed to the bare `<repo_dirs>` tag
        // name that the protocol instructions advertise in every turn.
        const DIRS_BLOCK: &str = "<repo_dirs>\ncrate\n  src\n</repo_dirs>";

        // An eager symbol map wins and suppresses the directory outline, even
        // though an outline is available to render.
        let mut with_map = base().with_repo_dirs(Some("crate\n  src".to_string()));
        with_map.repo_map = Some("crate\n  pub fn main".to_string());
        assert!(with_map.format_repo_dirs_block().is_some());
        let proto = JsonPromptProtocol::default();
        let messages = with_map.assemble_chat_messages("Go", &proto);
        let sys = &messages[0].content;
        assert!(sys.contains("## Repository Map"));
        assert!(!sys.contains("## Repository Directories"));
        assert!(!sys.contains(DIRS_BLOCK));

        // Without a map the compact outline answers where subsystems live.
        let with_dirs = base().with_repo_dirs(Some("crate\n  src".to_string()));
        let proto = JsonPromptProtocol::default();
        let messages = with_dirs.assemble_chat_messages("Go", &proto);
        let sys = &messages[0].content;
        assert!(!sys.contains("## Repository Map"));
        assert!(sys.contains("## Repository Directories"));
        assert!(sys.contains(DIRS_BLOCK));

        // A blank outline renders no block at all.
        let blank = base().with_repo_dirs(Some("   ".to_string()));
        assert!(blank.format_repo_dirs_block().is_none());

        // With neither input the prefix carries no repository outline.
        let proto = JsonPromptProtocol::default();
        let messages = base().assemble_chat_messages("Go", &proto);
        let sys = &messages[0].content;
        assert!(!sys.contains("## Repository Map"));
        assert!(!sys.contains("## Repository Directories"));
    }

    #[test]
    fn test_eager_repo_map_flag_tracks_the_rendered_block() {
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assert!(!assembly.has_eager_repo_map());
        assert!(assembly.format_repo_map_block().is_none());

        // A blank map renders no block, so it must not claim an eager mode.
        assembly.repo_map = Some("   ".to_string());
        assert!(!assembly.has_eager_repo_map());
        assert!(assembly.format_repo_map_block().is_none());

        assembly.repo_map = Some("crate\n  pub fn main".to_string());
        assert!(assembly.has_eager_repo_map());
        assert!(assembly.format_repo_map_block().is_some());
    }

    #[test]
    fn test_eager_repo_map_suppresses_on_demand_map_instructions() {
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            Some("crates/core/src/prompt.rs\n  pub fn estimate_tokens".to_string()),
        );
        assert!(assembly.has_eager_repo_map());

        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Go", &proto);
        let system = &messages[0].content;

        // The static symbol contours already answer where symbols live, so no
        // rendered instruction may advertise an on-demand map request.
        assert!(system.contains("## Repository Map"));
        assert!(!system.contains("repo_map path="));
        assert!(system.contains("repository symbol map is already rendered"));

        // Discovery still routes through targeted search, directory listings,
        // and full-file loading.
        assert!(system.contains("context_requests"));
        assert!(system.contains("repo_searches"));
        assert!(system.contains("dir_lists"));
        assert!(system.contains("repo_dirs"));
    }

    #[test]
    fn test_on_demand_repo_map_instructions_carry_an_explicit_example() {
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assert!(!assembly.has_eager_repo_map());

        let proto = JsonPromptProtocol::default();
        let messages = assembly.assemble_chat_messages("Go", &proto);
        let system = &messages[0].content;

        assert!(!system.contains("## Repository Map"));
        assert!(system.contains("repo_maps"));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Multi-byte UTF-8 is sized by encoded byte length, not scalar count.
        assert_eq!(estimate_tokens("абвг"), 2);
        assert_eq!(estimate_tokens_from_bytes(0), 0);
        assert_eq!(estimate_tokens_from_bytes(4), 1);
        assert_eq!(estimate_tokens_from_bytes(5), 2);
    }

    #[test]
    fn test_prompt_assembly_isolated_step() {
        let proto = step_protocol();
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assembly.turn_brief = Some(TurnBrief::step());
        assembly.active_plan = Some(Plan::from_parts(
            "plan-test".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        ));

        let sys_prompt = assembly.build_system_prompt(proto.as_ref());
        assert!(!sys_prompt.contains("Local Engineering Plans"));
        assert!(sys_prompt.contains("Step Execution Scope"));
        assert!(!sys_prompt.contains("<plan"));
        assert!(!sys_prompt.contains("<verify"));
        // The introduction belongs to the contour, so a step opens with its own
        // brief instead of the dialogue introduction.
        assert!(sys_prompt.contains("Tauqe Coder"));
        assert!(!sys_prompt.contains("proactive engineering assistant"));

        // An isolated step carries no documentation surface: the capability is
        // absent from its profile, so neither the self-knowledge rule nor the
        // environment ontology reaches it. A coder executing an approved design
        // never reads about a harness it cannot consult.
        assert!(!sys_prompt.contains("Self-Knowledge"));
        assert!(!sys_prompt.contains("<tauqe_doc_request"));
        assert!(!sys_prompt.contains("<doc_request"));
        assert!(!sys_prompt.contains("Environment Ontology"));
        let messages = assembly.assemble_chat_messages("Execute step", proto.as_ref());
        assert!(!messages[0].content.contains("## Active Task Plan Context"));
    }

    /// The introduction belongs to the contour: the assembly renders the brief the
    /// loop declared, and a turn without one opens with the dialogue introduction.
    #[test]
    fn the_turn_brief_renders_the_introduction_of_its_contour() {
        let proto = JsonPromptProtocol::default();
        let render = |turn_brief: Option<TurnBrief>| {
            let mut assembly = PromptAssembly::new(
                None,
                1,
                vec![],
                "git",
                EditProtocolKind::Structured,
                None,
                None,
            );
            assembly.turn_brief = turn_brief;
            assembly.build_system_prompt(&proto)
        };

        assert!(render(None).starts_with(DIALOGUE_INTRO));

        let step = render(Some(TurnBrief::step()));
        assert!(step.starts_with(STEP_INTRO));
        assert!(!step.starts_with(DIALOGUE_INTRO));

        // The brief also selects the regime the contract renders, so one
        // assembly states the dialogue authorization rule under the dialogue
        // contour and the scope of an isolated step under the step contour.
        let dialogue = render(Some(TurnBrief::dialogue()));
        assert!(dialogue.contains("Action Authorization"));
        assert!(!dialogue.contains("Step Execution Scope"));
        assert!(step.contains("Step Execution Scope"));
        assert!(step.contains("Architectural Decision Immutability"));
        assert!(!step.contains("Action Authorization"));

        // A synthesis opens with the introduction of its own planning role and
        // differs in the contour that gates its regime.
        let architect = render(Some(TurnBrief::architect()));
        assert!(architect.starts_with(ARCHITECT_INTRO));
        assert!(!architect.contains("Step Execution Scope"));
    }

    #[test]
    fn test_interactive_repl_routes_harness_questions_to_the_documentation() {
        let proto = JsonPromptProtocol::default();
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );

        // The self-knowledge rule routes a question about the harness to the
        // authoritative documentation, so the intent delta carries no second
        // lecture about the environment beside it.
        let sys_prompt = assembly.build_system_prompt(&proto);
        assert!(sys_prompt.contains("Self-Knowledge"));
        assert!(sys_prompt.contains("tauqe_doc_requests"));
        assert!(!sys_prompt.contains("<doc_request"));
        assert!(!sys_prompt.contains("Environment Ontology"));
        assert!(sys_prompt.contains("`tauqe.toml`"));
    }

    #[test]
    fn test_documentation_surface_is_absent_from_the_isolated_step_schema() {
        let render =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::SearchReplace);
        let interactive = flattened_surface(&crate::capabilities::capability_expr_schema(
            &crate::control::repl::repl_capability_expr(None),
            &render,
        ));
        assert!(interactive["properties"].get("context_requests").is_some());
        assert!(interactive["properties"]
            .get("tauqe_doc_requests")
            .is_some());
        assert!(interactive["properties"].get("doc_requests").is_none());

        // The composed schema is a projection of the capability set, so a turn
        // that cannot consult the documentation never advertises a field its
        // decoder would reject.
        let step = flattened_surface(&crate::capabilities::capability_expr_schema(
            &crate::control::turn::turn_capability_expr(),
            &render,
        ));
        assert!(step["properties"].get("tauqe_doc_requests").is_none());
        assert!(step["properties"].get("doc_requests").is_none());
        assert!(step["properties"].get("changes").is_some());
        // Toolchain verification belongs to the harness for a step, so the
        // strict step schema declares no verification channel either.
        assert!(step["properties"].get("verify").is_none());
    }

    #[test]
    fn test_planning_prompt_instructions() {
        let proto = JsonPromptProtocol::default();

        // Develop mode
        let dev_assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let dev_prompt = dev_assembly.build_system_prompt(&proto);
        assert!(dev_prompt.contains("Conceptual Foundation Invariant"));
        assert!(dev_prompt.contains("concept"));
        assert!(dev_prompt.contains("Leaf-Only Files Invariant"));
        // The plan rules state the invariants the model must satisfy; the
        // execution-time checks that follow from them are enforced by the
        // harness itself and are never restated here.
        assert!(dev_prompt.contains("Plan Creation Invariant"));
        assert!(dev_prompt.contains("only create a plan when explicitly instructed"));

        // Discussion mode
        // Discussion mode
        let disc_assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        // The plan-action semantics follow the surface the round actually
        // advertises, so the plan-anchored surface of the discussion protocol
        // is what authorizes in-place plan mutation.
        let disc_prompt = disc_assembly.build_system_prompt(&JsonPromptProtocol::discussion());
        assert!(disc_prompt.contains("Conceptual Foundation Invariant"));
        assert!(disc_prompt.contains("Leaf-Only Files Invariant"));
        assert!(disc_prompt.contains("Plan Action Semantics"));
        assert!(disc_prompt.contains("action: \"update\""));
    }

    #[test]
    fn test_prompt_assembly_research_instructions() {
        let proto = JsonPromptProtocol::default();

        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let prompt = assembly.build_system_prompt(&proto);
        assert!(!prompt.contains("<context_drop"));
        assert!(!prompt.contains("Incremental Strategy"));
        assert!(!prompt.contains("Snapshot Strategy"));
        assert!(prompt.contains("context_requests"));
        assert!(prompt.contains("empty answer is never a valid conclusion"));
    }
    #[test]
    fn test_system_prompt_anchors_project_configuration() {
        use crate::llm::LlmCapability;

        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let proto = JsonPromptProtocol::default();
        let full = assembly.build_system_prompt(&proto);

        // The configuration anchor belongs to the static introduction, and the
        // self-knowledge rule routes configuration questions to that topic.
        assert!(full.contains("tauqe.toml"));
        assert!(full.contains("Self-Knowledge"));

        // With documentation requests disabled the anchor survives as prose.
        let text_only = JsonPromptProtocol::with_capabilities(CapabilityTree::from_leaves([
            LlmCapability::TextMessage,
        ]));
        let narrowed = assembly.build_system_prompt(&text_only);
        assert!(narrowed.contains("tauqe.toml"));
        assert!(!narrowed.contains("Self-Knowledge"));

        // Discussion mode renders the same authoritative anchor.
        let discussion = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let proto = JsonPromptProtocol::discussion();
        let prompt = discussion.build_system_prompt(&proto);
        assert!(prompt.contains("tauqe.toml"));
    }

    #[test]
    fn test_prompt_assembly_with_turn_scratchpad() {
        let proto = JsonPromptProtocol::default();
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad = Some("   ".to_string());
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad =
            Some("- Hypothesis: Foo needs Bar\n- Status: Inspecting bar.rs".to_string());
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("<scratchpad>"));
        assert!(block.contains("- Hypothesis: Foo needs Bar"));
        assert!(block.contains("</scratchpad>"));

        // The scratchpad is rewritten on every round, so it travels in the
        // volatile tail of the transcript rather than in the byte-stable head.
        let messages = assembly.assemble_chat_messages("Continue", &proto);
        let tail = &messages[messages.len() - 2].content;
        assert!(tail.contains("## Turn Scratchpad (Working Memory)"));
        assert!(tail.contains("- Hypothesis: Foo needs Bar"));
    }

    #[test]
    fn test_turn_scratchpad_truncation_utf8_safety() {
        let multi_byte_text = "Рабочая гипотеза и детали проверки UTF-8 символов. ".repeat(200);
        assert!(multi_byte_text.len() > crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS);

        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assembly.turn_scratchpad = Some(multi_byte_text);
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("... [scratchpad truncated for length] ..."));
    }

    #[test]
    fn test_prompt_assembly_renders_protocol_instructions() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
            insertion_seq: 0,
        }];
        let assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );

        let json = JsonPromptProtocol::default();
        let json_prompt = assembly.build_system_prompt(&json);
        assert!(json_prompt.contains("Code Modification Protocol (Structured Output JSON)"));

        let messages = assembly.assemble_chat_messages("Explain this code", &json);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0]
            .content
            .contains("Code Modification Protocol (Structured Output JSON)"));
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("Explain this code")
        );
    }

    #[test]
    fn test_prompt_assembly_states_the_proactive_engineering_stance() {
        let proto = JsonPromptProtocol::default();

        // Interactive REPL turn: a direct request is executed immediately,
        // while an open or architectural question is answered in prose.
        let assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let prompt = assembly.build_system_prompt(&proto);
        // The unified composition states the proactive engineering stance
        // beside the intent-independent trunk it refines.
        assert!(prompt.contains("Action Authorization"));
        assert!(prompt.contains("executed immediately"));
        assert!(prompt.contains("Discussion is the mode"));
        assert!(!prompt.contains("Default to discussion"));

        // An isolated step turn is itself the explicit instruction, so the rule
        // is deliberately omitted there instead of contradicting the step scope.
        let mut isolated = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        isolated.turn_brief = Some(TurnBrief::step());
        let isolated_protocol = step_protocol();
        let isolated_prompt = isolated.build_system_prompt(isolated_protocol.as_ref());
        assert!(!isolated_prompt.contains("Action Authorization"));

        // A plan discussion carries the whole dialogue surface, so the very same
        // authorization rule guards it: a change there is permitted only when
        // the developer states the request directly, exactly as in the REPL.
        let discussion = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        let discussion_prompt = discussion.build_system_prompt(&proto);
        assert!(discussion_prompt.contains("Action Authorization"));
        assert!(!discussion_prompt.contains("Discussion Mode Scope"));
    }

    #[test]
    fn test_parse_structured_scribe_output_tolerates_fences_and_prose() {
        assert_eq!(
            parse_compaction_summary("{\"summary\":\"Done A and B\"}").as_deref(),
            Some("Done A and B")
        );
        assert_eq!(
            parse_squash_commit_message("```json\n{\"commit_message\":\"feat: add x\"}\n```")
                .as_deref(),
            Some("feat: add x")
        );
        assert_eq!(
            parse_squash_commit_message("Here: {\"commit_message\":\"fix: y\"} done").as_deref(),
            Some("fix: y")
        );
        let raw = "{\"summary\":\"Задача выполнена — контекст сохранён\"}";
        assert_eq!(
            parse_compaction_summary(raw).as_deref(),
            Some("Задача выполнена — контекст сохранён")
        );
        let fenced = "```json\n{\"commit_message\":\"feat: добавить функцию\"}\n```";
        assert_eq!(
            parse_squash_commit_message(fenced).as_deref(),
            Some("feat: добавить функцию")
        );
    }

    /// Returns the `Core System Contract` section of a rendered prompt.
    ///
    /// The section ends at the first subsequent markdown heading, so the
    /// numbering check never inspects edit-protocol or metadata sections.
    fn contract_section(prompt: &str) -> &str {
        let header = "## Core System Contract\n";
        let start = match prompt.find(header) {
            Some(idx) => idx + header.len(),
            None => return "",
        };
        let rest = &prompt[start..];
        let end = rest.find("\n#").unwrap_or(rest.len());
        &rest[..end]
    }

    /// Collects the indices of densely numbered contract rule headers.
    ///
    /// A rule header is a line of the form `N. Title: body`, so only lines
    /// whose prefix is all digits and whose title opens with an uppercase
    /// letter are counted. Inline body content is ignored.
    fn contract_rule_numbers(section: &str) -> Vec<usize> {
        section
            .lines()
            .filter_map(|line| {
                let (prefix, rest) = line.split_once(". ")?;
                if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let starts_title = rest
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false);
                if !starts_title {
                    return None;
                }
                prefix.parse::<usize>().ok()
            })
            .collect()
    }

    #[test]
    fn test_core_system_contract_is_numbered_monotonically() {
        for isolated in [false, true] {
            let proto = if isolated {
                step_protocol()
            } else {
                dialogue_protocol()
            };
            let mut assembly = PromptAssembly::new(
                None,
                1,
                vec![],
                "git",
                EditProtocolKind::Structured,
                None,
                None,
            );
            assembly.turn_brief = Some(if isolated {
                TurnBrief::step()
            } else {
                TurnBrief::dialogue()
            });
            let prompt = assembly.build_system_prompt(proto.as_ref());
            let section = contract_section(&prompt);
            assert!(
                !section.is_empty(),
                "missing contract for isolated = {isolated}"
            );

            let numbers = contract_rule_numbers(section);
            assert!(
                !numbers.is_empty(),
                "no rules rendered for isolated = {isolated}"
            );
            let expected: Vec<usize> = (1..=numbers.len()).collect();
            assert_eq!(
                numbers, expected,
                "non-monotonic numbering for isolated = {isolated}"
            );
        }
    }

    #[test]
    fn test_core_system_contract_has_no_lettered_surrogates() {
        for isolated in [false, true] {
            let proto = if isolated {
                step_protocol()
            } else {
                dialogue_protocol()
            };
            let mut assembly = PromptAssembly::new(
                None,
                1,
                vec![],
                "git",
                EditProtocolKind::Structured,
                None,
                None,
            );
            assembly.turn_brief = Some(if isolated {
                TurnBrief::step()
            } else {
                TurnBrief::dialogue()
            });
            let prompt = assembly.build_system_prompt(proto.as_ref());
            let section = contract_section(&prompt);
            for surrogate in ["1a.", "2a.", "4a.", "8a.", "10a.", "11a."] {
                assert!(
                    !section.contains(surrogate),
                    "surrogate {surrogate} present for isolated = {isolated}"
                );
            }
        }
    }

    /// Builds one declared external document for the catalog tests.
    fn outer_doc(id: &str, title: &str, description: Option<&str>) -> crate::config::OuterDocEntry {
        crate::config::OuterDocEntry {
            id: id.to_string(),
            title: title.to_string(),
            description: description.map(str::to_string),
            source: format!("https://example.com/{id}"),
        }
    }

    #[test]
    fn test_outer_docs_catalog_lists_the_declared_documents() {
        let declared = vec![
            outer_doc(
                "vendor/api.md",
                "Upstream API notes",
                Some("Endpoint contracts."),
            ),
            outer_doc("mylib", "MyLib Book", None),
        ];
        let built = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        )
        .with_outer_docs(declared);
        let catalog = built
            .format_outer_docs_catalog_block()
            .expect("the declarations render a catalog");
        assert!(catalog.starts_with("## External Documentation Catalog\n"));
        // The heading carries the invocation of a declared document, so the
        // trunk rule states only the essence of the declaration.
        assert!(catalog.contains("`outer_doc_requests`"));
        assert!(catalog.contains("`id/relative.md`"));
        assert!(catalog.contains("never climbs above it"));
        assert!(catalog.contains("- vendor/api.md — Upstream API notes: Endpoint contracts.\n"));
        assert!(catalog.contains("- mylib — MyLib Book\n"));
        // Entries keep their declaration order.
        let api = catalog.find("vendor/api.md").expect("the first entry");
        let book = catalog.find("mylib").expect("the second entry");
        assert!(api < book);
        // The catalog publishes what a request may name and nothing about where
        // the material comes from.
        assert!(!catalog.contains("https"));
        assert!(!catalog.contains(".tauqe"));
    }

    #[test]
    fn test_outer_docs_catalog_renders_nothing_without_a_resolvable_entry() {
        let empty = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assert!(empty.format_outer_docs_catalog_block().is_none());

        // An entry configuration validation rejects contributes no line, so a
        // declaration nothing can mount renders no block at all.
        let broken = crate::config::OuterDocEntry {
            id: "vendor/broken.md".to_string(),
            title: "Broken".to_string(),
            description: None,
            source: String::new(),
        };
        let built = empty.with_outer_docs(vec![broken]);
        assert!(built.format_outer_docs_catalog_block().is_none());
    }

    #[test]
    fn test_outer_docs_catalog_follows_the_capability_surface() {
        use crate::llm::LlmCapability;

        let declared = vec![outer_doc("vendor/api.md", "Upstream API notes", None)];
        let built = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        )
        .with_outer_docs(declared);

        // A turn that can request an external document publishes the catalog in
        // the byte-stable head, and the head carries no repository metadata: the
        // working-tree state reaches the model through the context block alone.
        let messages = built.assemble_chat_messages("Go", &JsonPromptProtocol::default());
        let system = &messages[0].content;
        assert!(!system.contains("## Project Metadata"));
        let catalog = system
            .find("## External Documentation Catalog")
            .expect("catalog block");
        // The catalog closes the head after the protocol instructions, and a
        // turn without context files renders no context block at all.
        assert!(!system.contains("## Project Context"));
        let instructions = system
            .find("## Code Modification Protocol")
            .expect("protocol instructions");
        assert!(instructions < catalog);
        assert!(system.contains("- vendor/api.md — Upstream API notes\n"));
        // The catalog is a head block: the task message never repeats it.
        assert!(!messages[1]
            .content
            .contains("External Documentation Catalog"));
        // Assembling the same state twice renders the very same bytes.
        assert_eq!(
            built.format_outer_docs_catalog_block(),
            built.format_outer_docs_catalog_block()
        );

        // A narrowed surface that cannot request a document advertises none.
        let narrowed = JsonPromptProtocol::with_capabilities(CapabilityTree::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::EditReplace,
        ]));
        let prompt = built.build_system_prompt(&narrowed);
        assert!(!prompt.contains("## External Documentation Catalog"));
        // The invocation disappears with the capability that performs it, so a
        // narrowed turn never states a channel it cannot use.
        assert!(!prompt.contains("`outer_doc_requests`"));
    }
}
