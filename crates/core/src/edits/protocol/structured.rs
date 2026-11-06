use std::borrow::Cow;

use serde::Deserialize;
use tauqe_protocol::discussion::normalize_repo_map_path;
use tauqe_protocol::{
    CapabilitySet, DirListRequest, DocTopic, EditFileOp, EditOperation, EditOutcome, LlmCapability,
    ModelResult, ModelResultProposal, PatchStrategy, StructuredChangeProposal,
};

use crate::capabilities::{
    abort_invocations, custom_tool_invocations, CapabilityRegistry, PromptRenderContext,
};
use crate::edits::protocol::xml::engine::{self, strip_all_control_tags};
use crate::edits::protocol::xml::CodeFences;
use crate::edits::strategy::StrategyProfile;
use crate::edits::stream::json::decode::{balanced_object_end, strip_code_fence};
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

use super::utils::{normalize_content, resolve_target_path_for_op};
use super::{ContextRequest, TerminalOutcome, VerifyOnSuccess, VerifyRequest, VerifyTarget};

/// Structured Output JSON-schema edit protocol
#[derive(Debug, Default, Clone)]
pub struct StructuredEditProtocol;

impl StructuredEditProtocol {
    /// Returns the edit schema of the interactive REPL capability surface.
    ///
    /// The default surface is intent-oriented, never transport-specific, so the
    /// structured contour advertises exactly what the XML contour advertises.
    pub fn schema(ctx: &PromptRenderContext<'_>) -> serde_json::Value {
        Self::schema_for(&CapabilitySet::interactive_repl(), ctx)
    }

    /// Builds the edit schema for exactly the enabled capabilities.
    ///
    /// The registry composes the schema from the same capability modules that
    /// render the prompt instructions and scope the decoder, so a disabled
    /// capability contributes neither an instruction nor a schema field. A
    /// property therefore cannot drift from the rules that describe it, and the
    /// strict `required` list is derived from the emitted properties, so a
    /// pruned property is never left required.
    ///
    /// The render context carries the patch strategy and the project command
    /// declaration the composed schema must observe, so a caller that renders
    /// the prompt and composes the schema hands over one identical value.
    pub fn schema_for(
        capabilities: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
    ) -> serde_json::Value {
        CapabilityRegistry::global().compose_structured_schema(capabilities, ctx)
    }

    fn proposal_to_model_result(
        &self,
        proposal: ModelResultProposal,
        editable_paths: &[String],
    ) -> ModelResult {
        if proposal.changes.is_empty() {
            return ModelResult::Answer {
                text: proposal.message,
            };
        }

        let edits = match change_to_operations(&proposal.changes, editable_paths) {
            Ok(edits) => edits,
            Err(message) => {
                return ModelResult::Edit {
                    summary: format!("Invalid change: {}", message),
                    edits: Vec::new(),
                    proposal: Some(proposal),
                    outcome: EditOutcome {
                        applied: false,
                        error: Some(message),
                        changed_files: Vec::new(),
                        commit_hash: None,
                        verification: None,
                    },
                };
            }
        };

        let summary = if !proposal.message.trim().is_empty() {
            let first_line = proposal
                .message
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Applied changes")
                .trim();
            if first_line.chars().count() > 100 {
                let truncated: String = first_line.chars().take(97).collect();
                format!("{}...", truncated)
            } else {
                first_line.to_string()
            }
        } else if edits.len() == 1 {
            match &edits[0] {
                EditOperation::Replace { path, .. } => format!("Update {}", path),
                EditOperation::Create { path, .. } => format!("Create {}", path),
                EditOperation::Delete { path } => format!("Delete {}", path),
                EditOperation::Move { from, to } => format!("Move {} to {}", from, to),
                EditOperation::MoveDir { from, to } => {
                    format!("Move directory {} to {}", from, to)
                }
                EditOperation::Overwrite { path, .. } => format!("Overwrite {}", path),
                EditOperation::LineAnchor { path, .. } => {
                    format!("Replace anchored range in {}", path)
                }
            }
        } else {
            format!("Apply {} structured edits", edits.len())
        };

        ModelResult::Edit {
            summary,
            edits,
            proposal: Some(proposal),
            outcome: EditOutcome {
                applied: false,
                error: None,
                changed_files: Vec::new(),
                commit_hash: None,
                verification: None,
            },
        }
    }

    /// Returns system prompt instructions for the structured-output protocol.
    pub fn system_instructions(
        &self,
        editable_paths: &[String],
        patch_strategy: PatchStrategy,
    ) -> String {
        self.system_instructions_for(
            &CapabilitySet::interactive_repl(),
            editable_paths,
            patch_strategy,
        )
    }

    /// Returns system prompt instructions for an explicit capability set.
    ///
    /// The field reference is composed from the capability registry, so a
    /// narrowed set never advertises a field for a disabled capability.
    pub fn system_instructions_for(
        &self,
        capabilities: &CapabilitySet,
        editable_paths: &[String],
        patch_strategy: PatchStrategy,
    ) -> String {
        let ctx =
            PromptRenderContext::new(editable_paths, None).with_patch_strategy(patch_strategy);
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Structured Output JSON)\n");
        prompt.push_str(
            "Respond with a single JSON object conforming to the structured output schema:\n",
        );
        prompt.push_str(
            &CapabilityRegistry::global()
                .compose_structured_protocol_instructions(capabilities, &ctx),
        );
        prompt.push_str("\n\n");

        if !capabilities.contains(LlmCapability::FileEdits) {
            return prompt;
        }

        // Every strategy-specific sentence of the contract comes from the
        // profile, so the instructions and the declared schema describe the same
        // fields.
        let profile = StrategyProfile::of(patch_strategy);
        prompt.push_str(profile.empty_field_guidance);

        prompt.push_str("Files inside <context> declare their status through a declarative `access` attribute: `access=\"editable\"` files may be modified directly, while a file marked `access=\"read_only\"` is provided for reference and becomes editable automatically once you propose a change to it. ");
        // The reachability of a file absent from <context> depends on whether
        // the discovery capability is enabled: only a set that advertises
        // `context_requests` in the schema may be told to use it.
        if capabilities.contains(LlmCapability::Research) {
            prompt.push_str("A file absent from <context> is reached through `context_requests` or created with `op: 'create'`.\n\n");
        } else {
            prompt
                .push_str("A file absent from <context> must be created with `op: 'create'`.\n\n");
        }

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Output MUST be valid JSON adhering to the specified schema.\n");
        prompt.push_str("2. For 'replace', `path` MUST match a file present in <context>.\n");
        prompt.push_str("3. ");
        prompt.push_str(profile.replace_invariant);
        prompt.push_str("4. For 'create' and 'overwrite', `content` must contain the complete file content from first to last line.\n");
        prompt.push_str("5. If no code changes are needed, set `changes: []` and provide your answer in `message`.\n");

