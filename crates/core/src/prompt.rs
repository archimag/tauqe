pub mod json;
pub mod layout;
pub mod protocol;
pub mod xml;

use std::collections::HashMap;

use crate::capabilities::{CapabilityRegistry, ContractContext, PromptRenderContext};
use crate::context::ContextFileContent;
use crate::edits::anchor::{encode_anchor, ANCHOR_LEN};
use crate::edits::stream::json::decode::first_json_object_str;
use crate::history::HistorySnapshot;
use crate::prompt::layout::layout_for;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{ChatMessage, JsonSchemaDefinition, ResponseFormat};
use tauqe_protocol::{
    CapabilitySet, ContextAccess, EditProtocolKind, ModelRole, PatchStrategy, Plan,
    PromptLayoutKind, RepositoryState, TurnIntent,
};

/// Default model role used for squash commit message synthesis.
pub const SQUASH_COMMIT_ROLE: ModelRole = ModelRole::Scribe;

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
pub fn history_compaction_response_schema() -> serde_json::Value {
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

/// Builds the provider [`ResponseFormat`] that enforces the history compaction schema.
pub fn history_compaction_response_format() -> ResponseFormat {
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name: "history_compaction".to_string(),
            description: Some(
                "Concise structured summary of compacted session history".to_string(),
            ),
            schema: history_compaction_response_schema(),
            strict: Some(true),
        },
    }
}

/// Extracts the summary text from the Scribe's structured compaction output.
pub fn parse_compaction_summary(raw: &str) -> Option<String> {
    extract_json_string_field(raw, "summary")
}