        prompt
    }

    /// Returns the JSON-schema response format used to constrain generation.
    pub fn response_format(&self, ctx: &PromptRenderContext<'_>) -> Option<ResponseFormat> {
        self.response_format_for(&CapabilitySet::interactive_repl(), ctx)
    }

    /// Returns the JSON-schema response format for an explicit capability set,
    /// so the wire schema matches the advertised capability surface.
    /// The render context is handed to the composed schema unchanged, so the
    /// format a caller constrains generation with is the very projection the
    /// rendered instruction described.
    pub fn response_format_for(
        &self,
        capabilities: &CapabilitySet,
        ctx: &PromptRenderContext<'_>,
    ) -> Option<ResponseFormat> {
        Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "tauqe_edit_proposal".to_string(),
                description: Some("Structured code modification proposal and response".to_string()),
                schema: Cow::Owned(Self::schema_for(capabilities, ctx)),
                strict: Some(true),
            },
        })
    }

    /// Extracts context requests from a structured proposal.
    pub fn parse_context_requests(&self, raw_text: &str) -> Vec<ContextRequest> {
        StructuredPayload::parse(raw_text).context_requests()
    }

    /// Extracts repository search requests from a structured proposal or fallback tags.
    pub fn parse_repo_searches(&self, raw_text: &str) -> Vec<tauqe_protocol::RepoSearchRequest> {
        StructuredPayload::parse(raw_text).repo_searches()
    }

    /// Extracts targeted repository map requests from a structured proposal,
    /// falling back to XML control tags when the JSON field is absent.
    ///
    /// Every entry is normalized through the shared `normalize_repo_map_path`
    /// rule, so an empty string or `.` names the whole repository while a
    /// traversal path is dropped instead of escaping it.
    pub fn parse_repo_maps(&self, raw_text: &str) -> Vec<String> {
        StructuredPayload::parse(raw_text).repo_maps()
    }

    /// Reports whether the model requested the compact repository directory
    /// tree, falling back to XML control tags when the JSON field is absent or
    /// reports no request.
    pub fn parse_repo_dirs(&self, raw_text: &str) -> bool {
        StructuredPayload::parse(raw_text).repo_dirs()
    }

    /// Extracts directory content listing requests from the structured
    /// `dir_lists` field, falling back to `<dir_list>` control tags when the
    /// JSON field is absent or empty.
    ///
    /// Every entry is normalized through [`DirListRequest::normalized`], so `.`
    /// and `./` collapse to the repository root while a path that escapes the
    /// repository is dropped instead of resolved. A missing `recursive` flag
    /// keeps the listing shallow, so a malformed entry narrows the result
    /// rather than failing it.
    pub fn parse_dir_lists(&self, raw_text: &str) -> Vec<DirListRequest> {
        StructuredPayload::parse(raw_text).dir_lists()
    }

    /// Extracts requested documentation topics from the canonical
    /// `tauqe_doc_requests` field, falling back to the pre-canonical
    /// `doc_requests` spelling and then to XML control tags.
    ///
    /// The canonical field is the one the strict response schema advertises, and
    /// the pre-canonical spelling stays a permanent alias, so a cached response
    /// written before the rename keeps decoding. Both vehicles carry the same
    /// payload, so a response never has to choose a spelling to be understood.
    ///
    /// A `null` element is a partial emission of one topic, so it yields the
    /// tolerant `DocTopic::All` fallback the DTO boundary resolves it to, while
    /// an element of any other non-string shape is skipped.
    pub fn parse_doc_requests(&self, raw_text: &str) -> Vec<DocTopic> {
        StructuredPayload::parse(raw_text).doc_requests()
    }

    /// Extracts requested external documents from the structured
    /// `outer_doc_requests` field, falling back to `<outer_doc_request>` control
    /// tags when the JSON field is absent or empty.
    ///
    /// Each entry names one document the project declares in `[[outer_docs]]`
    /// rather than a cache path, so where the materialized copy lives stays a
    /// harness detail. A blank identifier grants nothing and is dropped, exactly
    /// as the tag form drops one, and repetition is preserved because
    /// materializing a declared document twice is decided by the phase that owns
    /// the round.
    pub fn parse_outer_doc_requests(&self, raw_text: &str) -> Vec<String> {
        StructuredPayload::parse(raw_text).outer_doc_requests()
    }

    /// Extracts ephemeral scratchpad notes from a structured proposal,
    /// falling back to XML control tags when the JSON field is absent.
    pub fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        StructuredPayload::parse(raw_text).scratchpad()
    }

    /// Extracts the terminal outcome from a structured proposal, falling back
    /// to XML control tags when the JSON field is absent or reports `none`.
    ///
    /// A `<blocked>` tag is itself a terminal outcome: a broken precondition
    /// ends the turn exactly like success or error, so the fallback surfaces it
    /// as [`TerminalOutcome::Blocked`].
    pub fn parse_terminal_outcome(&self, raw_text: &str) -> Option<TerminalOutcome> {
        StructuredPayload::parse(raw_text).terminal_outcome()
    }

    /// Extracts a semantic blocker from a structured proposal, falling back to
    /// XML control tags when the JSON field is absent or reports no blocker.
    pub fn parse_blocked(&self, raw_text: &str) -> Option<(String, Option<String>)> {
        StructuredPayload::parse(raw_text).blocked()
    }

    /// Extracts a semantic abort from a structured proposal, falling back to
    /// XML control tags when the JSON field is absent or reports no abort.
    ///
    /// An abort is not a blocker: it reports work the harness cannot perform at
    /// all, so the payload carries both the concrete missing capability
    /// (`reason`) and the exact command the developer must run
    /// (`recommendation`). A report that states neither keeps a deterministic
    /// default instead of stopping the turn without a cause.
    pub fn parse_abort(&self, raw_text: &str) -> Option<(String, String)> {
        StructuredPayload::parse(raw_text).abort()
    }

    /// Extracts typed project command invocations from the structured
    /// `custom_tools` field, falling back to `<custom_tool>` control tags.
    ///
    /// Each invocation names one command from the declared allowlist and carries
    /// the typed argument object the declaration validates later in the turn.
    /// Whether the name and its arguments are actually declared is decided by
    /// the harness at execution time, so decoding never rejects an invocation
    /// the project itself may accept.
    pub fn parse_custom_tools(&self, raw_text: &str) -> Vec<(String, serde_json::Value)> {
        StructuredPayload::parse(raw_text).custom_tools()
    }

    /// Extracts a verification request from the structured `verify` field,
    /// falling back to XML control tags when the JSON field is absent or
    /// reports `none`.
    pub fn parse_verify_request(&self, raw_text: &str) -> Option<VerifyRequest> {
        StructuredPayload::parse(raw_text).verify_request()
    }

    /// Extracts the detected user language from the structured `user_language`
    /// field, falling back to XML control tags when the field is absent or
    /// blank.
    pub fn parse_user_language(&self, raw_text: &str) -> Option<String> {
        StructuredPayload::parse(raw_text).user_language()
    }

    /// Extracts plan creation or modification payloads from the structured
    /// `plan_update` field, falling back to `<plan>` control tags when the field
    /// is absent.
    ///
    /// A `null` payload is the idle representation the strict schema declares
    /// for a turn that leaves the plan untouched, so it yields no update. A
    /// payload that cannot be deserialized into
    /// [`tauqe_protocol::DiscussionPlanUpdate`] is reported as an error instead
    /// of being silently swallowed: the caller turns it into a `ProtocolError`
    /// command, which lets the bounded turn repeat generation with the decoding
    /// diagnostics attached. Such a payload never falls back to control tags,
    /// because a silent fallback would hide the very defect the retry has to
    /// repair.
    pub fn parse_plan_updates(
        &self,
        raw_text: &str,
    ) -> Result<Vec<tauqe_protocol::DiscussionPlanUpdate>, String> {
        StructuredPayload::parse(raw_text).plan_updates()
    }

    /// Extracts a decision recorded for one review finding.
    ///
    /// The decision is a dialogue mutation rather than a code change: it states
    /// the status the engineer settled on for one finding together with the
    /// reasoning behind it. The field is declared nullable, so the idle form
    /// records no decision. A payload that cannot be deserialized into
    /// [`tauqe_protocol::review::ReviewUpdateProposal`] is reported as an error
    /// instead of being silently swallowed, so the bounded turn can repeat
    /// generation with the decoding diagnostics attached.
    pub fn parse_review_update(
        &self,
        raw_text: &str,
    ) -> Result<Option<tauqe_protocol::review::ReviewUpdateProposal>, String> {
        StructuredPayload::parse(raw_text).review_update()
    }

    /// Cleans model output for presentation and history.
    pub fn clean_assistant_text(&self, raw_text: &str) -> String {
        StructuredPayload::parse(raw_text).clean_text()
    }

    /// Parses raw model text into a `ModelResult`.
    pub fn parse_output(&self, raw_text: &str, editable_paths: &[String]) -> ModelResult {
        let payload = StructuredPayload::parse(raw_text);
        if let Some(proposal) = payload.into_proposal() {
            return self.proposal_to_model_result(proposal, editable_paths);
        }

        if super::xml::has_xml_edit_tags(raw_text) {
            return super::xml::XmlEditProtocol.parse_output(raw_text, editable_paths);
        }

        ModelResult::Answer {
            text: raw_text.to_string(),
        }
    }
}

/// Locates and decodes the single JSON object a structured response carries.
///
/// A response may be the bare object, an object inside a markdown code fence, or
/// an object quoted in wrapper prose, so the text is stripped of its fence and
/// walked once for the first balanced object that actually deserializes. Every
/// offset the walk yields follows an ASCII delimiter, so neither the scan nor the
/// slice it produces can split a multi-byte character.
///
/// The flag reports whether that object is the whole response rather than a
/// fragment quoted inside prose, which decides whether a response with no
/// conversational channel collapses to nothing or keeps its prose.
fn decode_response_object(raw: &str) -> Option<(serde_json::Value, bool)> {
    let body = strip_code_fence(raw);
    let mut from = 0;
    while let Some(relative) = body.get(from..).and_then(|rest| rest.find('{')) {
        let start = from + relative;
        if let Some(end) = balanced_object_end(body, start) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..end]) {
                return Some((value, start == 0 && end == body.len()));
            }
        }
        from = start + 1;
    }
    None
}

/// Collects the conversational channel of every JSON object the response carries.
///
/// A structured turn concatenates the rounds it received, so one payload may
/// carry several responses, and wrapper prose may quote an object a model
/// explained. Each object's `message` is read in document order, and a blank
/// channel is dropped so a silent round adds no separator of its own. A decoded
/// object owns the braces inside it, so the walk resumes after the object
/// instead of descending into its payload.
fn collect_json_messages(raw: &str) -> Vec<String> {
    let body = strip_code_fence(raw);
    let mut messages = Vec::new();
    let mut from = 0;
    while let Some(relative) = body.get(from..).and_then(|rest| rest.find('{')) {
        let start = from + relative;
        let Some(end) = balanced_object_end(body, start) else {
            break;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..end]) {
            let channel = value.get("message").and_then(|field| field.as_str());
            if let Some(message) = channel {
                let trimmed = message.trim();
                if !trimmed.is_empty() {
                    messages.push(trimmed.to_string());
                }
            }
            from = end;
        } else {
            from = start + 1;
        }
    }
    messages
}

/// Validates every declared change against the files the turn may address.
fn change_to_operations(
    changes: &[StructuredChangeProposal],
    editable_paths: &[String],
) -> Result<Vec<EditOperation>, String> {
    changes
        .iter()
        .map(|change| change_to_operation(change, editable_paths))
        .collect()
}

/// Decodes the XML change set of a response that carries no structured one.
///
/// The structured contour answers in JSON, so an XML change set is the one case
/// a response written in the other transport is still applied instead of being
/// silently ignored. Its own rejection travels through the same channel.
fn xml_change_set(raw: &str, editable_paths: &[String]) -> Result<Vec<EditOperation>, String> {
    if !super::xml::has_xml_edit_tags(raw) {
        return Ok(Vec::new());
    }
    match super::xml::XmlEditProtocol.parse_output(raw, editable_paths) {
        ModelResult::Edit { edits, outcome, .. } => match outcome.error {
            Some(message) => Err(message),
            None => Ok(edits),
        },
        ModelResult::Answer { .. } => Ok(Vec::new()),
    }
}

/// One decoded structured response.
///
/// A structured round answers with a single JSON object, and every control field
/// of that answer is read from this one decoding: the text is stripped of its code
/// fence and walked for its object once, the object is decoded once, and the typed
/// proposal is deserialized from that decoded object instead of from the text
/// again. The cost of a round therefore no longer multiplies by the number of
/// control fields the answer carries, which a research, patch-retry, or
/// verification-healing cycle repeats many times inside one bounded turn.
///
/// A reader borrows what it needs, so one payload serves the conversational
/// channel, the discovery requests, the project tool invocations, the terminal
/// outcome, and the change set. [`StructuredPayload::change_set`] validates the
/// change set without consuming the payload; only the transport's own
/// [`StructuredEditProtocol::parse_output`] takes the proposal away.
pub struct StructuredPayload<'a> {
    /// The response text the payload was decoded from.
    raw: &'a str,
    /// The decoded JSON object, when the response carries one.
    ///
    /// The typed proposal does not model every control field, so verification,
    /// detected language, plan updates, review decisions, project tool
    /// invocations, and the external document identifiers are read straight from
    /// this object.
    object: Option<serde_json::Value>,
    /// The typed proposal deserialized from `object`.
    proposal: Option<ModelResultProposal>,
    /// Whether `object` is the whole response rather than an object quoted inside
    /// wrapper prose.
    whole_object: bool,
}

impl<'a> StructuredPayload<'a> {
    /// Decodes one structured response.
    pub fn parse(raw_text: &'a str) -> Self {
        let (object, whole_object) = match decode_response_object(raw_text) {
            Some((value, whole_object)) => (Some(value), whole_object),
            None => (None, false),
        };
        // The proposal is deserialized from the decoded object rather than from
        // the text, so the response is lexed once no matter how many of its
        // fields the turn reads.
        let proposal = object
            .as_ref()
            .and_then(|value| ModelResultProposal::deserialize(value).ok());
        Self {
            raw: raw_text,
            object,
            proposal,
            whole_object,
        }
    }

    /// The typed proposal the response decoded into.
    ///
    /// Reading it takes ownership of the payload, because the change-set result
    /// of a turn carries the proposal itself.
    fn into_proposal(self) -> Option<ModelResultProposal> {
        self.proposal
    }

    /// The named field of the decoded object.
    fn field(&self, name: &str) -> Option<&serde_json::Value> {
        self.object.as_ref().and_then(|value| value.get(name))
    }

    /// The conversational channel of the decoded object.
    ///
    /// A response whose whole body is one object states its answer in `message`,
    /// and so does an object quoted inside wrapper prose: the message is the
    /// answer the round produced, while the prose around it is wrapper text.
    fn message(&self) -> Option<&str> {
        let message = self.field("message")?.as_str()?.trim();
        if message.is_empty() {
            return None;
        }
        Some(message)
    }

    /// Cleans the response for presentation and history.
    ///
    /// Every control tag is removed by the single parametric sanitizer, and a
    /// payload that is itself one structured response collapses to its `message`
    /// channel, so neither XML control tags nor JSON scaffolding can leak into
    /// conversational text, session history, or the streamed interface. A
    /// payload that is not one response keeps every channel it carries, in
    /// document order, so the rounds a turn concatenated and the answer a model
    /// wrapped in prose both survive cleaning.
    pub fn clean_text(&self) -> String {
        // A payload that is itself one structured response carries its whole
        // conversational channel in `message`, and the object around it is
        // protocol chrome, never an answer: collapsing the payload keeps raw
        // wire markup out of session history and matches what the streaming
        // filter showed the interface.
        if self.whole_object {
            return self
                .message()
                .map(strip_all_control_tags)
                .unwrap_or_default();
        }
        // Such a payload may still carry answers: a turn concatenates the
        // rounds it received, and wrapper prose surrounds an object a model
        // quoted. Every channel is joined in document order, so cleaning drops
        // no answer the response produced.
        let messages = collect_json_messages(self.raw);
        if !messages.is_empty() {
            return strip_all_control_tags(&messages.join("\n\n"));
        }
        if super::xml::has_xml_edit_tags(self.raw) {
            return super::xml::extract_conversational_text(self.raw);
        }
        strip_all_control_tags(self.raw)
    }

    /// Validates the declared change set against the files the turn may address.
    ///
    /// An answer that declares no change is valid: a round that only reports,
    /// asks for context, or records a decision modifies no file.
    pub fn change_set(&self, editable_paths: &[String]) -> Result<Vec<EditOperation>, String> {
        let Some(proposal) = self.proposal.as_ref() else {
            return xml_change_set(self.raw, editable_paths);
        };
        change_to_operations(&proposal.changes, editable_paths)
    }