/// JSON Schema for the squashed commit message output.
///
/// Squash commit synthesis never produces code changes, so it is a Structured
/// Output operation (see `crates/core/Conventions.md`, section 1.2). The Scribe
/// returns the message as a machine-readable field instead of text recovered by
/// stripping code fences.
pub fn squash_commit_response_schema() -> serde_json::Value {
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
            schema: squash_commit_response_schema(),
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

/// Estimates a token count from a raw byte length.
///
/// The whole engine shares a single heuristic of roughly four bytes per
/// token. Basing the estimate on bytes keeps the on-disk sizing (only known
/// in bytes) and the in-memory sizing consistent, so multi-byte UTF-8
/// (Cyrillic, CJK, emoji) is never counted two to three times heavier by one
/// code path than by another.
pub fn estimate_tokens_from_bytes(byte_len: u64) -> u64 {
    byte_len.div_ceil(4)
}

/// Estimates a token count for an in-memory string.
///
/// Delegates to [`estimate_tokens_from_bytes`] so that string content and
/// file metadata share one consistent heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    estimate_tokens_from_bytes(text.len() as u64) as usize
}

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
    let mut out = String::with_capacity(content.len() + file.path.len() + 64);
    out.push_str(&format!(
        "    <file path=\"{}\" access=\"{}\">\n",
        file.path,
        access_attribute(access)
    ));
    out.push_str(&content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("    </file>\n");
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
    /// Number of files whose rendering the memo currently holds.
    pub fn cached_files(&self) -> usize {
        self.elements.len()
    }

    /// Returns the rendered element of a file, re-rendering it only when needed.
    ///
    /// A file whose effective access and content are unchanged is reproduced
    /// byte for byte; everything else is the only element the round encodes.
    fn element(
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

/// Renders the repository outline section of an Architect prompt.
///
/// The eager symbol map and the compact directory tree are mutually exclusive
/// alternatives: the model reads symbol contours whenever a map is available
/// and falls back to the folder layout only when eager map generation is
/// disabled or the rendered map is empty. The returned section ends with a
/// blank line, so a caller appends it as a single block.
pub(crate) fn format_repository_outline_section(
    repo_map: Option<&str>,
    repo_dirs: Option<&str>,
) -> Option<String> {
    if let Some(contours) = repo_map.map(str::trim).filter(|text| !text.is_empty()) {
        return Some(format!(
            "## Codebase Symbol Contours (<repo_map>):\n{contours}\n\n"
        ));
    }
    let dirs = repo_dirs.map(str::trim).filter(|text| !text.is_empty());
    dirs.map(|tree| format!("## Repository Directories (<repo_dirs>):\n{tree}\n\n"))
}

pub struct PromptAssembly {
    pub repo_state: Option<RepositoryState>,
    pub context_revision: u64,
    pub context_files: Vec<ContextFileContent>,
    pub workflow: String,
    /// Typed wire format of the active protocol.
    ///
    /// The format is a transport fact, so the assembly stores it as a typed
    /// value instead of the protocol's display name. It is rendered in the
    /// project metadata and handed to the contract composer, which keeps a
    /// structured prompt free of control-tag spellings.
    pub edit_protocol: EditProtocolKind,
    /// Interaction records frozen at the start of the turn.
    ///
    /// The records travel structurally rather than as a pre-rendered block: the
    /// semantic layout renders the `<history>` block from them byte for byte,
    /// and the chronological layout interleaves them with the context files by
    /// their insertion watermarks.
    pub history: Option<HistorySnapshot>,
    pub repo_map: Option<String>,
    /// Compact directory-tree outline of the repository.
    ///
    /// Rendered only when the eager global symbol map is disabled, so the model
    /// still learns where subsystems live without a full outline pass.
    pub repo_dirs: Option<String>,
    pub target_language: Option<String>,
    pub active_plan: Option<Plan>,
    /// Declared intent of this turn, which decides the advertised capability set.
    pub intent: TurnIntent,
    pub turn_scratchpad: Option<String>,
    /// Maximum characters of a turn scratchpad block kept before truncation.
    ///
    /// The bound is resolved from the project's `[context]` section, so a turn
    /// never truncates working memory at a compile-time ceiling.
    pub max_scratchpad_chars: usize,
    /// Strategy that decides how a change set addresses the lines it replaces.
    ///
    /// Under [`PatchStrategy::LineAnchor`] every file of the context block is
    /// rendered with the opaque anchor of each line, so the model names a line
    /// range instead of echoing the replaced text.
    pub patch_strategy: PatchStrategy,
    /// Layout policy that composes the message sequence of this turn.
    ///
    /// The policy travels as the typed [`PromptLayoutKind`] of the turn. The
    /// assembly never branches on a policy name: it hands the kind to
    /// [`layout_for`] and receives the interchangeable implementation that
    /// serves it.
    pub prompt_layout: PromptLayoutKind,
    pub project: Option<tauqe_protocol::ProjectConfig>,
}

impl PromptAssembly {
    pub fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
        workflow: impl Into<String>,
        edit_protocol: EditProtocolKind,
        history: Option<HistorySnapshot>,
        repo_map: Option<String>,
    ) -> Self {
        Self {
            repo_state,
            context_revision,
            context_files,
            workflow: workflow.into(),
            edit_protocol,
            history,
            repo_map,
            repo_dirs: None,
            target_language: None,
            active_plan: None,
            intent: TurnIntent::InteractiveRepl,
            turn_scratchpad: None,
            max_scratchpad_chars: crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS,
            patch_strategy: PatchStrategy::default(),
            // A caller that resolved a layout for the turn overwrites this
            // field with the typed kind; until then the assembly keeps the
            // semantic block order, which every intent renders correctly.
            prompt_layout: PromptLayoutKind::Semantic,
            project: None,
        }
    }

    /// Attaches the project command configuration available to this turn.
    pub fn with_project(mut self, project: Option<tauqe_protocol::ProjectConfig>) -> Self {
        self.project = project;
        self
    }

    /// Declares how the rendered context block must address the lines of a file.
    pub fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }

    /// Declares the intent that decides which capabilities this turn advertises.
    pub fn with_intent(mut self, intent: TurnIntent) -> Self {
        self.intent = intent;
        self
    }

    pub fn with_scratchpad(mut self, scratchpad: Option<String>) -> Self {
        self.turn_scratchpad = scratchpad;
        self
    }

    /// Attaches the compact directory outline rendered for this turn.
    pub fn with_repo_dirs(mut self, repo_dirs: Option<String>) -> Self {
        self.repo_dirs = repo_dirs;
        self
    }

    pub fn build_system_prompt(&self, protocol: &dyn PromptProtocol) -> String {
        let capabilities = self.effective_capabilities(protocol);
        let effective_protocol = protocol.with_capability_set(capabilities.clone());
        let mut prompt = String::new();
        prompt.push_str("You are Tauqe AI, an expert programming assistant operating inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n");
        prompt.push_str("TAUQE is the environment you operate within. Whenever the developer mentions TAUQE, they refer to this same control environment—its harness, workflows, context model, plans, and Git transactions—not to an external tool or framework.\n");
        prompt.push_str("Environment behavior is declared in `tauqe.toml` at the repository root: model roles, toolchain verification phases, context and repo-map budgets, pinned files, and workflow policy all come from that file. Treat it as the authoritative project configuration surface, and never infer a documented parameter, budget, or project convention from partial source memory.\n\n");
        let has_eager_repo_map = self.has_eager_repo_map();
        let render_ctx = PromptRenderContext::new(&[], effective_protocol.turn_marker())
            .with_eager_repo_map(has_eager_repo_map)
            .with_project(self.project.as_ref())
            .with_patch_strategy(self.patch_strategy);
        let contract_ctx = ContractContext::new(self.intent, protocol.kind(), render_ctx)
            .with_target_language(self.target_language.as_deref())
            .with_prompt_layout(self.prompt_layout);
        prompt.push_str("## Core System Contract\n");
        prompt.push_str(
            &CapabilityRegistry::global().compose_core_contract_rules(&capabilities, &contract_ctx),
        );
        prompt.push('\n');

        // The capability set narrowed by the declared `TurnIntent` decides which
        // mode the active protocol renders, so the assembly owns no wire-format
        // knowledge: a discussion turn receives exactly the instructions of the
        // concrete protocol whose decoder will parse its output.
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

        prompt.push_str("## Project Metadata\n");
        if let Some(repo) = &self.repo_state {
            prompt.push_str(&format!("- Root: {}\n", repo.root));
            prompt.push_str(&format!("- Branch: {}\n", repo.branch));
            prompt.push_str(&format!("- HEAD: {}\n", repo.head));
            let dirty_str = if repo.dirty {
                "dirty (uncommitted changes present)"
            } else {
                "clean"
            };
            prompt.push_str(&format!("- Status: {}\n", dirty_str));
        }
        prompt.push_str(&format!("- Workflow: {}\n", self.workflow));
        prompt.push_str(&format!("- Edit Protocol: {}\n\n", self.edit_protocol));

        prompt
    }

    /// Renders the contour delta of the declared regime as the leading block of
    /// the trailing user message.
    ///
    /// The system prompt head carries only the invariants every dialogue shares,
    /// so whatever the declared regime alone authorizes is rendered here instead:
    /// the delta rules of its profile list, followed by the delta protocol
    /// instructions of its contour. The block numbers its own rules densely from
    /// one and carries no runtime-varying content, so an unchanged regime renders
    /// identical bytes from one round of a turn to the next.
    ///
    /// An empty delta renders no block at all, so a regime that adds nothing to
    /// the shared head never widens the trailing message with an empty heading.
    pub fn format_regime_delta_section(&self, protocol: &dyn PromptProtocol) -> Option<String> {
        let capabilities = self.effective_capabilities(protocol);
        let effective_protocol = protocol.with_capability_set(capabilities.clone());
        let render_ctx = PromptRenderContext::new(&[], effective_protocol.turn_marker())
            .with_eager_repo_map(self.has_eager_repo_map())
            .with_project(self.project.as_ref())
            .with_patch_strategy(self.patch_strategy);
        let contract_ctx = ContractContext::new(self.intent, protocol.kind(), render_ctx)
            .with_target_language(self.target_language.as_deref())
            .with_prompt_layout(self.prompt_layout);
        let registry = CapabilityRegistry::global();
        let rules = registry.compose_delta_contract_rules(&capabilities, &contract_ctx);
        let instructions = if protocol.kind() == EditProtocolKind::Xml {
            registry.compose_delta_xml_instructions(
                &capabilities,
                &contract_ctx.render,
                self.intent,
            )
        } else {
            registry.compose_delta_structured_protocol_instructions(
                &capabilities,
                &contract_ctx.render,
                self.intent,
            )
        };
        let instructions = instructions.trim_end();
        if rules.trim().is_empty() && instructions.is_empty() {
            return None;
        }
        let mut section = String::from("## Regime Delta\n");
        section.push_str(&rules);
        if !instructions.is_empty() {
            section.push_str(instructions);
            section.push('\n');
        }
        section.push('\n');
        Some(section)
    }

    /// Computes the capability set this turn both advertises and accepts.
    ///
    /// The protocol's own set is narrowed by the declared turn intent, so the
    /// rendered instructions, the structured response schema, the stream filter,
    /// and the decoder all observe one surface: a capability can neither be
    /// advertised without being decodable nor accepted without being advertised.
    pub(crate) fn effective_capabilities(&self, protocol: &dyn PromptProtocol) -> CapabilitySet {
        self.intent.narrow_capabilities(&protocol.capability_set())
    }
    /// Resolves the access a layout renders for `file`.
    /// Resolves the access a layout renders for `file`.
    ///
    /// Every dialogue renders the access layer the file resolves to, and both
    /// layouts read this single predicate, so an element can never disagree with
    /// the layer behind it. No turn intent relabels the layers: a plan discussion
    /// is a dialogue like any other and renders exactly what the REPL renders,
    /// because the developer may request a concrete change there. A file the plan
    /// loaded as editable therefore stays editable instead of being flattened into
    /// a read-only view.
    /// layouts read this single predicate, so an element can never disagree with
    /// the layer behind it. A plan discussion is a dialogue like any other: the
    /// developer may request a concrete change there, so a file the plan loaded
    /// as editable stays editable instead of being relabelled read-only.
    pub(crate) fn context_file_access(&self, file: &ContextFileContent) -> ContextAccess {
        file.access
    }
    /// Renders the authoritative context block of a turn.
    /// Renders the authoritative context block of a turn.
    ///
    /// The block opens with the constant `<context>` tag and lists every file in
    /// context insertion order as `<file path="..." access="...">`. Nothing in
    /// the block depends on the context revision counter or on the layer a file
    /// came from: a newly discovered file appends an element at the end, and
    /// promoting a file from read-only to editable rewrites only its own
    /// attribute. A subsequent round therefore extends the previous context
    /// block instead of rewriting it, which keeps the cached prompt prefix
    /// intact.
    ///
    /// Under [`PatchStrategy::LineAnchor`] every line of every file is prefixed
    /// with its opaque anchor, so an edit names a line range instead of echoing
    /// the text it replaces. A caller that renders several rounds of one turn
    /// hands in a shared [`ContextBlockCache`] through
    /// [`PromptAssembly::format_context_block_with`].
    pub fn format_context_block(&self) -> Option<String> {
        self.format_context_block_with(&mut ContextBlockCache::default())
    }

    /// Renders the context block while reusing the memo of an earlier round.
    ///
    /// A file whose path, effective access, and content are unchanged is
    /// reproduced from the memo byte for byte, so only the files a later round
    /// actually changed or appended are encoded again.
    pub fn format_context_block_with(&self, cache: &mut ContextBlockCache) -> Option<String> {
        if self.context_files.is_empty() {
            return None;
        }

        let mut out = String::new();
        out.push_str("<context>\n");
        for file in &self.context_files {
            let access = self.context_file_access(file);
            out.push_str(cache.element(file, access, self.patch_strategy));
        }
        out.push_str("</context>");
        Some(out)
    }

    /// Returns the interaction records frozen for this turn, if any.
    ///
    /// The records travel structurally rather than as a pre-rendered block, so
    /// the semantic layout renders the `<history>` block from them and the
    /// chronological layout can interleave them with the context elements. An
    /// empty snapshot renders no block at all and is therefore reported as
    /// absent.
    pub fn history_snapshot(&self) -> Option<&HistorySnapshot> {
        self.history
            .as_ref()
            .filter(|snapshot| !snapshot.is_empty())
    }

    /// Reports whether an eager repository symbol map is rendered this turn.
    ///
    /// The predicate matches [`PromptAssembly::format_repo_map_block`] exactly,
    /// so a map that renders no block never claims to be present.
    fn has_eager_repo_map(&self) -> bool {
        self.repo_map
            .as_deref()
            .is_some_and(|map| !map.trim().is_empty())
    }

    pub fn format_repo_map_block(&self) -> Option<String> {
        self.repo_map.as_ref().and_then(|map| {
            let trimmed = map.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("<repo_map>\n{}\n</repo_map>", trimmed))
            }
        })
    }

    pub fn format_repo_dirs_block(&self) -> Option<String> {
        self.repo_dirs.as_ref().and_then(|dirs| {
            let trimmed = dirs.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("<repo_dirs>\n{}\n</repo_dirs>", trimmed))
            }
        })
    }

    /// Renders the repository outline section of a prompt.
    ///
    /// The eager symbol map and the compact directory outline are mutually
    /// exclusive: the model reads symbol contours whenever a map renders and the
    /// folder layout only when the map is absent. The section carries its own
    /// heading and ends with a blank line, so a layout appends it as one block.
    pub(crate) fn format_repository_outline_block(&self) -> Option<String> {
        if let Some(map) = self.format_repo_map_block() {
            return Some(format!("## Repository Map\n{map}\n\n"));
        }
        self.format_repo_dirs_block()
            .map(|dirs| format!("## Repository Directories\n{dirs}\n\n"))
    }

    /// Renders the language directive section of a prompt.
    ///
    /// The directive pins the natural language of every user-facing answer and
    /// keeps the reasoning in English. It is a stable block: the language of a
    /// dialogue changes between turns, not between the rounds of one turn.
    pub(crate) fn format_language_directive_block(&self) -> Option<String> {
        self.target_language.as_ref().map(|target| {
            format!(
                "## Language Directive\n- Target Response Language: {} (all conversational explanations, answers, messages, and plan items/titles/details MUST be in {})\n- Reasoning / Thoughts: Strictly in English.\n\n",
                target, target
            )
        })
    }

    /// Renders the active plan section of a prompt, if the turn may read it.
    ///
    /// An isolated step receives its own instructions and the scope of a single
    /// plan item, so the whole active plan is deliberately withheld from it.
    pub(crate) fn format_active_plan_block(&self) -> Option<String> {
        if self.intent.is_isolated_step() {
            return None;
        }
        let plan = self.active_plan.as_ref()?;
        let block = crate::plan::format_active_plan_context(plan)?;
        Some(format!("## Active Task Plan Context\n{block}\n\n"))
    }

    /// Renders the turn scratchpad section of a prompt.
    ///
    /// The scratchpad is rewritten on every round of a turn, so a layout places
    /// the section in the volatile tail rather than in the stable head.
    pub(crate) fn format_scratchpad_section(&self) -> Option<String> {
        self.format_scratchpad_block()
            .map(|block| format!("## Turn Scratchpad (Working Memory)\n{block}\n\n"))
    }

    /// Renders the compact manifest of the context this turn carries.
    ///
    /// The manifest lists one element per context file with its path and its
    /// effective access and carries no file content. A layout that distributes
    /// the file bodies through a transcript therefore still hands the model a
    /// complete view of what is in context, and the manifest grows at the tail
    /// without invalidating a byte that precedes it.
    pub(crate) fn format_context_manifest_section(&self) -> Option<String> {
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

    pub fn format_scratchpad_block(&self) -> Option<String> {
        self.turn_scratchpad.as_ref().and_then(|pad| {
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

    pub fn assemble_chat_messages(
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

    /// Assembles the messages while reusing the turn-scoped context memo.
    ///
    /// The block order belongs to the active [`layout::PromptLayout`] policy:
    /// the assembly renders no sequence of its own and merely hands itself, the
    /// developer message, and the shared memo to the policy that serves
    /// [`PromptAssembly::prompt_layout`]. The memo stays layout-agnostic, so a
    /// `<file>` element rendered in an earlier round is reproduced byte for
    /// byte and the provider keeps matching its cached prefix.
    pub fn assemble_chat_messages_with_context_cache(
        &self,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
        context_cache: &mut ContextBlockCache,
    ) -> Vec<ChatMessage> {
        layout_for(self.prompt_layout).assemble(self, current_prompt, protocol, context_cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEntry;
    use crate::prompt::xml::XmlPromptProtocol;

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
    fn test_prompt_assembly_with_target_language() {
        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assembly.target_language = Some("Russian".to_string());
        let proto = XmlPromptProtocol;
        let messages = assembly.assemble_chat_messages("Привет", &proto);
        assert!(messages[0].content.contains("## Language Directive"));
        assert!(messages[0]
            .content
            .contains("Target Response Language: Russian"));
        assert!(messages[0]
            .content
            .contains("Reasoning / Thoughts: Strictly in English."));
    }

    #[test]
    fn test_prompt_assembly() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let history = Some(HistorySnapshot::new(vec![HistoryEntry::Summary {
            id: None,
            text: "Initial".to_string(),
        }]));
        let repo_map = Some("crates/core/src/git.rs\n  pub fn get_repository_state()".to_string());
        let assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Xml,
            history,
            repo_map,
        );
        let proto = XmlPromptProtocol;
        let messages = assembly.assemble_chat_messages("Explain this code", &proto);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("Core System Contract"));
        assert!(messages[0].content.contains("src/main.rs"));
        assert!(messages[0].content.contains("fn main() {}"));
        assert!(messages[0].content.contains("## Repository Map"));
        assert!(messages[0].content.contains("<repo_map>"));
        assert!(messages[0].content.contains("crates/core/src/git.rs"));
        assert!(messages[0].content.contains("## Session History"));
        assert!(messages[0].content.contains("<history>"));
        assert!(messages[0]
            .content
            .contains("{\"type\":\"summary\",\"text\":\"Initial\"}"));
        // The regime delta opens the developer message while the task closes
        // it, so every byte of the stable head survives the change.
        let delta = assembly
            .format_regime_delta_section(&proto)
            .expect("the interactive regime states a delta");
        assert_eq!(messages[1].role, "user");
        assert!(messages[1].content.starts_with(&delta));
        assert!(messages[1].content.ends_with("Explain this code"));
    }

    #[test]
    fn test_prompt_assembly_discussion_mode() {
        let files = vec![
            ContextFileContent {
                path: "src/main.rs".to_string(),
                access: ContextAccess::Editable,
                content: "fn main() {}\n".to_string(),
            },
            ContextFileContent {
                path: "src/lib.rs".to_string(),
                access: ContextAccess::ReadOnly,
                content: "pub fn helper() {}\n".to_string(),
            },
        ];
        let mut assembly =
            PromptAssembly::new(None, 1, files, "git", EditProtocolKind::Xml, None, None);
        assembly.intent = TurnIntent::PlanDiscussion;
        let proto = XmlPromptProtocol;
        let messages = assembly.assemble_chat_messages("Let's discuss architecture", &proto);
        let sys = &messages[0].content;

        // A plan discussion is a dialogue like any other: the rule that
        // restricted it to prose is gone, and direct edits are authorized by
        // the very same Action Authorization rule the REPL states.
        assert!(!sys.contains("Discussion Mode Scope"));
        assert!(!sys.contains("architectural discussion"));
        // The authorization rule is regime-specific, so it travels in the
        // contour delta instead of the byte-stable head.
        assert!(!sys.contains("Action Authorization"));
        let delta = assembly
            .format_regime_delta_section(&proto)
            .expect("a dialogue regime states a delta");
        assert!(delta.contains("Action Authorization"));
        assert!(sys.contains("<context>"));
        // A plan discussion is a dialogue like any other: every element carries
        // the layer the file actually resolves to, so an editable file stays
        // editable and a reference file stays read-only instead of the whole
        // context being flattened into a read-only view. The assertion targets
        // the rendered element rather than the whole prompt, because the contract
        // names both attribute values as prompt-side vocabulary.
        assert!(sys.contains("path=\"src/main.rs\" access=\"editable\""));
        assert!(!sys.contains("path=\"src/main.rs\" access=\"read_only\""));
        assert!(sys.contains("path=\"src/lib.rs\" access=\"read_only\""));
        assert!(!sys.contains("path=\"src/lib.rs\" access=\"editable\""));
        assert!(!sys.contains("<editable_files>"));
        assert!(!sys.contains("<read_only_files>"));

        // The answer obligation is owned by the enabled capability modules: a
        // discussion that gathered its material answers in prose instead of
        // closing the turn silently.
        assert!(sys.contains("Never close a turn silently"));

        // The XML protocol renders its own discovery, plan, and language surface
        // for a discussion turn narrowed to the discussion capability profile.
        assert!(sys.contains("<context_request"));
        assert!(sys.contains("<plan action"));
        assert!(sys.contains("<user_language>"));
        // The code-modification channel travels with the discussion, so the XML
        // contour advertises its edit container as well.
        assert!(sys.contains("<edits summary"));

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
        use crate::prompt::protocol::PromptProtocol;
        use tauqe_protocol::{CapabilitySet, LlmCapability, LlmCommand, PatchStrategy};

        /// Protocol stub whose only mode text is its own heading.
        struct StubWireProtocol;

        impl PromptProtocol for StubWireProtocol {
            fn name(&self) -> &'static str {
                "stub-wire"
            }

            fn kind(&self) -> EditProtocolKind {
                EditProtocolKind::Structured
            }

            fn capabilities(&self) -> &[LlmCapability] {
                &[LlmCapability::TextMessage, LlmCapability::UserLanguage]
            }

            fn decode(&self, raw_text: &str, _context_paths: &[String]) -> Vec<LlmCommand> {
                vec![LlmCommand::Text {
                    content: raw_text.to_string(),
                }]
            }

            fn system_instructions(
                &self,
                _editable_paths: &[String],
                _has_eager_repo_map: bool,
                _patch_strategy: PatchStrategy,
            ) -> String {
                "## Mode: Stub Wire\nAnswer in the stub wire format.\n".to_string()
            }

            fn with_capability_set(&self, _capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
                Box::new(StubWireProtocol)
            }

            fn create_stream_filter(
                &self,
                editable_paths: Vec<String>,
                repo_root: std::path::PathBuf,
                staged_contents: std::collections::HashMap<String, String>,
            ) -> Box<dyn crate::edits::stream::EditStreamFilter> {
                Box::new(
                    crate::edits::stream::XmlStreamFilter::new(editable_paths, repo_root)
                        .with_staged_contents(staged_contents),
                )
            }
        }

        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assembly.intent = TurnIntent::PlanDiscussion;
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
        let assembly = {
            let mut assembly =
                PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
            assembly.intent = TurnIntent::PlanDiscussion;
            assembly
        };
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

    #[test]
    fn test_context_block_is_static_and_append_only() {
        let file = |path: &str, access: ContextAccess| ContextFileContent {
            path: path.to_string(),
            access,
            content: format!("// {path}\n"),
        };

        let mut assembly = PromptAssembly::new(
            None,
            7,
            vec![file("src/a.rs", ContextAccess::ReadOnly)],
            "git",
            EditProtocolKind::Xml,
            None,
            None,
        );
        let first = assembly.format_context_block().unwrap();
        assert!(first.starts_with("<context>\n"));
        assert!(!first.contains("revision"));
        assert!(first.contains("path=\"src/a.rs\" access=\"read_only\""));

        // A newly discovered file appends an element and leaves the previous
        // block untouched, and the revision counter never enters the prefix.
        assembly
            .context_files
            .push(file("src/b.rs", ContextAccess::Editable));
        assembly.context_revision = 8;
        let second = assembly.format_context_block().unwrap();
        assert!(second.starts_with(&first[..first.len() - "</context>".len()]));
        assert!(second.contains("path=\"src/b.rs\" access=\"editable\""));

        // Promotion rewrites only the access attribute in place.
        assembly.context_files[0].access = ContextAccess::Editable;
        let promoted = assembly.format_context_block().unwrap();
        assert_eq!(promoted.matches("path=\"src/a.rs\"").count(), 1);
        assert!(promoted.contains("path=\"src/a.rs\" access=\"editable\""));
        assert!(promoted.find("src/a.rs").unwrap() < promoted.find("src/b.rs").unwrap());
    }

    #[test]
    fn test_context_block_renders_opaque_anchors_for_every_file() {
        use crate::edits::anchor::encode_anchor;

        let files = || {
            vec![
                ContextFileContent {
                    path: "src/a.rs".to_string(),
                    access: ContextAccess::ReadOnly,
                    content: "alpha\nbeta\n".to_string(),
                },
                ContextFileContent {
                    path: "src/b.rs".to_string(),
                    access: ContextAccess::Editable,
                    content: "gamma".to_string(),
                },
            ]
        };

        let anchored =
            PromptAssembly::new(None, 1, files(), "git", EditProtocolKind::Xml, None, None)
                .with_patch_strategy(PatchStrategy::LineAnchor);
        let block = anchored.format_context_block().unwrap();

        // Both access layers carry anchors, so a read-only file is addressable
        // exactly like an editable one.
        assert!(block.contains("access=\"read_only\""));
        assert!(block.contains("access=\"editable\""));
        // Every line names its zero-based index through the codec seeded by
        // its own file, so the same line of two files carries unrelated tokens.
        assert!(block.contains(&format!("{} | alpha", encode_anchor("src/a.rs", 0))));
        assert!(block.contains(&format!("{} | beta", encode_anchor("src/a.rs", 1))));
        assert!(block.contains(&format!("{} | gamma", encode_anchor("src/b.rs", 0))));
        assert!(!block.contains("\nalpha"));
        // The closing newline of the file is still rendered exactly once.
        assert!(!block.contains(&format!("{} | beta\n\n", encode_anchor("src/a.rs", 1))));

        // The default strategy keeps the raw lines untouched.
        let plain = PromptAssembly::new(None, 1, files(), "git", EditProtocolKind::Xml, None, None);
        let raw = plain.format_context_block().unwrap();
        assert!(raw.contains("\nalpha\nbeta\n"));
        assert!(!raw.contains(&format!("{} | ", encode_anchor("src/a.rs", 0))));
    }

    #[test]
    fn test_context_cache_reuses_identical_bytes_and_rerenders_only_changed_files() {
        use crate::edits::anchor::encode_anchor;

        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![
                ContextFileContent {
                    path: "src/a.rs".to_string(),
                    access: ContextAccess::ReadOnly,
                    content: "alpha\n".to_string(),
                },
                ContextFileContent {
                    path: "src/b.rs".to_string(),
                    access: ContextAccess::ReadOnly,
                    content: "beta\n".to_string(),
                },
            ],
            "git",
            EditProtocolKind::Xml,
            None,
            None,
        )
        .with_patch_strategy(PatchStrategy::LineAnchor);

        let mut cache = ContextBlockCache::default();
        let first = assembly.format_context_block_with(&mut cache).unwrap();
        assert_eq!(cache.cached_files(), 2);

        // An unchanged round is reproduced byte for byte from the memo.
        let second = assembly.format_context_block_with(&mut cache).unwrap();
        assert_eq!(second, first);
        assert_eq!(cache.cached_files(), 2);

        // Promoting a file rewrites only its own element; its neighbor keeps
        // the exact bytes it already had, so the prefix stays stable.
        let untouched = format!(
            "    <file path=\"src/b.rs\" access=\"read_only\">\n{} | beta\n    </file>\n",
            encode_anchor("src/b.rs", 0)
        );
        assert!(first.contains(&untouched));

        assembly.context_files[0].access = ContextAccess::Editable;
        let promoted = assembly.format_context_block_with(&mut cache).unwrap();
        assert!(promoted.contains("path=\"src/a.rs\" access=\"editable\""));
        assert!(promoted.contains(&format!("{} | alpha", encode_anchor("src/a.rs", 0))));
        assert!(promoted.contains(&untouched));
    }

    #[test]
    fn test_anchored_context_falls_back_to_raw_beyond_the_codec_domain() {
        use crate::edits::anchor::encode_anchor;

        // The codec covers `u16::MAX + 1` lines. A file of exactly that length
        // is still addressable; one line more cannot be addressed at all.
        let within_domain: String = (0..=u16::MAX).map(|i| format!("line {i}\n")).collect();
        let beyond_domain = format!("{within_domain}line beyond\n");

        let render = |content: String| {
            PromptAssembly::new(
                None,
                1,
                vec![ContextFileContent {
                    path: "src/big.rs".to_string(),
                    access: ContextAccess::ReadOnly,
                    content,
                }],
                "git",
                EditProtocolKind::Xml,
                None,
                None,
            )
            .with_patch_strategy(PatchStrategy::LineAnchor)
        };

        let block = render(within_domain).format_context_block().unwrap();
        assert!(block.contains(&format!(
            "{} | line 65535",
            encode_anchor("src/big.rs", u16::MAX)
        )));

        let block = render(beyond_domain).format_context_block().unwrap();
        assert!(block.contains("\nline beyond\n"));
        assert!(!block.contains(&format!("{} | ", encode_anchor("src/big.rs", 0))));
    }

    #[test]
    fn test_stable_prefix_order_and_volatile_tail() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let mut assembly = PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Xml,
            Some(HistorySnapshot::new(vec![HistoryEntry::Summary {
                id: Some(1),
                text: "Earlier work".to_string(),
            }])),
            Some("crate\n  fn main".to_string()),
        );
        assembly.turn_scratchpad = Some("- verified: prefix is stable".to_string());
        assembly.active_plan = Some(Plan {
            id: "prefix-plan".to_string(),
            last_replanned_step: None,
            title: "Prefix Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                ..Default::default()
            }],
        });

        let messages = assembly.assemble_chat_messages("Do work", &XmlPromptProtocol);
        let sys = &messages[0].content;

        let repo_map = sys.find("## Repository Map").expect("repo map block");
        let history = sys.find("## Session History").expect("history block");
        let context = sys.find("## Project Context").expect("context block");
        let plan = sys
            .find("## Active Task Plan Context")
            .expect("active plan block");
        let scratchpad = sys
            .find("## Turn Scratchpad (Working Memory)")
            .expect("scratchpad block");

        assert!(repo_map < context);
        assert!(context < plan);
        // The session history grows after every completed turn, so it is the
        // last stable block: a divergence between two consecutive turns cuts
        // the cached prefix at the latest possible point.
        assert!(plan < history);
        assert!(history < scratchpad);
        assert!(sys.trim_end().ends_with("</scratchpad>"));
        let delta = assembly
            .format_regime_delta_section(&XmlPromptProtocol)
            .expect("the interactive regime states a delta");
        assert!(messages[1].content.starts_with(&delta));
        assert!(messages[1].content.ends_with("Do work"));
    }

    #[test]
    fn test_repository_outline_is_map_or_directories_never_both() {
        let base =
            || PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);

        // The rendered outline block, as opposed to the bare `<repo_dirs>` tag
        // name that the protocol instructions advertise in every turn.
        const DIRS_BLOCK: &str = "<repo_dirs>\ncrate\n  src\n</repo_dirs>";

        // An eager symbol map wins and suppresses the directory outline, even
        // though an outline is available to render.
        let mut with_map = base().with_repo_dirs(Some("crate\n  src".to_string()));
        with_map.repo_map = Some("crate\n  pub fn main".to_string());
        assert!(with_map.format_repo_dirs_block().is_some());
        let messages = with_map.assemble_chat_messages("Go", &XmlPromptProtocol);
        let sys = &messages[0].content;
        assert!(sys.contains("## Repository Map"));
        assert!(!sys.contains("## Repository Directories"));
        assert!(!sys.contains(DIRS_BLOCK));

        // Without a map the compact outline answers where subsystems live.
        let with_dirs = base().with_repo_dirs(Some("crate\n  src".to_string()));
        let messages = with_dirs.assemble_chat_messages("Go", &XmlPromptProtocol);
        let sys = &messages[0].content;
        assert!(!sys.contains("## Repository Map"));
        assert!(sys.contains("## Repository Directories"));
        assert!(sys.contains(DIRS_BLOCK));

        // A blank outline renders no block at all.
        let blank = base().with_repo_dirs(Some("   ".to_string()));
        assert!(blank.format_repo_dirs_block().is_none());

        // With neither input the prefix carries no repository outline.
        let messages = base().assemble_chat_messages("Go", &XmlPromptProtocol);
        let sys = &messages[0].content;
        assert!(!sys.contains("## Repository Map"));
        assert!(!sys.contains("## Repository Directories"));
    }

    #[test]
    fn test_eager_repo_map_flag_tracks_the_rendered_block() {
        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
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
            EditProtocolKind::Xml,
            None,
            Some("crates/core/src/prompt.rs\n  pub fn estimate_tokens".to_string()),
        );
        assert!(assembly.has_eager_repo_map());

        let messages = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        let system = &messages[0].content;

        // The static symbol contours already answer where symbols live, so no
        // rendered instruction may advertise an on-demand map request.
        assert!(system.contains("## Repository Map"));
        assert!(!system.contains("repo_map path="));
        assert!(system.contains("repository symbol map is already rendered"));

        // Discovery still routes through targeted search, directory listings,
        // and full-file loading.
        assert!(system.contains("<repo_search"));
        assert!(system.contains("<dir_list"));
        assert!(system.contains("<context_request"));
    }

    #[test]
    fn test_on_demand_repo_map_instructions_carry_an_explicit_example() {
        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assert!(!assembly.has_eager_repo_map());

        let messages = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        let system = &messages[0].content;

        // Without the rendered outline the model needs an unambiguous tag
        // invocation example, so it requests symbol contours instead of loading
        // whole files.
        assert!(!system.contains("## Repository Map"));
        assert!(system.contains("<repo_map path=\"path/to/file.rs\" />"));
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
        let proto = XmlPromptProtocol;
        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assembly.intent = TurnIntent::IsolatedStep;
        assembly.active_plan = Some(Plan {
            id: "plan-test".to_string(),
            last_replanned_step: None,
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![],
        });

        let sys_prompt = assembly.build_system_prompt(&proto);
        assert!(!sys_prompt.contains("Local Engineering Plans"));
        assert!(!sys_prompt.contains("Step Execution Scope"));
        let delta = assembly
            .format_regime_delta_section(&proto)
            .expect("the step regime states a delta");
        assert!(delta.contains("Step Execution Scope"));
        assert!(!sys_prompt.contains("<plan"));
        assert!(!sys_prompt.contains("<verify"));

        let messages = assembly.assemble_chat_messages("Execute step", &proto);
        assert!(!messages[0].content.contains("## Active Task Plan Context"));
    }

    #[test]
    fn test_planning_prompt_instructions() {
        let proto = XmlPromptProtocol;

        // Develop mode
        let dev_assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let dev_prompt = dev_assembly.build_system_prompt(&proto);
        assert!(dev_prompt.contains("Conceptual Foundation Invariant"));
        assert!(dev_prompt.contains("<concept_item"));
        assert!(dev_prompt.contains("Structured Plan XML Format"));
        assert!(dev_prompt.contains("Leaf-Only Files Invariant"));
        // The planning objective belongs to the declared regime, so it travels
        // in the delta instead of conditioning the shared head.
        assert!(!dev_prompt.contains("Planning Objective: "));
        assert!(dev_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));
        assert!(dev_prompt.contains("File Attributes Contract"));
        assert!(dev_prompt.contains("readonly"));
        let dev_delta = dev_assembly
            .format_regime_delta_section(&proto)
            .expect("the develop regime states a delta");
        assert!(dev_delta.contains(
            "Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What')"
        ));
        assert!(!dev_delta.contains("Step Formulation Contract: "));

        // Discussion mode
        let mut disc_assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        disc_assembly.intent = TurnIntent::PlanDiscussion;
        let disc_prompt = disc_assembly.build_system_prompt(&proto);
        assert!(disc_prompt.contains("Conceptual Foundation Invariant"));
        assert!(disc_prompt.contains("Leaf-Only Files Invariant"));
        assert!(!disc_prompt.contains("Planning Objective: "));
        let disc_delta = disc_assembly
            .format_regime_delta_section(&proto)
            .expect("the plan discussion states a delta");
        assert!(disc_delta.contains(
            "Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What')"
        ));
        assert!(disc_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));
    }

    #[test]
    fn test_prompt_assembly_discovery_instructions() {
        let proto = XmlPromptProtocol;

        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let prompt = assembly.build_system_prompt(&proto);
        assert!(!prompt.contains("<context_drop"));
        assert!(!prompt.contains("Incremental Strategy"));
        assert!(!prompt.contains("Snapshot Strategy"));
        assert!(prompt.contains("<context_request"));
        assert!(prompt.contains("<repo_search"));
        // A tag runs a request only when it is emitted verbatim at the very end
        // of the response, outside markdown code; prose that merely describes
        // an intent performs nothing.
        assert!(prompt.contains("describing the intent in prose performs nothing"));
    }

    #[test]
    fn test_system_prompt_anchors_project_configuration() {
        use tauqe_protocol::LlmCapability;

        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let full = assembly.build_system_prompt(&XmlPromptProtocol);

        // The configuration anchor belongs to the static introduction, and the
        // self-knowledge rule routes configuration questions to that topic.
        assert!(full.contains("tauqe.toml"));
        assert!(full.contains("Self-Knowledge"));
        assert!(full.contains("topic=\"config\""));

        // With documentation requests disabled the anchor survives as prose,
        // because it advertises no control tag the decoder would reject.
        let text_only = XmlPromptProtocol::with_capabilities(CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
        ]));
        let narrowed = assembly.build_system_prompt(&*text_only);
        assert!(narrowed.contains("tauqe.toml"));
        assert!(!narrowed.contains("Self-Knowledge"));
        assert!(!narrowed.contains("<doc_request"));

        // Discussion mode renders the same authoritative anchor.
        let mut discussion =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        discussion.intent = TurnIntent::PlanDiscussion;
        let prompt = discussion.build_system_prompt(&XmlPromptProtocol);
        assert!(prompt.contains("tauqe.toml"));
    }

    #[test]
    fn test_prompt_assembly_with_turn_scratchpad() {
        let proto = XmlPromptProtocol;
        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad = Some("   ".to_string());
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad =
            Some("- Hypothesis: Foo needs Bar\n- Status: Inspecting bar.rs".to_string());
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("<scratchpad>"));
        assert!(block.contains("- Hypothesis: Foo needs Bar"));
        assert!(block.contains("</scratchpad>"));

        let messages = assembly.assemble_chat_messages("Continue", &proto);
        assert!(messages[0]
            .content
            .contains("## Turn Scratchpad (Working Memory)"));
        assert!(messages[0].content.contains("- Hypothesis: Foo needs Bar"));
    }

    #[test]
    fn test_turn_scratchpad_truncation_utf8_safety() {
        let multi_byte_text = "Рабочая гипотеза и детали проверки UTF-8 символов. ".repeat(200);
        assert!(multi_byte_text.len() > crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS);

        let mut assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        assembly.turn_scratchpad = Some(multi_byte_text);
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("... [scratchpad truncated for length] ..."));
    }

    #[test]
    fn test_narrowed_capabilities_hide_disabled_instructions() {
        use crate::prompt::protocol::PromptProtocol;
        use tauqe_protocol::{LlmCapability, LlmCommand};

        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let narrowed = XmlPromptProtocol::with_capabilities(CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
        ]));
        let prompt = assembly.build_system_prompt(&*narrowed);

        assert!(prompt.contains("<edits summary"));
        assert!(!prompt.contains("<context_request>"));
        assert!(!prompt.contains("<verify"));
        assert!(!prompt.contains("<scratchpad"));
        assert!(!prompt.contains("<success"));
        assert!(!prompt.contains("Self-Knowledge"));
        assert!(!prompt.contains("Turn Scratchpad"));

        // The same set narrows decoding, so a disabled capability never reaches
        // the execution pipeline.
        let raw =
            "Working.\n<context_request path=\"src/a.rs\" />\n<scratchpad>notes</scratchpad>\n";
        let full = XmlPromptProtocol.decode(raw, &[]);
        assert!(full
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest { .. })));
        let filtered = narrowed.decode(raw, &[]);
        assert!(!filtered
            .iter()
            .any(|command| matches!(command, LlmCommand::ContextRequest { .. })));
        assert!(!filtered
            .iter()
            .any(|command| matches!(command, LlmCommand::Scratchpad { .. })));
    }

    #[test]
    fn test_prompt_assembly_renders_protocol_instructions() {
        use crate::prompt::json::JsonPromptProtocol;

        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let assembly =
            PromptAssembly::new(None, 1, files, "git", EditProtocolKind::Xml, None, None);

        // Each protocol renders its own edit reference directly instead of being
        // reconstructed from its name.
        let xml = XmlPromptProtocol;
        let xml_prompt = assembly.build_system_prompt(&xml);
        assert!(xml_prompt.contains("Code Modification Protocol (XML Edits)"));

        let json = JsonPromptProtocol::default();
        let json_prompt = assembly.build_system_prompt(&json);
        assert!(json_prompt.contains("Code Modification Protocol (Structured Output JSON)"));

        // Chat assembly yields system + user messages.
        let messages = assembly.assemble_chat_messages("Explain this code", &xml);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert!(messages[1].content.ends_with("Explain this code"));
    }

    #[test]
    fn test_prompt_assembly_preserves_turn_marker() {
        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let marked = XmlPromptProtocol
            .with_turn_marker("M12345")
            .expect("marker-bound protocol");

        let prompt = assembly.build_system_prompt(&*marked);
        assert!(prompt.contains("<scratchpad_M12345>"));
        assert!(prompt.contains("</scratchpad_M12345>"));
        assert!(prompt.contains("<plan_M12345"));
        assert!(prompt.contains("<concept_M12345>"));
        assert!(prompt.contains("<concept_item_M12345"));
        assert!(prompt.contains("<context_request_M12345>"));

        let messages = assembly.assemble_chat_messages("Go", &*marked);
        assert!(messages[0].content.contains("<scratchpad_M12345>"));
    }

    #[test]
    fn test_prompt_assembly_requires_an_explicit_request_for_code_changes() {
        let proto = XmlPromptProtocol;

        // Interactive develop turn: changes require an explicit user request or clear unambiguous task.
        let assembly =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        let prompt = assembly.build_system_prompt(&proto);
        // The head carries the intent-independent trunk only, so the rule that
        // authorizes an edit travels in the contour delta.
        assert!(!prompt.contains("Action Authorization"));
        let delta = assembly
            .format_regime_delta_section(&proto)
            .expect("the interactive regime states a delta");
        assert!(delta.contains("Action Authorization"));
        assert!(delta.contains("Default to discussion"));
        assert!(delta.contains("when the required change is clear"));

        // An isolated step turn is itself the explicit instruction, so the rule
        // is deliberately omitted there instead of contradicting the step scope.
        let mut isolated =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        isolated.intent = TurnIntent::IsolatedStep;
        let isolated_prompt = isolated.build_system_prompt(&proto);
        assert!(!isolated_prompt.contains("Action Authorization"));
        let isolated_delta = isolated
            .format_regime_delta_section(&proto)
            .expect("the step regime states its own delta");
        assert!(!isolated_delta.contains("Action Authorization"));

        // A plan discussion carries the whole dialogue surface, so the very same
        // authorization rule guards it: a change there is permitted only when
        // the developer states the request directly, exactly as in the REPL.
        let mut discussion =
            PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
        discussion.intent = TurnIntent::PlanDiscussion;
        let discussion_prompt = discussion.build_system_prompt(&proto);
        assert!(!discussion_prompt.contains("Action Authorization"));
        let discussion_delta = discussion
            .format_regime_delta_section(&proto)
            .expect("the plan discussion states a delta");
        assert!(discussion_delta.contains("Action Authorization"));
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
        let proto = XmlPromptProtocol;
        for intent in [
            TurnIntent::InteractiveRepl,
            TurnIntent::PlanDiscussion,
            TurnIntent::IsolatedStep,
        ] {
            let mut assembly =
                PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
            assembly.intent = intent;
            let prompt = assembly.build_system_prompt(&proto);
            let section = contract_section(&prompt);
            assert!(!section.is_empty(), "missing contract for {intent:?}");

            let numbers = contract_rule_numbers(section);
            assert!(!numbers.is_empty(), "no rules rendered for {intent:?}");
            let expected: Vec<usize> = (1..=numbers.len()).collect();
            assert_eq!(numbers, expected, "non-monotonic numbering for {intent:?}");
        }
    }

    #[test]
    fn test_core_system_contract_has_no_lettered_surrogates() {
        let proto = XmlPromptProtocol;
        for intent in [
            TurnIntent::InteractiveRepl,
            TurnIntent::PlanDiscussion,
            TurnIntent::IsolatedStep,
        ] {
            let mut assembly =
                PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None);
            assembly.intent = intent;
            let prompt = assembly.build_system_prompt(&proto);
            let section = contract_section(&prompt);
            for surrogate in ["1a.", "2a.", "4a.", "8a.", "10a.", "11a."] {
                assert!(
                    !section.contains(surrogate),
                    "surrogate {surrogate} present for {intent:?}"
                );
            }
        }
    }

    #[test]
    fn test_transport_never_changes_the_capability_surface_of_a_turn() {
        use crate::prompt::json::JsonPromptProtocol;

        // Both transports advertise one intent-oriented surface, so switching
        // the wire format changes the encoding only.
        assert_eq!(
            XmlPromptProtocol.capability_set(),
            JsonPromptProtocol::edits().capability_set()
        );

        // Whatever the declared intent, the narrowed surface a turn renders and
        // decodes is identical for both transports.
        for intent in [
            TurnIntent::InteractiveRepl,
            TurnIntent::PlanDiscussion,
            TurnIntent::IsolatedStep,
        ] {
            let assembly =
                PromptAssembly::new(None, 1, vec![], "git", EditProtocolKind::Xml, None, None)
                    .with_intent(intent);
            let xml = assembly.effective_capabilities(&XmlPromptProtocol);
            let structured = assembly.effective_capabilities(&JsonPromptProtocol::edits());
            assert_eq!(xml, structured, "surface diverged for {intent:?}");
        }
    }
}