    /// File context requests decoded from the `context_requests` field.
    pub fn context_requests(&self) -> Vec<ContextRequest> {
        self.proposal
            .as_ref()
            .map(|proposal| {
                proposal
                    .context_requests
                    .iter()
                    .map(|request| ContextRequest {
                        path: request.path.clone(),
                        access: request.access,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Repository search requests decoded from the `repo_searches` field,
    /// falling back to `<repo_search>` control tags.
    pub fn repo_searches(&self) -> Vec<tauqe_protocol::RepoSearchRequest> {
        if let Some(proposal) = self.proposal.as_ref() {
            if !proposal.repo_searches.is_empty() {
                return proposal.repo_searches.clone();
            }
        }
        super::xml::tags::parse_repo_search_tags(self.raw)
    }

    /// Targeted repository map paths decoded from the `repo_maps` field,
    /// falling back to `<repo_map>` control tags.
    ///
    /// Every entry is normalized through the shared `normalize_repo_map_path`
    /// rule, so an empty string or `.` names the whole repository while a
    /// traversal path is dropped instead of escaping it.
    pub fn repo_maps(&self) -> Vec<String> {
        if let Some(entries) = self.field("repo_maps").and_then(|field| field.as_array()) {
            let maps: Vec<String> = entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .filter_map(normalize_repo_map_path)
                .collect();
            if !maps.is_empty() {
                return maps;
            }
        }
        super::xml::tags::parse_repo_map_tags(self.raw)
            .iter()
            .filter_map(|path| normalize_repo_map_path(path))
            .collect()
    }

    /// Reports whether the response requested the compact repository directory
    /// tree, falling back to `<repo_dirs>` control tags.
    pub fn repo_dirs(&self) -> bool {
        if let Some(requested) = self.field("repo_dirs").and_then(|field| field.as_bool()) {
            if requested {
                return true;
            }
        }
        super::xml::tags::parse_repo_dirs_tags(self.raw)
    }

    /// Directory content listing requests decoded from the `dir_lists` field,
    /// falling back to `<dir_list>` control tags.
    ///
    /// Every entry is normalized through [`DirListRequest::normalized`], so `.`
    /// and `./` collapse to the repository root while a path that escapes the
    /// repository is dropped instead of resolved. A missing `recursive` flag
    /// keeps the listing shallow, so a malformed entry narrows the result rather
    /// than failing it.
    pub fn dir_lists(&self) -> Vec<DirListRequest> {
        if let Some(entries) = self.field("dir_lists").and_then(|field| field.as_array()) {
            let requests: Vec<DirListRequest> = entries
                .iter()
                .filter_map(|entry| {
                    let path = entry.get("path").and_then(|field| field.as_str())?;
                    let recursive = entry
                        .get("recursive")
                        .and_then(|field| field.as_bool())
                        .unwrap_or(false);
                    DirListRequest::new(path, recursive).normalized()
                })
                .collect();
            if !requests.is_empty() {
                return requests;
            }
        }
        super::xml::tags::parse_dir_list_tags(self.raw)
    }

    /// Documentation topics decoded from the canonical `tauqe_doc_requests`
    /// field, falling back to the pre-canonical `doc_requests` spelling and then
    /// to the documentation control tags.
    ///
    /// The canonical field is the one the strict response schema advertises, and
    /// the pre-canonical spelling stays a permanent alias, so a cached response
    /// written before the rename keeps decoding.
    ///
    /// A `null` element is a partial emission of one topic, so it yields the
    /// tolerant `DocTopic::All` fallback the DTO boundary resolves it to, while
    /// an element of any other non-string shape is skipped.
    pub fn doc_requests(&self) -> Vec<DocTopic> {
        for spelling in ["tauqe_doc_requests", "doc_requests"] {
            let Some(entries) = self.field(spelling).and_then(|entries| entries.as_array()) else {
                continue;
            };
            let topics: Vec<DocTopic> = entries
                .iter()
                .filter_map(|entry| match entry {
                    // A `null` element resolves to the tolerant fallback at the DTO
                    // boundary, so the raw reader keeps the request the DTO would
                    // keep instead of dropping it.
                    serde_json::Value::Null => Some(DocTopic::All),
                    other => other.as_str().map(DocTopic::parse_lossy),
                })
                .collect();
            if !topics.is_empty() {
                return topics;
            }
        }
        super::xml::tags::parse_doc_request_tags(self.raw)
    }

    /// External document identifiers decoded from the `outer_doc_requests`
    /// field, falling back to `<outer_doc_request>` control tags.
    ///
    /// Each entry names one document the project declares in `[[outer_docs]]`
    /// rather than a cache path, so where the materialized copy lives stays a
    /// harness detail. A blank identifier grants nothing and is dropped, exactly
    /// as the tag form drops one, and repetition is preserved because
    /// materializing a declared document twice is decided by the phase that owns
    /// the round.
    pub fn outer_doc_requests(&self) -> Vec<String> {
        if let Some(entries) = self
            .field("outer_doc_requests")
            .and_then(|field| field.as_array())
        {
            let ids: Vec<String> = entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect();
            if !ids.is_empty() {
                return ids;
            }
        }
        let Some(spec) = engine::tag_spec("outer_doc_request") else {
            return Vec::new();
        };
        engine::attribute_values_with_fences(self.raw, spec, "id", &CodeFences::find(self.raw))
    }

    /// Ephemeral scratchpad notes decoded from the `scratchpad` field, falling
    /// back to `<scratchpad>` control tags.
    pub fn scratchpad(&self) -> Option<String> {
        if let Some(notes) = self
            .proposal
            .as_ref()
            .and_then(|proposal| proposal.scratchpad.as_deref())
        {
            let trimmed = notes.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        super::xml::tags::extract_scratchpad(self.raw)
    }

    /// Terminal outcome decoded from the `outcome` field, falling back to the
    /// terminal-outcome control tags.
    ///
    /// A `<blocked>` tag is itself a terminal outcome: a broken precondition ends
    /// the turn exactly like success or error, so the fallback surfaces it as
    /// [`TerminalOutcome::Blocked`].
    pub fn terminal_outcome(&self) -> Option<TerminalOutcome> {
        if let Some(outcome) = self
            .proposal
            .as_ref()
            .and_then(|proposal| proposal.outcome.as_ref())
        {
            let status = outcome.status.trim().to_ascii_lowercase();
            match status.as_str() {
                "success" => {
                    let commit_message = outcome
                        .commit_message
                        .as_deref()
                        .map(str::trim)
                        .filter(|commit_message| !commit_message.is_empty())
                        .map(str::to_string);
                    return Some(TerminalOutcome::Success { commit_message });
                }
                "error" => {
                    let message = outcome
                        .message
                        .as_deref()
                        .map(str::trim)
                        .filter(|message| !message.is_empty())
                        .unwrap_or("Execution error")
                        .to_string();
                    return Some(TerminalOutcome::Error { message });
                }
                "skip" | "skipped" => {
                    let reason = outcome
                        .reason
                        .as_deref()
                        .map(str::trim)
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or("Step skipped without a stated reason")
                        .to_string();
                    return Some(TerminalOutcome::Skip { reason });
                }
                _ => {}
            }
        }
        super::xml::tags::extract_terminal_outcome(self.raw)
    }

    /// Semantic blocker decoded from the `blocked` field, falling back to
    /// `<blocked>` control tags.
    pub fn blocked(&self) -> Option<(String, Option<String>)> {
        if let Some(blocked) = self
            .proposal
            .as_ref()
            .and_then(|proposal| proposal.blocked.as_ref())
        {
            if blocked.is_blocked {
                let reason = blocked.reason.trim();
                let reason = if reason.is_empty() {
                    "Execution blocked".to_string()
                } else {
                    reason.to_string()
                };
                let details = blocked
                    .details
                    .as_deref()
                    .map(str::trim)
                    .filter(|details| !details.is_empty())
                    .map(str::to_string);
                return Some((reason, details));
            }
        }
        super::xml::tags::extract_blocked(self.raw)
    }

    /// Semantic abort decoded from the `abort` field, falling back to `<abort>`
    /// control tags.
    ///
    /// An abort is not a blocker: it reports work the harness cannot perform at
    /// all, so the payload carries both the concrete missing capability (`reason`)
    /// and the exact command the developer must run (`recommendation`). A report
    /// that states neither keeps a deterministic default instead of stopping the
    /// turn without a cause.
    pub fn abort(&self) -> Option<(String, String)> {
        if let Some(abort) = self.field("abort").and_then(|field| field.as_object()) {
            let is_aborted = abort
                .get("is_aborted")
                .and_then(|field| field.as_bool())
                .unwrap_or(false);
            if is_aborted {
                let reason = abort
                    .get("reason")
                    .and_then(|field| field.as_str())
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("Execution aborted")
                    .to_string();
                let recommendation = abort
                    .get("recommendation")
                    .and_then(|field| field.as_str())
                    .map(str::trim)
                    .filter(|recommendation| !recommendation.is_empty())
                    .unwrap_or("No recommendation was stated.")
                    .to_string();
                return Some((reason, recommendation));
            }
        }
        abort_invocations(self.raw).into_iter().next()
    }

    /// Typed project command invocations decoded from the `custom_tools` field,
    /// falling back to `<custom_tool>` control tags.
    ///
    /// Each invocation names one command from the declared allowlist and carries
    /// the typed argument object the declaration validates later in the turn.
    /// Whether the name and its arguments are actually declared is decided by the
    /// harness at execution time, so decoding never rejects an invocation the
    /// project itself may accept.
    pub fn custom_tools(&self) -> Vec<(String, serde_json::Value)> {
        if let Some(entries) = self
            .field("custom_tools")
            .and_then(|field| field.as_array())
        {
            let invocations: Vec<(String, serde_json::Value)> = entries
                .iter()
                .filter_map(|entry| {
                    let name = entry
                        .get("name")
                        .and_then(|field| field.as_str())
                        .map(str::trim)
                        .filter(|name| !name.is_empty())?;
                    let args = entry
                        .get("args")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    Some((name.to_string(), args))
                })
                .collect();
            if !invocations.is_empty() {
                return invocations;
            }
        }
        custom_tool_invocations(self.raw)
    }

    /// Verification request decoded from the `verify` field, falling back to
    /// `<verify>` control tags.
    pub fn verify_request(&self) -> Option<VerifyRequest> {
        if let Some(verify) = self.field("verify").and_then(|field| field.as_object()) {
            let target = verify
                .get("target")
                .and_then(|field| field.as_str())
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if !target.is_empty() && target != "none" {
                let target = match target.as_str() {
                    "check" => VerifyTarget::Check,
                    "test" => VerifyTarget::Test,
                    "clippy" => VerifyTarget::Clippy,
                    _ => VerifyTarget::All,
                };
                // The declared success policy keeps its permissive default: any
                // spelling other than `silent` reports the outcome.
                let declared = verify
                    .get("on_success")
                    .and_then(|field| field.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                let on_success = if declared == "silent" {
                    VerifyOnSuccess::Silent
                } else {
                    VerifyOnSuccess::Report
                };
                return Some(VerifyRequest { target, on_success });
            }
        }
        super::xml::tags::extract_verify_request(self.raw)
    }

    /// Detected user language decoded from the `user_language` field, falling
    /// back to `<user_language>` control tags.
    pub fn user_language(&self) -> Option<String> {
        if let Some(language) = self.field("user_language").and_then(|field| field.as_str()) {
            let trimmed = language.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        super::xml::tags::extract_user_language(self.raw)
    }

    /// Plan creation or modification payloads decoded from the `plan_update`
    /// field, falling back to `<plan>` control tags.
    ///
    /// A `null` payload is the idle representation the strict schema declares for
    /// a turn that leaves the plan untouched, so it yields no update. A payload
    /// that cannot be deserialized into
    /// [`tauqe_protocol::DiscussionPlanUpdate`] is reported as an error instead of
    /// being swallowed: the caller turns it into a `ProtocolError` command, which
    /// lets the bounded turn repeat generation with the decoding diagnostics
    /// attached.
    pub fn plan_updates(&self) -> Result<Vec<tauqe_protocol::DiscussionPlanUpdate>, String> {
        let plan_update = self.field("plan_update");
        if let Some(plan_update) = plan_update.filter(|plan_update| !plan_update.is_null()) {
            let update = tauqe_protocol::DiscussionPlanUpdate::deserialize(plan_update)
                .map_err(|error| format!("invalid plan_update: {error}"))?;
            return Ok(vec![update]);
        }
        let (tags, diagnostics) = super::xml::tags::parse_plan_tags_with_diagnostics(self.raw);
        // A `<plan>` control tag is the second vehicle of the same mutation, so a
        // structurally malformed container is a present payload the reader
        // refuses: it is reported through the single error channel of this reader
        // instead of degrading into "no plan update", and the bounded turn turns
        // it into a `ProtocolError` command so a later round can repair it.
        if !diagnostics.is_empty() {
            return Err(format!(
                "invalid <plan> payload in the structured response: {}",
                diagnostics.join("; ")
            ));
        }
        Ok(tags.into_iter().map(plan_tag_to_update).collect())
    }

    /// Decision recorded for one review finding, decoded from the `review_update`
    /// field.
    ///
    /// The decision is a dialogue mutation rather than a code change: it states
    /// the status the engineer settled on for one finding together with the
    /// reasoning behind it. The field is declared nullable, so the idle form
    /// records no decision. A payload that cannot be deserialized into
    /// [`tauqe_protocol::review::ReviewUpdateProposal`] is reported as an error
    /// instead of being swallowed, so the bounded turn can repeat generation with
    /// the decoding diagnostics attached.
    pub fn review_update(
        &self,
    ) -> Result<Option<tauqe_protocol::review::ReviewUpdateProposal>, String> {
        let payload = self.field("review_update");
        let Some(payload) = payload.filter(|review_update| !review_update.is_null()) else {
            return Ok(None);
        };
        let decision = tauqe_protocol::review::ReviewUpdateProposal::deserialize(payload)
            .map_err(|error| format!("invalid review_update: {error}"))?;
        Ok(Some(decision))
    }
}

/// Resolves one structured change into a concrete, validated edit operation.
///
/// The target path is resolved against the permitted files, the payload is
/// normalized, and a move destination is resolved exactly like its source. Both
/// wire vehicles — the structured `changes` field and the anchored change-set
/// tag of the XML contour — travel through this routine, so a change is
/// validated identically whichever encoding carried it.
///
/// A directory move is the one entry that names no file of the context block: a
/// directory is never rendered as one, so its source is only normalized here
/// and the collisions of the subtree it fans out into are decided while the
/// change set is staged.
fn change_to_operation(
    change: &StructuredChangeProposal,
    editable_paths: &[String],
) -> Result<EditOperation, String> {
    let is_create = change.op == EditFileOp::Create;
    // Resolving a directory against the permitted files would let a suffix of
    // a context path stand in for the directory the model named, so the source
    // of a directory move is normalized instead of resolved.
    let moves_directory = change.op == EditFileOp::MoveDir;
    let target_path = if moves_directory {
        tauqe_protocol::edit::normalize_repo_path(&change.path)?
    } else {
        resolve_target_path_for_op(&change.path, editable_paths, is_create)?
    };

    if !moves_directory
        && !editable_paths.is_empty()
        && !is_create
        && !editable_paths.contains(&target_path)
    {
        return Err(format!(
            "File '{}' is not present in context. To edit it, request it first via context_requests. Permitted files in context: {}",
            target_path,
            editable_paths.join(", ")
        ));
    }

    let mut resolved = change.clone();
    resolved.path = target_path;
    resolved.old_text = change.old_text.as_deref().map(normalize_content);
    resolved.new_text = change.new_text.as_deref().map(normalize_content);
    resolved.content = change.content.as_deref().map(normalize_content);

    if resolved.op == EditFileOp::Move {
        let destination = change.to.as_deref().unwrap_or_default();
        resolved.to = Some(resolve_target_path_for_op(
            destination,
            editable_paths,
            true,
        )?);
    }

    resolved.to_edit_operation()
}

/// Decodes an anchored change set from its JSON array.
///
/// The anchored vehicle carries the very entries the structured contour declares
/// in `changes`, so one conversion and one set of path rules serve both
/// transports: the XML contour hands in the body of its change-set tag and the
/// structured contour hands in the `changes` field. Every entry is resolved and
/// validated like a structured change, so a file outside the permitted set or an
/// operation missing its payload is reported as a diagnostic instead of reaching
/// the staging engine.
pub fn parse_anchor_edits(
    body: &str,
    editable_paths: &[String],
) -> Result<Vec<EditOperation>, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let changes: Vec<StructuredChangeProposal> = serde_json::from_str(strip_code_fence(trimmed))
        .map_err(|error| format!("invalid anchored change set: {error}"))?;
    changes
        .iter()
        .map(|change| change_to_operation(change, editable_paths))
        .collect()
}

/// Converts a parsed `<plan>` tag into a normalized plan update.
fn plan_tag_to_update(
    tag: crate::edits::protocol::xml::tags::ParsedPlanTag,
) -> tauqe_protocol::DiscussionPlanUpdate {
    let action = match tag.action.as_str() {
        "save" => tauqe_protocol::DiscussionPlanAction::Save,
        "delete" => tauqe_protocol::DiscussionPlanAction::Delete,
        _ => tauqe_protocol::DiscussionPlanAction::Update,
    };
    tauqe_protocol::DiscussionPlanUpdate {
        action,
        id: Some(tag.id),
        title: tag.title,
        description: tag.description,
        concept: tag.concept,
        items: tag
            .items
            .into_iter()
            .map(crate::edits::protocol::xml::tags::ParsedPlanItem::into_plan_item)
            .collect(),
    }
}

/// Cleans raw text or JSON into conversational text without raw schema markup.
///
/// Every control tag is removed by the single parametric sanitizer, and a
/// payload that is itself one structured response collapses to its `message`
/// channel, so neither XML control tags nor JSON scaffolding can leak into
/// conversational text, session history, or the streamed interface.
pub fn clean_raw_json_or_text(raw: &str) -> String {
    StructuredPayload::parse(raw).clean_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::protocol::xml::tags::{VerifyOnSuccess, VerifyTarget};
    use tauqe_protocol::ContextAccess;

    /// Builds the render context a schema composition uses when the project
    /// declares no commands.
    fn render_context(patch_strategy: PatchStrategy) -> PromptRenderContext<'static> {
        PromptRenderContext::new(&[], None).with_patch_strategy(patch_strategy)
    }
    #[test]
    fn test_structured_protocol_repo_searches() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Let me search",
            "changes": [],
            "context_requests": [],
            "repo_searches": [{"path": "crates/core", "regex": "fn execute"}],
            "suggested_actions": []
        }"#;
        let searches = proto.parse_repo_searches(json);
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0].path, "crates/core");
        assert_eq!(searches[0].regex, "fn execute");
    }

    #[test]
    fn test_structured_protocol_repo_dirs_and_repo_maps() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Map the subsystem first",
            "changes": [],
            "repo_dirs": true,
            "repo_maps": [".", "./crates/core/", "../escape.rs", "crates/protocol/"]
        }"#;
        assert!(proto.parse_repo_dirs(json));
        // `.` names the whole repository, `./` prefixes collapse, and the
        // traversal entry is dropped instead of escaping the repository.
        assert_eq!(
            proto.parse_repo_maps(json),
            vec![
                String::new(),
                "crates/core".to_string(),
                "crates/protocol".to_string()
            ]
        );

        let idle = r#"{"message":"x","changes":[],"repo_dirs":false,"repo_maps":[]}"#;
        assert!(!proto.parse_repo_dirs(idle));
        assert!(proto.parse_repo_maps(idle).is_empty());
    }

    #[test]
    fn test_structured_protocol_repo_requests_fall_back_to_xml_tags() {
        let proto = StructuredEditProtocol;
        let raw = "{\"message\":\"Working\",\"changes\":[]}\n\
<repo_dirs />\n\
<repo_map path=\"crates/core\" />\n\
<repo_map />";
        assert!(proto.parse_repo_dirs(raw));
        assert_eq!(
            proto.parse_repo_maps(raw),
            vec!["crates/core".to_string(), String::new()]
        );
    }

    #[test]
    fn test_structured_protocol_dir_lists() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "List the subsystem",
            "changes": [],
            "dir_lists": [
                {"path": "./crates/core/", "recursive": false},
                {"path": ".", "recursive": true},
                {"path": "../escape", "recursive": true}
            ]
        }"#;
        // `./` collapses, `.` names the repository root, and the traversal
        // entry is dropped instead of escaping the repository.
        assert_eq!(
            proto.parse_dir_lists(json),
            vec![
                DirListRequest::new("crates/core", false),
                DirListRequest::new("", true),
            ]
        );

        let idle = r#"{"message":"x","changes":[],"dir_lists":[]}"#;
        assert!(proto.parse_dir_lists(idle).is_empty());
    }

    #[test]
    fn test_structured_protocol_dir_lists_fall_back_to_xml_tags() {
        let proto = StructuredEditProtocol;
        let raw = "{\"message\":\"Working\",\"changes\":[]}\n\
<dir_list path=\"crates/core\" />\n\
<dir_list path=\"crates/protocol\" recursive=\"true\" />";
        assert_eq!(
            proto.parse_dir_lists(raw),
            vec![
                DirListRequest::new("crates/core", false),
                DirListRequest::new("crates/protocol", true),
            ]
        );
    }

    #[test]
    fn test_structured_protocol_context_requests() {
        let proto = StructuredEditProtocol;
        let json = r#"{"message":"Need files","changes":[],"context_requests":["src/a.rs","editable:src/b.rs"],"suggested_actions":[]}"#;
        let reqs = proto.parse_context_requests(json);
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].access, ContextAccess::ReadOnly);
        assert_eq!(reqs[1].path, "src/b.rs");
        assert_eq!(reqs[1].access, ContextAccess::Editable);
    }

    #[test]
    fn test_structured_protocol_parse_replace() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let json = r#"{
            "message": "Update greeting function",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/lib.rs",
                    "old_text": "fn old() {}",
                    "new_text": "fn new() {}"
                }
            ],
            "context_requests": [],
            "suggested_actions": ["Run cargo test"]
        }"#;

        let res = proto.parse_output(json, &editable);
        match res {
            ModelResult::Edit {
                summary,
                edits,
                proposal,
                ..
            } => {
                assert_eq!(summary, "Update greeting function");
                assert_eq!(edits.len(), 1);
                assert!(proposal.is_some());
                match &edits[0] {
                    EditOperation::Replace {
                        path,
                        old_text,
                        new_text,
                    } => {
                        assert_eq!(path, "src/lib.rs");
                        assert_eq!(old_text, "fn old() {}");
                        assert_eq!(new_text, "fn new() {}");
                    }
                    _ => panic!("Expected Replace operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_utf8_long_summary_truncation() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        // A long Cyrillic string exceeding 100 characters where byte 97 falls inside a multibyte char
        let cyrillic_msg = "Я обновлю реализацию функции валидации пути и добавлю расширенную обработку возможных ошибок для обеспечения безопасности";
        let json = serde_json::json!({
            "message": cyrillic_msg,
            "changes": [
                {
                    "op": "replace",
                    "path": "src/lib.rs",
                    "old_text": "a",
                    "new_text": "b",
                    "content": null
                }
            ],
            "context_requests": [],
            "suggested_actions": []
        })
        .to_string();

        let res = proto.parse_output(&json, &editable);
        match res {
            ModelResult::Edit { summary, .. } => {
                assert!(summary.ends_with("..."));
                assert!(summary.chars().count() <= 100);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_schema_has_no_nullable_types() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        // Only the idle representation of the plan payload is nullable; every
        // edit field stays a concrete typed value.
        let changes = &schema["properties"]["changes"]["items"];
        assert!(!changes.to_string().contains("\"null\""));
        assert_eq!(changes["additionalProperties"], serde_json::json!(false));
        let required = changes["required"].as_array().unwrap();
        assert_eq!(required.len(), 6);
    }

    #[test]
    fn test_structured_schema_and_instructions_include_overwrite() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let ops = schema["properties"]["changes"]["items"]["properties"]["op"]["enum"]
            .as_array()
            .unwrap();
        assert!(ops.iter().any(|op| op == "overwrite"));

        let instr = StructuredEditProtocol.system_instructions(&[], PatchStrategy::SearchReplace);
        assert!(instr.contains("overwrite"));
    }

    #[test]
    fn test_structured_protocol_empty_string_fields() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Create file",
            "changes": [
                {"op": "create", "path": "src/new.rs", "old_text": "", "new_text": "", "content": "fn x() {}\n"}
            ],
            "context_requests": [],
            "suggested_actions": []
        }"#;
        match proto.parse_output(json, &editable) {
            ModelResult::Edit { edits, .. } => match &edits[0] {
                EditOperation::Create { path, content } => {
                    assert_eq!(path, "src/new.rs");
                    assert_eq!(content, "fn x() {}\n");
                }
                _ => panic!("Expected Create"),
            },
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_changes_before_message_with_wrapper_text() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let raw = "Here you go:\n{\"changes\":[{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"old_text\":\"a {\",\"new_text\":\"b {\",\"content\":\"\"}],\"message\":\"Swap a for b\",\"context_requests\":[],\"suggested_actions\":[]}\nThanks!";
        match proto.parse_output(raw, &editable) {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Swap a for b");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_parse_answer() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Hello there! How can I help you?",
            "changes": [],
            "context_requests": [],
            "suggested_actions": []
        }"#;

        let res = proto.parse_output(json, &editable);
        match res {
            ModelResult::Answer { text } => {
                assert_eq!(text, "Hello there! How can I help you?");
            }
            _ => panic!("Expected ModelResult::Answer"),
        }
    }

    #[test]
    fn test_structured_system_instructions_respect_the_capability_set() {
        let full = StructuredEditProtocol.system_instructions(&[], PatchStrategy::SearchReplace);
        assert!(full.contains("context_requests"));
        assert!(full.contains("overwrite"));

        let narrowed = StructuredEditProtocol.system_instructions_for(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            &[],
            PatchStrategy::SearchReplace,
        );
        assert!(narrowed.contains("changes"));
        assert!(!narrowed.contains("context_requests"));
        assert!(!narrowed.contains("repo_searches"));
    }

    #[test]
    fn test_structured_protocol_parses_an_anchored_change_set() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let json = r#"{
            "message": "Replace the guarded branch",
            "changes": [{
                "op": "replace",
                "path": "src/lib.rs",
                "to": "",
                "start": "Kf9Q",
                "end": "Tq2W",
                "content": "fn replacement() {}\n"
            }]
        }"#;

        match proto.parse_output(json, &editable) {
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(outcome.error.is_none(), "{:?}", outcome.error);
                match edits.as_slice() {
                    [EditOperation::LineAnchor {
                        path,
                        start,
                        end,
                        content,
                    }] => {
                        assert_eq!(path, "src/lib.rs");
                        assert_eq!(start, "Kf9Q");
                        assert_eq!(end, "Tq2W");
                        assert_eq!(content, "fn replacement() {}\n");
                    }
                    other => panic!("expected one anchored replacement, got {other:?}"),
                }
            }
            other => panic!("expected anchored edits, got {other:?}"),
        }
    }
    #[test]
    fn test_structured_protocol_parses_a_directory_move() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/old".to_string()];
        let json = r#"{
            "message": "Rename the module directory",
            "changes": [{
                "op": "move_dir",
                "path": "src/old",
                "to": "src/new"
            }]
        }"#;

        // A directory is never a file of the context block, so the move is
        // resolved as two directory paths instead of a context membership.
        match proto.parse_output(json, &editable) {
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(outcome.error.is_none(), "{:?}", outcome.error);
                assert_eq!(
                    edits,
                    vec![EditOperation::MoveDir {
                        from: "src/old".to_string(),
                        to: "src/new".to_string(),
                    }]
                );
            }
            other => panic!("expected a directory move, got {other:?}"),
        }
    }
    #[test]
    fn test_anchor_change_set_vehicle_decodes_shared_entries() {
        let editable = vec!["src/lib.rs".to_string()];
        let body = r#"[{"op":"replace","path":"src/lib.rs","to":"","start":"Kf9Q","end":"Kf9Q","content":"x\n"}]"#;
        let operations = parse_anchor_edits(body, &editable).expect("valid change set");
        assert!(matches!(
            operations.as_slice(),
            [EditOperation::LineAnchor { start, end, .. }] if start == "Kf9Q" && end == "Kf9Q"
        ));

        // A change that addresses no file of the context is reported instead of
        // reaching the staging engine.
        assert!(
            parse_anchor_edits(r#"[{"op":"replace","path":"../escape.rs"}]"#, &editable).is_err()
        );

        // An operation missing its payload, and a body that is not JSON at all,
        // both stay diagnostics.
        assert!(parse_anchor_edits(r#"[{"op":"create","path":"src/new.rs"}]"#, &editable).is_err());
        assert!(parse_anchor_edits("not json", &editable).is_err());

        // An empty vehicle requests no change at all.
        assert!(parse_anchor_edits("   ", &editable)
            .expect("an empty vehicle")
            .is_empty());
    }

    #[test]
    fn test_anchored_schema_declares_range_fields_instead_of_echoed_text() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::LineAnchor));
        let items = &schema["properties"]["changes"]["items"];
        let properties = items["properties"].as_object().expect("properties object");
        for field in ["op", "path", "to", "start", "end", "content"] {
            assert!(properties.contains_key(field), "missing {field}");
        }
        for field in ["old_text", "new_text"] {
            assert!(!properties.contains_key(field), "{field} must be absent");
        }
        assert_eq!(
            items["required"],
            serde_json::json!(["op", "path", "to", "start", "end", "content"])
        );
        assert_eq!(items["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_structured_protocol_system_instructions() {
        let proto = StructuredEditProtocol;
        let paths = vec!["src/lib.rs".to_string()];

        let instr = proto.system_instructions(&paths, PatchStrategy::SearchReplace);
        assert!(!instr.contains("context_drops"));
        assert!(instr.contains("context_requests"));
    }

    #[test]
    fn test_structured_protocol_multibyte_utf8_slicing_safety() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];

        // Surrounding text contains multi-byte Cyrillic, emoji, and escaped characters
        // directly adjacent to the JSON structure boundaries.
        let raw = "Привет, мир! 🚀 Тестируем UTF-8 границы: {\"message\":\"Всё отлично! ✨\",\"changes\":[{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"old_text\":\"старый\",\"new_text\":\"новый\",\"to\":\"\",\"content\":\"\"}],\"context_requests\":[],\"suggested_actions\":[]} Конец сообщения. 🦀";

        let res = proto.parse_output(raw, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Всё отлично! ✨");
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace {
                        old_text, new_text, ..
                    } => {
                        assert_eq!(old_text, "старый");
                        assert_eq!(new_text, "новый");
                    }
                    _ => panic!("Expected Replace operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_clean_collapses_a_silent_structured_round() {
        // A structured response whose only conversational channel is empty has
        // no prose to recover. Its JSON scaffolding is protocol chrome, never
        // an answer: collapsing it keeps raw wire markup out of session history
        // and matches what the streaming filter showed the interface (nothing).
        let silent = r#"{
            "message": "",
            "changes": [],
            "context_requests": ["src/a.rs"],
            "verify": {"target": "none", "on_success": "silent"},
            "outcome": {"status": "none", "summary": "", "reason": "", "message": ""}
        }"#;
        assert_eq!(clean_raw_json_or_text(silent), "");

        // A fenced payload is the same protocol response with decoration.
        let fenced = "```json\n{\"message\":\"\",\"changes\":[]}\n```";
        assert_eq!(clean_raw_json_or_text(fenced), "");
    }

    #[test]
    fn test_clean_keeps_prose_that_quotes_a_json_object() {
        // Prose that merely quotes a JSON object is conversational text: the
        // object does not span the whole payload, so the structured collapse
        // never fires and the explanation survives verbatim.
        let prose = "A silent round is {\"message\":\"\",\"changes\":[]} in the journal.";
        assert_eq!(clean_raw_json_or_text(prose), prose);
    }

    #[test]
    fn test_clean_joins_the_channels_of_concatenated_responses() {
        // A turn concatenates the rounds it received, so one payload may carry
        // several responses. Their conversational channels are joined in
        // document order instead of only the first one surviving cleaning.
        let rounds = concat!(
            "{\"message\":\"Need file a.rs\",\"changes\":[]}\n\n",
            "{\"message\":\"Updated file a.rs\",\"changes\":[]}",
        );
        assert_eq!(
            clean_raw_json_or_text(rounds),
            "Need file a.rs\n\nUpdated file a.rs"
        );

        // A silent round contributes no channel of its own and no separator.
        let silent_then_answer = concat!(
            "{\"message\":\"\",\"changes\":[]}\n\n",
            "{\"message\":\"Updated file a.rs\",\"changes\":[]}",
        );
        assert_eq!(
            clean_raw_json_or_text(silent_then_answer),
            "Updated file a.rs"
        );
    }

    #[test]
    fn test_structured_schema_declares_control_fields() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        for field in [
            "tauqe_doc_requests",
            "scratchpad",
            "outcome",
            "blocked",
            "abort",
        ] {
            assert!(
                required.contains(&field),
                "field '{field}' must be required in the schema"
            );
            assert!(
                schema["properties"].get(field).is_some(),
                "field '{field}' must be declared in properties"
            );
        }

        let statuses = schema["properties"]["outcome"]["properties"]["status"]["enum"]
            .as_array()
            .unwrap();
        for status in ["none", "success", "error"] {
            assert!(statuses.iter().any(|value| value == status));
        }
        assert_eq!(
            schema["properties"]["blocked"]["properties"]["is_blocked"]["type"],
            "boolean"
        );
        assert_eq!(
            schema["properties"]["tauqe_doc_requests"]["items"]["type"],
            "string"
        );
        // The strict control objects carry no nullable type; the plan payload
        // is the only property whose idle form is `null`.
        assert!(!schema["properties"]["blocked"]
            .to_string()
            .contains("\"null\""));
    }

    #[test]
    fn test_structured_system_instructions_include_control_fields() {
        let capabilities = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Research,
            LlmCapability::DocRequest,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
        ]);
        let instructions = StructuredEditProtocol.system_instructions_for(
            &capabilities,
            &[],
            PatchStrategy::SearchReplace,
        );
        for field in ["tauqe_doc_requests", "scratchpad", "outcome", "blocked"] {
            assert!(
                instructions.contains(field),
                "instructions must mention '{field}'"
            );
        }
    }

    #[test]
    fn test_structured_protocol_parse_doc_requests_and_scratchpad() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Need docs",
            "changes": [],
            "tauqe_doc_requests": ["workflow", "git"],
            "scratchpad": "Verified: parser fixed."
        }"#;
        assert_eq!(
            proto.parse_doc_requests(json),
            vec![DocTopic::Workflow, DocTopic::Git]
        );
        assert_eq!(
            proto.parse_scratchpad(json).as_deref(),
            Some("Verified: parser fixed.")
        );
        // The pre-canonical spelling stays a permanent alias, so a cached
        // response decodes into the very same topics.
        let aliased = json.replace("tauqe_doc_requests", "doc_requests");
        assert_eq!(
            proto.parse_doc_requests(&aliased),
            proto.parse_doc_requests(json)
        );

        let empty = r#"{"message":"Nothing","changes":[],"tauqe_doc_requests":[],"scratchpad":""}"#;
        assert!(proto.parse_doc_requests(empty).is_empty());

        // A `null` element is a partial emission of one topic, so the raw reader
        // resolves it to the same tolerant fallback the DTO boundary resolves it
        // to, while an element of any other non-string shape is still skipped.
        let partial = r#"{
            "message": "Need docs",
            "changes": [],
            "tauqe_doc_requests": ["git", null, 7, "workflow"]
        }"#;
        assert_eq!(
            proto.parse_doc_requests(partial),
            vec![DocTopic::Git, DocTopic::All, DocTopic::Workflow]
        );

        // A list of nothing but non-string junk still requests no documentation,
        // so the reader keeps falling through to its control-tag vehicle.
        let junk = r#"{"message":"x","changes":[],"tauqe_doc_requests":[7,false]}"#;
        assert!(proto.parse_doc_requests(junk).is_empty());
        assert_eq!(proto.parse_scratchpad(empty), None);
    }

    #[test]
    fn test_structured_protocol_parse_outer_doc_requests() {
        let proto = StructuredEditProtocol;

        // Each entry names the mount point of one project declaration, and a
        // blank identifier grants nothing.
        let json = r#"{
            "message": "Checking the upstream contract",
            "changes": [],
            "outer_doc_requests": [" vendor/api.md ", "   ", "mylib"]
        }"#;
        assert_eq!(
            proto.parse_outer_doc_requests(json),
            vec!["vendor/api.md".to_string(), "mylib".to_string()]
        );

        // An idle array requests no document at all.
        let idle = r#"{"message":"x","changes":[],"outer_doc_requests":[]}"#;
        assert!(proto.parse_outer_doc_requests(idle).is_empty());

        // Control-tag fallback keeps both transports decoding one payload.
        let fallback = concat!(
            "{\"message\":\"Working\",\"changes\":[]}\n",
            "<outer_doc_request id=\"mylib\" />",
        );
        assert_eq!(
            proto.parse_outer_doc_requests(fallback),
            vec!["mylib".to_string()]
        );
    }

    #[test]
    fn test_structured_protocol_parse_terminal_outcome() {
        let proto = StructuredEditProtocol;

        let success = r#"{
            "message": "Done",
            "changes": [],
            "outcome": {"status": "success", "commit_message": "All checks passed", "message": ""}
        }"#;
        assert_eq!(
            proto.parse_terminal_outcome(success),
            Some(TerminalOutcome::Success {
                commit_message: Some("All checks passed".to_string())
            })
        );

        let error = r#"{
            "message": "Failed",
            "changes": [],
            "outcome": {"status": "error", "summary": "", "message": "Missing table"}
        }"#;
        assert_eq!(
            proto.parse_terminal_outcome(error),
            Some(TerminalOutcome::Error {
                message: "Missing table".to_string()
            })
        );

        let ongoing = r#"{
            "message": "Working",
            "changes": [],
            "outcome": {"status": "none", "summary": "", "message": ""}
        }"#;
        assert_eq!(proto.parse_terminal_outcome(ongoing), None);
    }

    #[test]
    fn test_structured_protocol_parse_blocked() {
        let proto = StructuredEditProtocol;

        let blocked = r#"{
            "message": "Cannot proceed",
            "changes": [],
            "blocked": {"is_blocked": true, "reason": "Missing table", "details": "users table absent"}
        }"#;
        assert_eq!(
            proto.parse_blocked(blocked),
            Some((
                "Missing table".to_string(),
                Some("users table absent".to_string())
            ))
        );

        let clear = r#"{
            "message": "Fine",
            "changes": [],
            "blocked": {"is_blocked": false, "reason": "", "details": ""}
        }"#;
        assert_eq!(proto.parse_blocked(clear), None);
    }

    #[test]
    fn test_structured_protocol_parse_abort() {
        let proto = StructuredEditProtocol;

        let abort = r#"{
            "message": "Cannot continue",
            "changes": [],
            "abort": {
                "is_aborted": true,
                "reason": "Native build approval required",
                "recommendation": "pnpm approve-builds"
            }
        }"#;
        assert_eq!(
            proto.parse_abort(abort),
            Some((
                "Native build approval required".to_string(),
                "pnpm approve-builds".to_string()
            ))
        );

        let clear = r#"{
            "message": "Fine",
            "changes": [],
            "abort": {"is_aborted": false, "reason": "", "recommendation": ""}
        }"#;
        assert_eq!(proto.parse_abort(clear), None);

        // A report that states no reason still stops the turn, with a
        // deterministic payload instead of an empty one.
        let bare = r#"{
            "message": "x",
            "changes": [],
            "abort": {"is_aborted": true, "reason": "  ", "recommendation": ""}
        }"#;
        assert_eq!(
            proto.parse_abort(bare),
            Some((
                "Execution aborted".to_string(),
                "No recommendation was stated.".to_string()
            ))
        );

        // Control-tag fallback keeps both transports decoding one payload.
        let xml = "{\"message\":\"Working\",\"changes\":[]}\n\
<abort reason=\"Toolchain missing\">Install the SDK, then rerun the step.</abort>";
        assert_eq!(
            proto.parse_abort(xml),
            Some((
                "Toolchain missing".to_string(),
                "Install the SDK, then rerun the step.".to_string()
            ))
        );
        assert_eq!(
            proto.parse_abort("{\"message\":\"x\",\"changes\":[]}"),
            None
        );
    }

    #[test]
    fn test_structured_protocol_parse_custom_tools() {
        let proto = StructuredEditProtocol;

        let json = r#"{
            "message": "Running the declared commands",
            "changes": [],
            "custom_tools": [
                {"name": "probe", "args": {"n": "10"}},
                {"name": "check", "args": {}}
            ]
        }"#;
        assert_eq!(
            proto.parse_custom_tools(json),
            vec![
                ("probe".to_string(), serde_json::json!({ "n": "10" })),
                ("check".to_string(), serde_json::json!({})),
            ]
        );

        // An idle invocation array requests no execution.
        let idle = r#"{"message":"x","changes":[],"custom_tools":[]}"#;
        assert!(proto.parse_custom_tools(idle).is_empty());

        // Control-tag fallback decodes the same invocation shape.
        let xml = "{\"message\":\"Working\",\"changes\":[]}\n\
<custom_tool name=\"probe\" n=\"10\" />";
        assert_eq!(
            proto.parse_custom_tools(xml),
            vec![("probe".to_string(), serde_json::json!({ "n": "10" }))]
        );
    }

    #[test]
    fn test_structured_protocol_parse_skip_outcome() {
        let proto = StructuredEditProtocol;

        let skip = r#"{
            "message": "Already satisfied",
            "changes": [],
            "outcome": {
                "status": "skip",
                "commit_message": "",
                "message": "",
                "reason": "Step already implemented"
            }
        }"#;
        assert_eq!(
            proto.parse_terminal_outcome(skip),
            Some(TerminalOutcome::Skip {
                reason: "Step already implemented".to_string()
            })
        );

        // The `skipped` alias and an empty reason both fall back to a
        // deterministic reason instead of dropping the outcome.
        let skipped_alias = r#"{
            "message": "Already satisfied",
            "changes": [],
            "outcome": {"status": "skipped", "commit_message": "", "message": "", "reason": "  "}
        }"#;
        assert_eq!(
            proto.parse_terminal_outcome(skipped_alias),
            Some(TerminalOutcome::Skip {
                reason: "Step skipped without a stated reason".to_string()
            })
        );

        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let outcome = &schema["properties"]["outcome"];
        let statuses = outcome["properties"]["status"]["enum"]
            .as_array()
            .expect("status enum");
        for status in ["none", "success", "error", "skip"] {
            assert!(
                statuses.iter().any(|value| value == status),
                "the outcome status enum must declare '{status}'"
            );
        }

        let required: Vec<&str> = outcome["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(
            required.len(),
            outcome["properties"]
                .as_object()
                .expect("properties object")
                .len(),
            "a strict outcome object must require every declared property"
        );
        assert!(required.contains(&"reason"));
        assert_eq!(outcome["properties"]["reason"]["type"], "string");
    }

    #[test]
    fn test_structured_protocol_control_fields_fall_back_to_xml_tags() {
        let proto = StructuredEditProtocol;
        let raw = "{\"message\":\"Working\",\"changes\":[]}\n\
<doc_request topic=\"git\" />\n\
<scratchpad>Inspect parser</scratchpad>\n\
<blocked reason=\"Missing table\" details=\"users table absent\" />";

        assert_eq!(proto.parse_doc_requests(raw), vec![DocTopic::Git]);
        // The canonical spelling of the same channel decodes into the very same
        // request, so a transcript written before the rename keeps working.
        let canonical = raw.replace("doc_request", "tauqe_doc_request");
        assert_eq!(proto.parse_doc_requests(&canonical), vec![DocTopic::Git]);
        assert_eq!(
            proto.parse_scratchpad(raw).as_deref(),
            Some("Inspect parser")
        );
        assert_eq!(
            proto.parse_blocked(raw),
            Some((
                "Missing table".to_string(),
                Some("users table absent".to_string())
            ))
        );
        // A `<blocked>` tag is itself a terminal outcome, so the XML fallback
        // reports the extracted blocker instead of yielding no outcome at all.
        assert_eq!(
            proto.parse_terminal_outcome(raw),
            Some(TerminalOutcome::Blocked {
                reason: "Missing table".to_string(),
                details: Some("users table absent".to_string())
            })
        );
    }

    #[test]
    fn test_structured_protocol_extract_verify_request_without_xml_edits() {
        let proto = StructuredEditProtocol;
        let raw = "{\"message\":\"Done\",\"changes\":[],\"context_requests\":[],\"suggested_actions\":[]}\n<verify target=\"test\" on_success=\"report\" />";
        let req = proto.parse_verify_request(raw);
        assert!(req.is_some());
        let req = req.unwrap();
        assert_eq!(req.target, VerifyTarget::Test);
        assert_eq!(req.on_success, VerifyOnSuccess::Report);
    }

    #[test]
    fn test_structured_protocol_parse_verify_request_from_json_field() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Run tests",
            "changes": [],
            "verify": {"target": "test", "on_success": "silent"}
        }"#;
        let req = proto.parse_verify_request(json).expect("verify request");
        assert_eq!(req.target, VerifyTarget::Test);
        assert_eq!(req.on_success, VerifyOnSuccess::Silent);

        let none =
            r#"{"message":"x","changes":[],"verify":{"target":"none","on_success":"silent"}}"#;
        assert!(proto.parse_verify_request(none).is_none());
    }

    #[test]
    fn test_structured_protocol_parse_user_language_from_json_field() {
        let proto = StructuredEditProtocol;
        let json = r#"{"message":"x","changes":[],"user_language":"Russian"}"#;
        assert_eq!(proto.parse_user_language(json).as_deref(), Some("Russian"));
        let empty = r#"{"message":"x","changes":[],"user_language":""}"#;
        assert_eq!(proto.parse_user_language(empty), None);
    }

    #[test]
    fn test_structured_protocol_parse_plan_updates_from_json_and_xml() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Refine",
            "changes": [],
            "plan_update": {"action": "update", "id": "p-1", "title": "Refined"}
        }"#;
        let updates = proto.parse_plan_updates(json).expect("plan update");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id.as_deref(), Some("p-1"));
        assert_eq!(
            updates[0].action,
            tauqe_protocol::DiscussionPlanAction::Update
        );

        let xml = "Working\n<plan action=\"save\" id=\"p-2\" title=\"P2\"><concept><concept_item id=\"c-1\" title=\"C\" /></concept><item id=\"1\" title=\"Step\" /></plan>";
        let updates = proto.parse_plan_updates(xml).expect("plan tag");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id.as_deref(), Some("p-2"));
        assert_eq!(
            updates[0].action,
            tauqe_protocol::DiscussionPlanAction::Save
        );
    }

    #[test]
    fn test_structured_protocol_reports_a_malformed_plan_tag() {
        let proto = StructuredEditProtocol;

        // A `<plan>` container that fails structural validation is a present
        // payload the reader refused, never a plan that silently disappeared.
        let malformed = "Refining the plan.\n<plan action=\"save\" id=\"p-1\" title=\"P1\"></plan>";
        let error = proto
            .parse_plan_updates(malformed)
            .expect_err("a malformed <plan> container must be reported");
        assert!(error.contains("<plan>"), "{error}");
        assert!(error.contains("<concept>"), "{error}");

        // The very same container decodes once the concept tree is present, so
        // the strictness belongs to the payload rather than to the vehicle.
        let valid = "Refining the plan.\n<plan action=\"save\" id=\"p-1\" title=\"P1\"><concept><concept_item id=\"c-1\" title=\"C\" /></concept></plan>";
        let updates = proto.parse_plan_updates(valid).expect("a valid container");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id.as_deref(), Some("p-1"));
    }

    #[test]
    fn test_structured_protocol_plan_update_failure_is_reported() {
        let proto = StructuredEditProtocol;

        // A declared `plan_update` whose payload the DTO cannot hold must
        // surface as an error instead of being silently dropped, so the bounded
        // turn can repeat generation with the diagnostics attached.
        let malformed = r#"{
            "message": "Refine",
            "changes": [],
            "plan_update": {"action": "save", "items": "not-a-list"}
        }"#;
        let error = proto
            .parse_plan_updates(malformed)
            .expect_err("an undecodable plan_update must fail");
        assert!(error.contains("plan_update"), "{error}");

        // A control tag never rescues a malformed structured payload: a silent
        // fallback would hide the defect the retry has to repair.
        let rescuing = r#"{
            "message": "Refine",
            "changes": [],
            "plan_update": {"items": "not-a-list"}
        }
<plan action="save" id="p-1" title="P1" />"#;
        assert!(proto.parse_plan_updates(rescuing).is_err());

        // `null` is the idle representation of the field the strict schema
        // requires, so it stays a normal "no plan update" answer.
        let idle = r#"{"message":"x","changes":[],"plan_update":null}"#;
        assert_eq!(proto.parse_plan_updates(idle), Ok(Vec::new()));
    }

    #[test]
    fn test_structured_schema_declares_verify_and_user_language() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert!(required.contains(&"verify"));
        assert!(required.contains(&"user_language"));

        let targets = schema["properties"]["verify"]["properties"]["target"]["enum"]
            .as_array()
            .unwrap();
        for target in ["none", "all", "check", "test", "clippy"] {
            assert!(targets.iter().any(|value| value == target));
        }
        assert_eq!(schema["properties"]["user_language"]["type"], "string");
        assert!(!schema["properties"]["verify"]
            .to_string()
            .contains("\"null\""));
    }

    #[test]
    fn test_structured_schema_reflects_the_capability_set() {
        let full = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let full_properties = full["properties"].as_object().expect("properties object");
        for field in [
            "message",
            "changes",
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "scratchpad",
            "outcome",
            "blocked",
            "abort",
            "verify",
            "user_language",
        ] {
            assert!(
                full_properties.contains_key(field),
                "the development schema must declare '{field}'"
            );
        }

        // `suggested_actions` belongs to no capability, so the registry never
        // emits it: the schema advertises only the enabled protocol surface.
        assert!(!full_properties.contains_key("suggested_actions"));
        assert_eq!(
            full["required"].as_array().map(|required| required.len()),
            Some(full_properties.len()),
            "a strict schema must require every declared property"
        );

        let narrowed = StructuredEditProtocol::schema_for(
            &CapabilitySet::from_slice(&[LlmCapability::TextMessage, LlmCapability::FileEdits]),
            &render_context(PatchStrategy::SearchReplace),
        );
        let properties = narrowed["properties"]
            .as_object()
            .expect("properties object");
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("changes"));
        for field in [
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "scratchpad",
            "outcome",
            "blocked",
            "abort",
            "verify",
            "user_language",
            "plan_update",
            "custom_tools",
        ] {
            assert!(
                !properties.contains_key(field),
                "a disabled capability must not advertise '{field}'"
            );
        }

        let required: Vec<&str> = narrowed["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(
            required.len(),
            properties.len(),
            "pruning a property must also prune its required entry"
        );
        assert_eq!(narrowed["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_structured_response_format_follows_the_capability_set() {
        let narrowed = CapabilitySet::from_slice(&[LlmCapability::TextMessage]);
        match StructuredEditProtocol
            .response_format_for(&narrowed, &render_context(PatchStrategy::SearchReplace))
        {
            Some(ResponseFormat::JsonSchema { json_schema, .. }) => {
                let properties = json_schema.schema["properties"]
                    .as_object()
                    .expect("properties object");
                assert!(properties.contains_key("message"));
                assert!(!properties.contains_key("changes"));
                assert!(!properties.contains_key("verify"));
            }
            other => panic!("expected a JSON schema response format, got {other:?}"),
        }
    }
    #[test]
    fn test_structured_protocol_parse_review_update() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Decision recorded",
            "review_update": {
                "item_id": 7,
                "status": "skipped",
                "note": "  Not reproducible  ",
                "recommendations": ["Cover the retry path"]
            }
        }"#;
        let decision = proto
            .parse_review_update(json)
            .expect("a decodable payload")
            .expect("a recorded decision");
        assert_eq!(decision.item_id, 7);
        assert_eq!(
            decision.status,
            tauqe_protocol::review::ReviewDecisionStatus::Skipped
        );
        assert_eq!(decision.note, "  Not reproducible  ");
        assert_eq!(
            decision.recommendations,
            vec!["Cover the retry path".to_string()]
        );

        // The field is nullable and optional, so both idle forms record nothing.
        assert_eq!(
            proto.parse_review_update(r#"{"message":"x","review_update":null}"#),
            Ok(None)
        );
        assert_eq!(proto.parse_review_update(r#"{"message":"x"}"#), Ok(None));

        // A payload the DTO cannot hold is reported instead of being swallowed.
        let malformed =
            proto.parse_review_update(r#"{"message":"x","review_update":{"item_id":"seven"}}"#);
        assert!(malformed.is_err(), "{malformed:?}");
    }
    #[test]
    fn test_review_update_field_follows_the_review_discussion_surface() {
        let review = StructuredEditProtocol::schema_for(
            &CapabilitySet::review_discussion(),
            &render_context(PatchStrategy::SearchReplace),
        );
        assert!(
            review["properties"].get("review_update").is_some(),
            "a review discussion declares the decision field"
        );
        let required: Vec<&str> = review["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert!(required.contains(&"review_update"));

        // Every other dialogue contour leaves the field out, so a decision can
        // never be recorded outside the regime that authorizes it.
        for surface in [
            CapabilitySet::interactive_repl(),
            CapabilitySet::discussion(),
            CapabilitySet::isolated_step(),
        ] {
            let schema = StructuredEditProtocol::schema_for(
                &surface,
                &render_context(PatchStrategy::SearchReplace),
            );
            assert!(
                schema["properties"].get("review_update").is_none(),
                "the decision field leaked into a surface that does not authorize it"
            );
        }
    }
}
